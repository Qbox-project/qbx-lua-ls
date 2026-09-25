use lsp_types::{GotoDefinitionResponse, Location, Position, Range};
use qbx_lua_syntax::ast::ExprKind;

use super::hover::{target_at, Target};
use super::with_infer;
use crate::document::Document;
use crate::index::{EventKind, FileId, FileOrigin};
use crate::locate::locate;
use crate::workspace::Workspace;

const REQUIRE_CALLS: &[&str] = &["require", "lib.require", "lib.load"];

fn location(ws: &Workspace, file: FileId, range: Range) -> Option<Location> {
    let entry = ws.index.file(file).filter(|f| f.origin != FileOrigin::Stub)?;
    Some(Location::new(entry.uri.clone(), range))
}

fn string_definition(ws: &Workspace, doc: &Document, offset: u32) -> Vec<Location> {
    let located = locate(&doc.chunk, offset);
    let Some((string, call)) = located.string else { return Vec::new() };
    let ExprKind::String(value) = &string.kind else { return Vec::new() };

    let callee = call.and_then(|(call, _)| match &call.kind {
        ExprKind::Call { callee, .. } => callee.dotted_path(),
        _ => None,
    });
    if callee.as_deref() == Some("locale") {
        let locale = ws.index.resource_of(doc.file).and_then(|r| qbx_lua_analysis::locale::LocaleFile::load(&r.root));
        let Some(locale) = locale else { return Vec::new() };
        let lines = qbx_lua_syntax::LineIndex::new(&locale.source);
        return locale
            .keys
            .iter()
            .filter(|(key, ..)| key == value.as_str())
            .map(|(_, span, _)| {
                let range = crate::indexer::span_to_range(&locale.source, &lines, *span);
                Location::new(crate::workspace::path_to_uri(&locale.path), range)
            })
            .collect();
    }
    if callee.as_deref().is_some_and(|path| REQUIRE_CALLS.contains(&path)) {
        return ws
            .index
            .resolve_require(value, doc.file)
            .and_then(|file| location(ws, file, Range::default()))
            .into_iter()
            .collect();
    }
    ws.index
        .events()
        .filter(|(_, e)| e.name == *value && e.kind != EventKind::Trigger)
        .filter_map(|(file, event)| location(ws, file, event.range))
        .collect()
}

pub fn definition(ws: &Workspace, doc: &Document, position: Position) -> Option<GotoDefinitionResponse> {
    let offset = doc.offset(position);
    let locations = with_infer(ws, doc, |infer| match target_at(infer, doc, offset) {
        Some(Target::Local(id, _)) => {
            let local = doc.resolution.local(id);
            vec![Location::new(doc.uri.clone(), doc.range(local.decl))]
        }
        Some(Target::Global(name, _)) => ws
            .index
            .globals_named(&name, doc.file)
            .into_iter()
            .filter_map(|(file, symbol)| location(ws, file, symbol.range))
            .collect(),
        Some(Target::Member { info, .. }) => {
            info.location.and_then(|(file, range)| location(ws, file, range)).into_iter().collect()
        }
        Some(Target::Type(name, _)) => {
            let classes = ws.index.class_defs(&name).into_iter().map(|(file, class)| (file, class.range));
            let aliases = ws.index.alias_defs(&name).into_iter().map(|(file, alias)| (file, alias.range));
            classes.chain(aliases).filter_map(|(file, range)| location(ws, file, range)).collect()
        }
        None => string_definition(ws, doc, offset),
    });
    (!locations.is_empty()).then_some(GotoDefinitionResponse::Array(locations))
}
