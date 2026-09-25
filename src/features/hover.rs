use lsp_types::{Hover, HoverContents, Position};
use qbx_fivem_data::{native, native_docs};
use qbx_lua_analysis::scope::{LocalId, LocalKind, Resolved};
use qbx_lua_syntax::ast::ExprKind;
use qbx_lua_syntax::{CommentKind, SmolStr, Span};

use super::{lua_block, markdown, with_infer};
use crate::document::Document;
use crate::index::{ClassDef, FileId, FileOrigin, SymbolKind};
use crate::indexer::render_doc;
use crate::infer::{Decl, Infer, MemberInfo};
use crate::locate::locate;
use crate::luacats::type_name_at;
use crate::types::Type;
use crate::workspace::Workspace;

pub enum Target {
    Local(LocalId, Span),
    Global(SmolStr, Span),
    Member { info: MemberInfo, owner: Type, span: Span },
    Type(SmolStr, Span),
}

impl Target {
    pub fn span(&self) -> Span {
        match self {
            Target::Local(_, span) | Target::Global(_, span) | Target::Member { span, .. } => *span,
            Target::Type(_, span) => *span,
        }
    }
}

/// A class or alias named in the doc comment under the cursor.
fn annotation_type_at(doc: &Document, offset: u32) -> Option<(SmolStr, Span)> {
    let comment = doc.chunk.comments.iter().find(|c| c.span.contains_inclusive(offset))?;
    let content = comment.content.text(&doc.text);
    // The content of a `---` line starts at its third dash. `--[[@as T]]` is the only long form.
    let line = match comment.kind {
        CommentKind::Line => content.strip_prefix('-')?,
        CommentKind::Long if content.starts_with("@as") => content,
        _ => return None,
    };
    let line_start = comment.content.end - line.len() as u32;
    let (start, name) = type_name_at(line, offset.checked_sub(line_start)? as usize)?;
    let start = line_start + start as u32;
    Some((SmolStr::new(name), Span::new(start, start + name.len() as u32)))
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
        let name = access.name(&doc.text)?;
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
    annotation_type_at(doc, offset).map(|(name, span)| Target::Type(name, span))
}

const MAX_OVERVIEW_FIELDS: usize = 14;

/// `name: type`, a function signature, or for tables an overview of the fields that are in scope
/// for this file, with the literal values the index remembered. Aliases follow on their own lines.
fn describe_value(infer: &Infer, prefix: &str, name: &str, ty: &Type, literal: Option<&str>) -> String {
    if let (Some(fun), Type::Fun(_)) = (ty.as_fun(), ty) {
        return format!("{prefix}{}", fun.signature(name));
    }
    let mut out = value_overview(infer, prefix, name, ty, literal);
    for (alias, target) in alias_expansions(infer, ty) {
        out.push_str(&format!("\ntype {alias} = {target}"));
    }
    out
}

fn value_overview(infer: &Infer, prefix: &str, name: &str, ty: &Type, literal: Option<&str>) -> String {
    let bare = ty.without_nil();
    let members = infer.members(&table_part(infer, &bare, 0));
    if members.is_empty() {
        let value = literal.map(|l| format!(" = {l}")).unwrap_or_default();
        return format!("{prefix}{name}: {}{value}", shown_type(infer, ty));
    }
    let label = match &bare {
        Type::Named(class, _) => format!("{class} "),
        _ => String::new(),
    };
    let mut out = format!("{prefix}{name}: {label}{{");
    for member in members.iter().take(MAX_OVERVIEW_FIELDS) {
        let ty = match &member.ty {
            Type::Fun(_) => "function".to_string(),
            Type::GlobalTable(_) | Type::Shape(_) => "table".to_string(),
            other => other.to_string(),
        };
        let value = member.literal.as_ref().map(|l| format!(" = {l}")).unwrap_or_default();
        out.push_str(&format!("\n    {}: {ty}{value},", member.name));
    }
    if members.len() > MAX_OVERVIEW_FIELDS {
        out.push_str(&format!("\n    ...(+{})", members.len() - MAX_OVERVIEW_FIELDS));
    }
    out.push_str("\n}");
    out
}

/// A table the index holds with only integer keys, such as `local list = { 'a', 'b' }` or
/// `Config.Items = { 'a', 'b' }`, is shown as `string[]` rather than as a bare `table`.
fn shown_type(infer: &Infer, ty: &Type) -> Type {
    match ty {
        Type::GlobalTable(_) => match infer.key_value_types(ty, false) {
            (Type::Integer, value) if !value.is_unknown() => Type::Array(Box::new(value)),
            _ => ty.clone(),
        },
        _ => ty.clone(),
    }
}

/// The parts of `ty` whose members a hover lists. `"male"|"female"`, or an alias of it, is shown as
/// itself rather than as the `string` library, and `Garage|string` lists only the `Garage` fields.
fn table_part(infer: &Infer, ty: &Type, depth: u32) -> Type {
    if depth > 8 {
        return Type::Unknown;
    }
    match infer.resolve_alias(ty) {
        Type::Union(types) => Type::union(types.iter().map(|t| table_part(infer, t, depth + 1))),
        Type::GlobalTable(_) | Type::Named(..) | Type::Shape(_) | Type::Require(_) | Type::Exports(Some(_)) => {
            ty.clone()
        }
        _ => Type::Unknown,
    }
}

