use lsp_types::{Range, Url};
use qbx_lua_analysis::project::read_source;
use qbx_lua_analysis::scope::Resolved;
use qbx_lua_syntax::ast::*;
use qbx_lua_syntax::lexer::{decode_string, lex, TokenKind};
use qbx_lua_syntax::visit::{self, Visitor};
use qbx_lua_syntax::{SmolStr, Span};

use super::hover::{target_at, Target};
use super::with_infer;
use crate::document::Document;
use crate::index::{FileId, FileOrigin};
use crate::infer::{table_fields, Infer};
use crate::locate::string_content_span;
use crate::server::Documents;
use crate::types::Type;
use crate::workspace::Workspace;

pub struct MemberTarget {
    pub name: SmolStr,
    pub file: FileId,
    declarations: Vec<(FileId, Range)>,
}

/// The field or method under the cursor, identified by where it is defined.
pub fn member_target(ws: &Workspace, doc: &Document, offset: u32) -> Option<MemberTarget> {
    let located = with_infer(ws, doc, |infer| match target_at(infer, doc, offset)? {
        Target::Member { info, .. } => {
            let (file, range) = info.location?;
            Some((info.name, file, range))
        }
        _ => None,
    });
    // Constructor keys and annotation declarations are not expressions in the scope resolver.
    let (name, file, range) = located.or_else(|| {
        let entry = ws.index.file(doc.file)?;
        entry
            .index
            .members
            .iter()
            .map(|member| &member.symbol)
            .chain(entry.index.classes.iter().flat_map(|class| &class.fields))
            .find_map(|symbol| {
                let range = declaration_range(doc, &symbol.name, symbol.range)?;
                let position = doc.position(offset);
                (range.start <= position && position <= range.end)
                    .then(|| (symbol.name.clone(), doc.file, symbol.range))
            })
    })?;
    let mut declarations = vec![(file, range)];
    let entry = ws.index.file(file)?;
    let owner = entry
        .index
        .members
        .iter()
        .find(|member| member.symbol.name == name && member.symbol.range == range)
        .map(|member| &member.owner)
        .or_else(|| {
            entry
                .index
                .classes
                .iter()
                .find(|class| class.fields.iter().any(|field| field.name == name && field.range == range))
                .map(|class| &class.name)
        });
    if let Some(owner) = owner {
        declarations.extend(
            ws.index
                .members_of(owner, file)
                .into_iter()
                .filter(|(_, symbol)| symbol.name == name)
                .map(|(file, symbol)| (file, symbol.range)),
        );
        declarations.extend(ws.index.class_defs(owner).into_iter().flat_map(|(file, class)| {
            class.fields.iter().filter(|field| field.name == name).map(move |field| (file, field.range))
        }));
    }
    declarations.sort_by_key(|(file, range)| (*file, range.start, range.end));
    declarations.dedup();
    Some(MemberTarget { name, file, declarations })
}

/// Index entries may point at a quoted key or a whole @field comment. Resolve the actual token;
/// if it cannot be resolved, rename must fail instead of silently omitting its declaration.
fn declaration_range(doc: &Document, name: &str, range: Range) -> Option<Range> {
    let span = Span::new(doc.offset(range.start), doc.offset(range.end));
    let raw = span.text(&doc.text);
    if raw == name {
        return Some(range);
    }
    let string_range = |token_span: Span| {
        let decoded = decode_string(token_span.text(&doc.text), token_span.start);
        (decoded.errors.is_empty() && decoded.value == name)
            .then(|| string_content_span(token_span, &doc.text))
            .flatten()
            .map(|span| doc.range(span))
    };
    if string_content_span(span, &doc.text).is_some() {
        return string_range(span);
    }
    let mut rest = raw.strip_prefix("---")?.trim_start().strip_prefix("@field")?;
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    rest = rest.trim_start();
    for scope in ["public ", "private ", "protected ", "package "] {
        if let Some(stripped) = rest.strip_prefix(scope) {
            rest = stripped.trim_start();
        }
    }
    if let Some(index) = rest.strip_prefix('[') {
        rest = index.trim_start();
        let tokens = lex(rest);
        let token = tokens.tokens.first()?;
        if !matches!(token.kind, TokenKind::String | TokenKind::LongString) {
            return None;
        }
        let start = span.start + (raw.len() - rest.len()) as u32;
        return string_range(Span::new(start + token.span.start, start + token.span.end));
    }
    let len = rest.bytes().take_while(|b| b.is_ascii_alphanumeric() || *b == b'_').count();
    if &rest[..len] != name {
        return None;
    }
    let start = span.start + (raw.len() - rest.len()) as u32;
    Some(doc.range(Span::new(start, start + len as u32)))
}

