use lsp_types::{FoldingRange, FoldingRangeKind};
use qbx_lua_syntax::ast::*;
use qbx_lua_syntax::visit::{self, Visitor};
use qbx_lua_syntax::Span;

use crate::document::Document;

struct Folds<'a> {
    doc: &'a Document,
    out: Vec<FoldingRange>,
}

impl Folds<'_> {
    fn push(&mut self, span: Span, kind: Option<FoldingRangeKind>) {
        let start = self.doc.lines.line_of(span.start);
        let end = self.doc.lines.line_of(span.end.saturating_sub(1).max(span.start));
        if end > start {
            let end_line = if kind.is_none() { end - 1 } else { end };
            if end_line > start || kind.is_some() {
                self.out.push(FoldingRange { start_line: start, end_line: end_line.max(start), kind, ..FoldingRange::default() });
            }
        }
    }
}

impl<'ast> Visitor<'ast> for Folds<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        match &stmt.kind {
            StmtKind::If { branches, else_block } => {
                for (i, branch) in branches.iter().enumerate() {
                    let end = branches
                        .get(i + 1)
                        .map(|b| b.keyword_span.start)
                        .or(else_block.as_ref().map(|b| b.span.start))
                        .unwrap_or(stmt.span.end);
                    self.push(Span::new(branch.keyword_span.start, end), None);
                }
                if let Some(block) = else_block {
                    self.push(Span::new(block.span.start, stmt.span.end), None);
                }
            }
            StmtKind::Function { .. }
            | StmtKind::LocalFunction { .. }
            | StmtKind::Do(_)
            | StmtKind::While { .. }
            | StmtKind::Repeat { .. }
            | StmtKind::NumericFor { .. }
            | StmtKind::GenericFor { .. }
            | StmtKind::Defer(_) => self.push(stmt.span, None),
            _ => {}
        }
        visit::walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        match &expr.kind {
            ExprKind::Function(func) => self.push(func.span, None),
            ExprKind::Table(_) => self.push(expr.span, None),
            ExprKind::Call { args_span, .. } | ExprKind::MethodCall { args_span, .. } => self.push(*args_span, None),
            _ => {}
        }
        visit::walk_expr(self, expr);
    }
}

pub fn folding_ranges(doc: &Document) -> Vec<FoldingRange> {
    let mut folds = Folds { doc, out: Vec::new() };
    folds.visit_block(&doc.chunk.block);

    let mut run: Option<(u32, u32)> = None;
    for comment in &doc.chunk.comments {
        let first = doc.lines.line_of(comment.span.start);
        let last = doc.lines.line_of(comment.span.end.saturating_sub(1).max(comment.span.start));
        run = match run {
            Some((start, end)) if first <= end + 1 => Some((start, last.max(end))),
            Some((start, end)) => {
                if end > start {
                    folds.out.push(FoldingRange { start_line: start, end_line: end, kind: Some(FoldingRangeKind::Comment), ..FoldingRange::default() });
                }
                Some((first, last))
            }
            None => Some((first, last)),
        };
    }
    if let Some((start, end)) = run.filter(|(s, e)| e > s) {
        folds.out.push(FoldingRange { start_line: start, end_line: end, kind: Some(FoldingRangeKind::Comment), ..FoldingRange::default() });
    }
    folds.out.sort_by_key(|f| (f.start_line, f.end_line));
    folds.out.dedup_by_key(|f| f.start_line);
    folds.out
}
