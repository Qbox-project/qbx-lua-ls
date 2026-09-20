use lsp_types::{Range, Url};
use qbx_lua_analysis::project::read_source;
use qbx_lua_syntax::ast::*;
use qbx_lua_syntax::visit::{self, Visitor};
use qbx_lua_syntax::SmolStr;

use super::hover::{target_at, Target};
use super::with_infer;
use crate::document::Document;
use crate::index::{FileId, FileOrigin};
use crate::infer::Infer;
use crate::server::Documents;
use crate::workspace::Workspace;

pub struct MemberTarget {
    pub name: SmolStr,
    pub file: FileId,
    pub range: Range,
}

/// The field or method under the cursor, identified by where it is defined.
pub fn member_target(ws: &Workspace, doc: &Document, offset: u32) -> Option<MemberTarget> {
    with_infer(ws, doc, |infer| match target_at(infer, doc, offset)? {
        Target::Member { info, .. } => {
            let (file, range) = info.location?;
            Some(MemberTarget { name: info.name, file, range })
        }
        _ => None,
    })
}

struct Finder<'a, 'b> {
    doc: &'a Document,
    infer: &'a Infer<'b>,
    target: &'a MemberTarget,
    out: Vec<Range>,
}

impl Finder<'_, '_> {
    fn check(&mut self, owner: &crate::types::Type, name: &Name) {
        let same = self.infer.member(owner, &name.text).and_then(|m| m.location)
            == Some((self.target.file, self.target.range));
        if same {
            self.out.push(self.doc.range(name.span));
        }
    }
}

impl<'ast> Visitor<'ast> for Finder<'_, '_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if let StmtKind::Function { name, .. } = &stmt.kind {
            let segments: Vec<&Name> = name.path.iter().chain(&name.method).collect();
            if segments.iter().any(|s| s.text == self.target.name) {
                let base = FuncName { base: name.base.clone(), path: Vec::new(), method: None, span: name.base.span };
                let mut owner = self.infer.func_name_owner_type(&base);
                for segment in segments {
                    if segment.text == self.target.name {
                        self.check(&owner, segment);
                    }
                    owner = self.infer.member(&owner, &segment.text).map(|m| m.ty).unwrap_or_default();
                }
            }
        }
        visit::walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        match &expr.kind {
            ExprKind::Field { base, name, .. } | ExprKind::MethodCall { base, method: name, .. }
                if name.text == self.target.name =>
            {
                let owner = self.infer.expr(base);
                self.check(&owner, name);
            }
            _ => {}
        }
        visit::walk_expr(self, expr);
    }
}

fn occurrences_in(ws: &Workspace, doc: &Document, target: &MemberTarget) -> Vec<Range> {
    with_infer(ws, doc, |infer| {
        let mut finder = Finder { doc, infer, target, out: Vec::new() };
        finder.visit_block(&doc.chunk.block);
        finder.out
    })
}

/// Every use of the member across the files that can reach its definition. Closed files are parsed
/// on demand, and only when they mention the name at all.
pub fn member_occurrences(
    ws: &Workspace,
    docs: &Documents,
    doc: &Document,
    target: &MemberTarget,
) -> Vec<(Url, Range)> {
    let mut out: Vec<(Url, Range)> = Vec::new();
    for (id, entry) in ws.index.files() {
        let reachable = id == target.file || ws.index.is_related(doc.file, id) || ws.index.is_related(target.file, id);
        if entry.origin == FileOrigin::Stub || !reachable {
            continue;
        }
        let ranges = match docs.get(&entry.uri) {
            Some(open) => occurrences_in(ws, open, target),
            None => {
                let Ok(source) = read_source(&entry.path) else { continue };
                if !source.contains(target.name.as_str()) {
                    continue;
                }
                let mut closed = Document::new(entry.uri.clone(), entry.path.clone(), 0, source);
                closed.file = id;
                occurrences_in(ws, &closed, target)
            }
        };
        out.extend(ranges.into_iter().map(|range| (entry.uri.clone(), range)));
    }
    if let Some(definition) = ws.index.file(target.file) {
        let declared = (definition.uri.clone(), target.range);
        if !out.contains(&declared) {
            out.push(declared);
        }
    }
    out
}

pub fn in_document(ws: &Workspace, doc: &Document, target: &MemberTarget) -> Vec<Range> {
    let mut ranges = occurrences_in(ws, doc, target);
    if target.file == doc.file && !ranges.contains(&target.range) {
        ranges.push(target.range);
    }
    ranges
}

/// Members of the runtime stubs and of indexed libraries cannot be renamed from here.
pub fn is_renamable(ws: &Workspace, target: &MemberTarget) -> bool {
    ws.index.file(target.file).is_some_and(|f| f.origin == FileOrigin::Workspace)
}