struct Finder<'a, 'b> {
    doc: &'a Document,
    infer: &'a Infer<'b>,
    target: &'a MemberTarget,
    out: Vec<Range>,
    limit: usize,
    positions: Option<super::assistant::InspectionPositions<'a>>,
}

impl Finder<'_, '_> {
    fn check(&mut self, owner: &Type, name: &str, span: Span) {
        if self.out.len() >= self.limit {
            return;
        }
        let same = self
            .infer
            .member(owner, name)
            .and_then(|m| m.location)
            .is_some_and(|location| self.target.declarations.contains(&location));
        if same {
            self.out
                .push(self.positions.as_ref().map_or_else(|| self.doc.range(span), |positions| positions.range(span)));
        }
    }

    fn table(&mut self, owner: &Type, expr: &Expr) {
        if self.out.len() >= self.limit {
            return;
        }
        let Some(fields) = table_fields(expr) else { return };
        for field in fields {
            let (name, span, value) = match field {
                TableField::Named { name, value } => (name.text.as_str(), name.span, value),
                TableField::Keyed { key, value } => {
                    let Some(name) = key.as_string() else { continue };
                    let Some(span) = string_content_span(key.span, &self.doc.text) else { continue };
                    (name.as_str(), span, value)
                }
                _ => continue,
            };
            if name == self.target.name {
                self.check(owner, name, span);
            }
            if let Some(member) = self.infer.member(owner, name) {
                self.table(&member.ty, value);
            }
        }
    }
}

impl<'ast> Visitor<'ast> for Finder<'_, '_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if self.out.len() >= self.limit {
            return;
        }
        if let StmtKind::Function { name, .. } = &stmt.kind {
            let segments: Vec<&Name> = name.path.iter().chain(&name.method).collect();
            if segments.iter().any(|s| s.text == self.target.name) {
                let base = FuncName { base: name.base.clone(), path: Vec::new(), method: None, span: name.base.span };
                let mut owner = self.infer.func_name_owner_type(&base);
                for segment in segments {
                    if segment.text == self.target.name {
                        self.check(&owner, &segment.text, segment.span);
                    }
                    owner = self.infer.member(&owner, &segment.text).map(|m| m.ty).unwrap_or_default();
                }
            }
        }
        match &stmt.kind {
            StmtKind::Local { names, exprs, .. } => {
                for (name, expr) in names.iter().zip(exprs) {
                    if let Some(Resolved::Local(id)) = self.doc.resolution.resolve_at(name.name.span.start) {
                        self.table(&self.infer.local_type(id), expr);
                    }
                }
            }
            StmtKind::Assign { targets, exprs } => {
                for (target, expr) in targets.iter().zip(exprs) {
                    self.table(&self.infer.expr(target), expr);
                }
            }
            _ => {}
        }
        visit::walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if self.out.len() >= self.limit {
            return;
        }
        match &expr.kind {
            ExprKind::Field { base, name, .. } | ExprKind::MethodCall { base, method: name, .. }
                if name.text == self.target.name =>
            {
                let owner = self.infer.expr(base);
                self.check(&owner, &name.text, name.span);
            }
            ExprKind::Index { base, index, .. } if index.as_string() == Some(&self.target.name) => {
                if let Some(span) = string_content_span(index.span, &self.doc.text) {
                    let owner = self.infer.expr(base);
                    self.check(&owner, &self.target.name, span);
                }
            }
            _ => {}
        }
        visit::walk_expr(self, expr);
    }
}

