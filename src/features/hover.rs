use lsp_types::{Hover, HoverContents, Position};
use qbx_fivem_data::{native, native_docs};
use qbx_lua_analysis::scope::{LocalId, LocalKind, Resolved};
use qbx_lua_syntax::ast::ExprKind;
use qbx_lua_syntax::{SmolStr, Span};

use super::{lua_block, markdown, with_infer};
use crate::document::Document;
use crate::index::{FileOrigin, SymbolKind};
use crate::indexer::render_doc;
use crate::infer::{Decl, Infer, MemberInfo};
use crate::locate::locate;
use crate::types::Type;
use crate::workspace::Workspace;

pub enum Target {
    Local(LocalId, Span),
    Global(SmolStr, Span),
    Member { info: MemberInfo, owner: Type, span: Span },
}

impl Target {
    pub fn span(&self) -> Span {
        match self {
            Target::Local(_, span) | Target::Global(_, span) | Target::Member { span, .. } => *span,
        }
    }
}

pub fn target_at(infer: &Infer, doc: &Document, offset: u32) -> Option<Target> {
    if let Some((resolved, span)) = doc.resolution.resolved_at_offset(offset) {
        return Some(match resolved {
            Resolved::Local(id) => Target::Local(id, span),
            Resolved::Global(index) => Target::Global(doc.resolution.globals[index as usize].name.clone(), span),
        });
    }
    let located = locate(&doc.chunk, offset);
    if let Some(access) = located.member {
        let owner = infer.expr(access.base());
        let name = access.name();
        let info = infer.member(&owner, &name.text)?;
        return Some(Target::Member { info, owner, span: name.span });
    }
    if let Some((func_name, segment)) = located.func_name.filter(|(_, segment)| *segment > 0) {
        let segments: Vec<_> = func_name.path.iter().chain(&func_name.method).collect();
        let mut owner = infer.func_name_owner_type(&qbx_lua_syntax::ast::FuncName {
            base: func_name.base.clone(),
            path: Vec::new(),
            method: None,
            span: func_name.base.span,
        });
        for name in &segments[..segment - 1] {
            owner = infer.member(&owner, &name.text)?.ty;
        }
        let name = segments[segment - 1];
        let info = infer.member(&owner, &name.text)?;
        return Some(Target::Member { info, owner, span: name.span });
    }
    None
}

fn describe_value(prefix: &str, name: &str, ty: &Type) -> String {
    match ty.as_fun() {
        Some(fun) if matches!(ty, Type::Fun(_)) => format!("{prefix}{}", fun.signature(name)),
        _ => format!("{prefix}{name}: {ty}"),
    }
}

fn local_hover(infer: &Infer, id: LocalId) -> String {
    let local = infer.ctx.resolution.local(id);
    let ty = infer.local_type(id);
    let prefix = match local.kind {
        LocalKind::Param => "(parameter) ",
        LocalKind::ImplicitSelf => "(self) ",
        LocalKind::LoopVar => "(loop variable) ",
        LocalKind::Local | LocalKind::LocalFunction => "local ",
    };
    let mut out = lua_block(&describe_value(prefix, &local.name, &ty));
    let doc = match infer.ctx.decl(local.decl.start) {
        Some(Decl::Local { stmt, .. } | Decl::LocalFunction { stmt, .. }) => render_doc(&infer.ctx.doc_at(stmt.span.start)),
        Some(Decl::Param { doc_anchor: Some(anchor), .. }) => {
            infer.ctx.doc_at(*anchor).param_description(&local.name).map(Into::into)
        }
        _ => None,
    };
    if let Some(doc) = doc {
        out.push_str("\n\n");
        out.push_str(&doc);
    }
    out
}

fn native_hover(name: &str) -> Option<String> {
    let native = native(name)?;
    let mut out = lua_block(&native.signature());
    let canonical = native.alias_of.map(|target| format!(" · alias of `{target}`")).unwrap_or_default();
    out.push_str(&format!(
        "\n\n*{} native* · `{}` · `{}`{canonical}",
        native.side.label(),
        native.namespace,
        native.hash
    ));
    if let Some(docs) = native_docs(name) {
        out.push_str("\n\n");
        out.push_str(&docs);
    }
    Some(out)
}

