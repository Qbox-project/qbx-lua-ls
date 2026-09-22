use std::collections::HashMap;

use lsp_types::{
    DocumentHighlight, DocumentHighlightKind, Location, Position, PrepareRenameResponse, TextEdit, Url, WorkspaceEdit,
};
use qbx_lua_analysis::project::read_source;
use qbx_lua_analysis::scope::{resolve, GlobalRefKind, Resolved};
use qbx_lua_syntax::{parse, LineIndex, Span};

use super::member_refs::{in_document, is_renamable, member_occurrences, member_target};
use crate::document::Document;
use crate::index::FileOrigin;
use crate::indexer::span_to_range;
use crate::server::Documents;
use crate::workspace::Workspace;

struct Occurrence {
    span: Span,
    write: bool,
}

fn local_occurrences(doc: &Document, offset: u32) -> Option<Vec<Occurrence>> {
    let (Resolved::Local(id), _) = doc.resolution.resolved_at_offset(offset)? else { return None };
    let local = doc.resolution.local(id);
    let mut out = Vec::new();
    if !local.decl.is_empty() {
        out.push(Occurrence { span: local.decl, write: true });
    }
    out.extend(local.refs.iter().map(|r| Occurrence { span: r.span, write: r.write }));
    Some(out)
}

fn global_name_at(doc: &Document, offset: u32) -> Option<&str> {
    match doc.resolution.resolved_at_offset(offset)? {
        (Resolved::Global(index), _) => Some(doc.resolution.globals[index as usize].name.as_str()),
        _ => None,
    }
}

/// Every occurrence of a global across the files that can see it. Closed files are parsed on demand
/// instead of keeping their reference lists in memory.
fn global_occurrences(
    ws: &Workspace,
    docs: &Documents,
    doc: &Document,
    name: &str,
) -> Vec<(Url, lsp_types::Range, bool)> {
    let mut out = Vec::new();
    for (id, entry) in ws.index.files() {
        if entry.origin == FileOrigin::Stub || !ws.index.is_related(doc.file, id) {
            continue;
        }
        let mentions = |text: &str| text.contains(name);
        if let Some(open) = docs.get(&entry.uri) {
            for global in open.resolution.globals.iter().filter(|g| g.name == name) {
                out.push((entry.uri.clone(), open.range(global.span), global.kind != GlobalRefKind::Read));
            }
            continue;
        }
        let Ok(source) = read_source(&entry.path) else { continue };
        if !mentions(&source) {
            continue;
        }
        let resolution = resolve(&parse(&source));
        let lines = LineIndex::new(&source);
        for global in resolution.globals.iter().filter(|g| g.name == name) {
            out.push((
                entry.uri.clone(),
                span_to_range(&source, &lines, global.span),
                global.kind != GlobalRefKind::Read,
            ));
        }
    }
    out
}

pub fn references(
    ws: &Workspace,
    docs: &Documents,
    doc: &Document,
    position: Position,
    include_declaration: bool,
) -> Vec<Location> {
    let offset = doc.offset(position);
    if let Some(occurrences) = local_occurrences(doc, offset) {
        let decl = doc.resolution.resolved_at_offset(offset).and_then(|(r, _)| match r {
            Resolved::Local(id) => Some(doc.resolution.local(id).decl),
            Resolved::Global(_) => None,
        });
        return occurrences
            .into_iter()
            .filter(|o| include_declaration || Some(o.span) != decl)
            .map(|o| Location::new(doc.uri.clone(), doc.range(o.span)))
            .collect();
    }
    let Some(name) = global_name_at(doc, offset) else {
        let Some(target) = member_target(ws, doc, offset) else { return Vec::new() };
        return member_occurrences(ws, docs, doc, &target)
            .unwrap_or_default()
            .into_iter()
            .map(|(uri, range)| Location::new(uri, range))
            .collect();
    };
    global_occurrences(ws, docs, doc, name)
        .into_iter()
        .filter(|(_, _, is_definition)| include_declaration || !is_definition)
        .map(|(uri, range, _)| Location::new(uri, range))
        .collect()
}

pub fn highlights(ws: &Workspace, doc: &Document, position: Position) -> Vec<DocumentHighlight> {
    let offset = doc.offset(position);
    let kind = |write: bool| Some(if write { DocumentHighlightKind::WRITE } else { DocumentHighlightKind::READ });
    if let Some(occurrences) = local_occurrences(doc, offset) {
        return occurrences
            .into_iter()
            .map(|o| DocumentHighlight { range: doc.range(o.span), kind: kind(o.write) })
            .collect();
    }
    let Some(name) = global_name_at(doc, offset) else {
        let Some(target) = member_target(ws, doc, offset) else { return Vec::new() };
        return in_document(ws, doc, &target)
            .into_iter()
            .map(|range| DocumentHighlight { range, kind: Some(DocumentHighlightKind::TEXT) })
            .collect();
    };
    doc.resolution
        .globals
        .iter()
        .filter(|g| g.name == name)
        .map(|g| DocumentHighlight { range: doc.range(g.span), kind: kind(g.kind != GlobalRefKind::Read) })
        .collect()
}

pub fn prepare_rename(ws: &Workspace, doc: &Document, position: Position) -> Option<PrepareRenameResponse> {
    let offset = doc.offset(position);
    let Some((resolved, span)) = doc.resolution.resolved_at_offset(offset) else {
        let target = member_target(ws, doc, offset).filter(|t| is_renamable(ws, t))?;
        let here = in_document(ws, doc, &target).into_iter().find(|r| r.start <= position && position <= r.end)?;
        return Some(PrepareRenameResponse::Range(here));
    };
    if span.is_empty() {
        return None;
    }
    if let Resolved::Global(index) = resolved {
        let name = &doc.resolution.globals[index as usize].name;
        let is_runtime =
            qbx_lua_analysis::env::builtins().get(name).is_some() || qbx_fivem_data::native(name).is_some();
        if is_runtime {
            return None;
        }
    }
    Some(PrepareRenameResponse::Range(doc.range(span)))
}

pub fn rename(
    ws: &Workspace,
    docs: &Documents,
    doc: &Document,
    position: Position,
    new_name: &str,
) -> Option<WorkspaceEdit> {
    let valid = new_name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && new_name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && qbx_lua_syntax::lexer::keyword(new_name).is_none();
    if !valid {
        return None;
    }
    let offset = doc.offset(position);
    let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();
    if let Some(occurrences) = local_occurrences(doc, offset) {
        let edits = occurrences.into_iter().map(|o| TextEdit::new(doc.range(o.span), new_name.to_string())).collect();
        changes.insert(doc.uri.clone(), edits);
    } else if let Some(name) = global_name_at(doc, offset) {
        for (uri, range, _) in global_occurrences(ws, docs, doc, name) {
            changes.entry(uri).or_default().push(TextEdit::new(range, new_name.to_string()));
        }
    } else {
        let target = member_target(ws, doc, offset).filter(|t| is_renamable(ws, t))?;
        for (uri, range) in member_occurrences(ws, docs, doc, &target)? {
            changes.entry(uri).or_default().push(TextEdit::new(range, new_name.to_string()));
        }
    }
    Some(WorkspaceEdit { changes: Some(changes), ..WorkspaceEdit::default() })
}