/// The aliases in `ty` that stand for something other than a table, such as `"male"|"female"`,
/// expanded one layer the way a class is expanded into its fields.
fn alias_expansions(infer: &Infer, ty: &Type) -> Vec<(SmolStr, Type)> {
    let parts = match ty {
        Type::Union(types) => types.as_slice(),
        other => std::slice::from_ref(other),
    };
    let mut out: Vec<(SmolStr, Type)> = Vec::new();
    for part in parts {
        let part = match part {
            Type::Array(inner) => &**inner,
            other => other,
        };
        let Type::Named(name, _) = part else { continue };
        if infer.index.class(name).is_some() || out.iter().any(|(seen, _)| seen == name) {
            continue;
        }
        let Some((_, alias)) = infer.index.alias(name) else { continue };
        if table_part(infer, &alias.ty, 0).is_unknown() {
            out.push((name.clone(), alias.ty.clone()));
        }
    }
    out
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
    let mut out = lua_block(&describe_value(infer, prefix, &local.name, &ty, None));
    let doc = match infer.ctx.decl(local.decl.start) {
        Some(Decl::Local { stmt, .. } | Decl::LocalFunction { stmt, .. }) => {
            render_doc(&infer.ctx.doc_at(stmt.span.start))
        }
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
        return (!ty.is_unknown()).then(|| lua_block(&describe_value(infer, "(global) ", name, &ty, None)));
    };
    // Going through `global_type` merges the table with members other files of the resource add.
    let ty = match infer.global_type(name) {
        Type::Unknown => symbol.ty.clone(),
        resolved => resolved,
    };
    let mut out = lua_block(&describe_value(infer, "(global) ", name, &ty, symbol.literal.as_deref()));
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

pub fn member_hover(infer: &Infer, info: &MemberInfo, owner: &Type) -> String {
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
    let mut out = lua_block(&describe_value(infer, prefix, &qualified, &info.ty, info.literal.as_deref()));
    if info.deprecated {
        out.push_str("\n\n**Deprecated**");
    }
    if let Some(doc) = &info.doc {
        out.push_str("\n\n");
        out.push_str(doc);
    }
    out
}

fn class_hover(infer: &Infer, class: &ClassDef) -> String {
    let mut declaration = format!("(class) {}", class.name);
    if !class.parents.is_empty() {
        declaration.push_str(&format!(" : {}", class.parents.join(", ")));
    }
    let members = infer.members(&Type::Named(class.name.clone(), Vec::new()));
    if !members.is_empty() || class.index.is_some() {
        declaration.push_str(" {");
        for member in members.iter().take(MAX_OVERVIEW_FIELDS) {
            declaration.push_str(&format!("\n    {}: {},", member.name, member.ty));
        }
        if let Some((key, value)) = &class.index {
            declaration.push_str(&format!("\n    [{key}]: {value},"));
        }
        if members.len() > MAX_OVERVIEW_FIELDS {
            declaration.push_str(&format!("\n    ...(+{})", members.len() - MAX_OVERVIEW_FIELDS));
        }
        declaration.push_str("\n}");
    }
    let mut out = lua_block(&declaration);
    if let Some(doc) = &class.doc {
        out.push_str("\n\n");
        out.push_str(doc);
    }
    out
}

/// A class or alias, preferring workspace declarations over the built-in library and this file's
/// over those of other files.
fn type_hover(infer: &Infer, name: &str) -> Option<String> {
    let preference = |file: FileId| {
        (infer.index.file(file).is_some_and(|entry| entry.origin != FileOrigin::Stub), file == infer.ctx.file)
    };
    if let Some((_, class)) = infer.index.class_defs(name).into_iter().max_by_key(|(file, _)| preference(*file)) {
        return Some(class_hover(infer, class));
    }
    let (_, alias) = infer.index.alias_defs(name).into_iter().max_by_key(|(file, _)| preference(*file))?;
    let mut out = lua_block(&format!("type {name} = {}", alias.ty));
    if let Some(doc) = &alias.doc {
        out.push_str("\n\n");
        out.push_str(doc);
    }
    Some(out)
}

fn string_hover(ws: &Workspace, doc: &Document, offset: u32) -> Option<(String, Span)> {
    let located = locate(&doc.chunk, offset);
    let (string, call) = located.string?;
    let ExprKind::String(value) = &string.kind else { return None };
    let callee = call.and_then(|(call, _)| match &call.kind {
        ExprKind::Call { callee, .. } => callee.dotted_path(),
        _ => None,
    });
    if callee.as_deref() == Some("locale") {
        let resource = ws.index.resource_of(doc.file)?;
        let locale = qbx_lua_analysis::locale::LocaleFile::load(&resource.root)?;
        let file = locale.path.file_name()?.to_string_lossy().into_owned();
        let text = locale.text_of(value)?;
        return Some((format!("`{value}` · locales/{file}\n\n{text}"), string.span));
    }
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
            Target::Member { info, owner, .. } => Some(member_hover(infer, info, owner)),
            Target::Type(name, _) => type_hover(infer, name),
        };
        text.map(|t| (t, target.span()))
    })?;
    Some(Hover { contents: HoverContents::Markup(markdown(text)), range: Some(doc.range(span)) })
}