fn global_hover(ws: &Workspace, infer: &Infer, name: &str) -> Option<String> {
    let symbols = ws.index.globals_named(name, infer.ctx.file);
    let preferred = symbols
        .iter()
        .max_by_key(|(_, s)| (matches!(s.ty, Type::GlobalTable(_) | Type::Named(..)), s.ty.specificity()));
    let Some((file, symbol)) = preferred else {
        if let Some(hover) = native_hover(name) {
            return Some(hover);
        }
        let ty = infer.global_type(name);
        return (!ty.is_unknown()).then(|| lua_block(&describe_value("", name, &ty)));
    };
    let ty = if symbol.ty.is_unknown() { infer.global_type(name) } else { symbol.ty.clone() };
    let mut out = lua_block(&describe_value("", name, &ty));
    if let Some(doc) = &symbol.doc {
        out.push_str("\n\n");
        out.push_str(doc);
    }
    if let Some(entry) = ws.index.file(*file).filter(|f| f.origin != FileOrigin::Stub && *file != infer.ctx.file) {
        let resource = entry.resource.and_then(|r| ws.index.resource(r));
        let location = match resource {
            Some(resource) => format!(
                "{}/{}",
                resource.name,
                qbx_lua_analysis::project::relative_slash_path(&resource.root, &entry.path)
            ),
            None => entry.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        };
        out.push_str(&format!("\n\n*defined in* `{location}`"));
    }
    Some(out)
}

pub fn member_hover(info: &MemberInfo, owner: &Type) -> String {
    let owner_label = match owner {
        Type::GlobalTable(path) if path.starts_with('%') => String::new(),
        other => other.without_nil().to_string(),
    };
    let is_method = info.ty.as_fun().is_some_and(|f| f.is_method);
    let qualified = match (owner_label.is_empty(), is_method) {
        (true, _) => info.name.to_string(),
        (false, true) => format!("{owner_label}:{}", info.name),
        (false, false) => format!("{owner_label}.{}", info.name),
    };
    let prefix = if matches!(info.kind, SymbolKind::Field) && info.ty.as_fun().is_none() { "(field) " } else { "" };
    let mut out = lua_block(&describe_value(prefix, &qualified, &info.ty));
    if info.deprecated {
        out.push_str("\n\n**Deprecated**");
    }
    if let Some(doc) = &info.doc {
        out.push_str("\n\n");
        out.push_str(doc);
    }
    out
}

fn string_hover(ws: &Workspace, doc: &Document, offset: u32) -> Option<(String, Span)> {
    let located = locate(&doc.chunk, offset);
    let (string, _) = located.string?;
    let ExprKind::String(value) = &string.kind else { return None };
    let registrations: Vec<_> =
        ws.index.events().filter(|(_, e)| e.name == *value && e.kind != crate::index::EventKind::Trigger).collect();
    if registrations.is_empty() {
        return None;
    }
    let mut out = format!("event `{value}`");
    for (file, event) in registrations.iter().take(5) {
        let Some(entry) = ws.index.file(*file) else { continue };
        let name = entry.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let side = entry.side.map_or("", |s| s.label());
        let handler = event.handler.as_ref().map(|h| format!(" `{}`", h.signature(""))).unwrap_or_default();
        out.push_str(&format!("\n- {side} `{name}:{}`{handler}", event.range.start.line + 1));
    }
    Some((out, string.span))
}

pub fn hover(ws: &Workspace, doc: &Document, position: Position) -> Option<Hover> {
    let offset = doc.offset(position);
    let (text, span) = with_infer(ws, doc, |infer| {
        let Some(target) = target_at(infer, doc, offset) else { return string_hover(ws, doc, offset) };
        let text = match &target {
            Target::Local(id, _) => Some(local_hover(infer, *id)),
            Target::Global(name, _) => global_hover(ws, infer, name),
            Target::Member { info, owner, .. } => Some(member_hover(info, owner)),
        };
        text.map(|t| (t, target.span()))
    })?;
    Some(Hover { contents: HoverContents::Markup(markdown(text)), range: Some(doc.range(span)) })
}