fn occurrences_in(ws: &Workspace, doc: &Document, target: &MemberTarget) -> Option<Vec<Range>> {
    occurrences_limited(ws, doc, target, usize::MAX)
}
fn occurrences_limited(ws: &Workspace, doc: &Document, target: &MemberTarget, limit: usize) -> Option<Vec<Range>> {
    with_infer(ws, doc, |infer| {
        let mut finder = Finder {
            doc,
            infer,
            target,
            out: Vec::new(),
            limit,
            positions: (limit != usize::MAX).then(|| super::assistant::InspectionPositions::new(&doc.text)),
        };
        finder.visit_block(&doc.chunk.block);
        for (_, range) in target.declarations.iter().filter(|(file, _)| *file == doc.file) {
            if finder.out.len() >= limit {
                break;
            }
            finder.out.push(declaration_range(doc, &target.name, *range)?);
        }
        finder.out.sort_by_key(|range| (range.start, range.end));
        finder.out.dedup();
        Some(finder.out)
    })
}

pub(crate) fn member_occurrences_bounded(
    ws: &Workspace,
    docs: &Documents,
    doc: &Document,
    target: &MemberTarget,
    budget: &mut super::assistant::InspectionBudget,
) -> Vec<(Url, Range)> {
    let mut out = Vec::new();
    for (id, entry) in ws.index.files() {
        if out.len() >= 20_000 {
            budget.result_limit = true;
            break;
        }
        let reachable = target.declarations.iter().any(|(file, _)| *file == id)
            || ws.index.is_related(doc.file, id)
            || ws.index.is_related(target.file, id);
        if entry.origin == FileOrigin::Stub || !reachable {
            continue;
        }
        let closed;
        let source = if entry.uri == doc.uri {
            doc
        } else if let Some(open) = docs.get(&entry.uri) {
            open
        } else {
            let Some(text) = budget.read(&entry.path) else { continue };
            closed = {
                let mut closed = Document::new(entry.uri.clone(), entry.path.clone(), 0, text);
                closed.file = id;
                closed
            };
            &closed
        };
        if !budget.claim(&entry.path, source.text.len()) {
            continue;
        }
        match occurrences_limited(ws, source, target, 20_000 - out.len()) {
            Some(ranges) => out.extend(ranges.into_iter().map(|range| (entry.uri.clone(), range))),
            None => budget.skip(&entry.path),
        }
    }
    if out.len() >= 20_000 {
        budget.result_limit = true;
    }
    out
}

/// Every use of the member across the files that can reach its definition. Closed files are parsed
/// on demand. Literal spellings can be escaped, so a substring search cannot rule out references.
pub fn member_occurrences(
    ws: &Workspace,
    docs: &Documents,
    doc: &Document,
    target: &MemberTarget,
) -> Option<Vec<(Url, Range)>> {
    let mut out: Vec<(Url, Range)> = Vec::new();
    for (id, entry) in ws.index.files() {
        let reachable = target.declarations.iter().any(|(file, _)| *file == id)
            || ws.index.is_related(doc.file, id)
            || ws.index.is_related(target.file, id);
        if entry.origin == FileOrigin::Stub || !reachable {
            continue;
        }
        let ranges = match docs.get(&entry.uri) {
            Some(open) => occurrences_in(ws, open, target)?,
            None => {
                let source = read_source(&entry.path).ok()?;
                let mut closed = Document::new(entry.uri.clone(), entry.path.clone(), 0, source);
                closed.file = id;
                occurrences_in(ws, &closed, target)?
            }
        };
        out.extend(ranges.into_iter().map(|range| (entry.uri.clone(), range)));
    }
    Some(out)
}

pub fn in_document(ws: &Workspace, doc: &Document, target: &MemberTarget) -> Vec<Range> {
    occurrences_in(ws, doc, target).unwrap_or_default()
}

/// Members of the runtime stubs and of indexed libraries cannot be renamed from here.
pub fn is_renamable(ws: &Workspace, target: &MemberTarget) -> bool {
    target.declarations.iter().all(|(file, _)| ws.index.file(*file).is_some_and(|f| f.origin == FileOrigin::Workspace))
}
