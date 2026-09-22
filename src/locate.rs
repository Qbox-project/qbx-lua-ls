use qbx_lua_syntax::ast::*;
use qbx_lua_syntax::visit::{self, Visitor};
use qbx_lua_syntax::Span;

/// What the cursor is on, when it is on a name that belongs to a member access.
pub enum MemberAccess<'a> {
    Field { base: &'a Expr, name: &'a Name },
    Method { base: &'a Expr, name: &'a Name },
    Index { base: &'a Expr, index: &'a Expr },
}

impl<'a> MemberAccess<'a> {
    pub fn base(&self) -> &'a Expr {
        match self {
            MemberAccess::Field { base, .. } | MemberAccess::Method { base, .. } | MemberAccess::Index { base, .. } => {
                base
            }
        }
    }

    pub fn name(&self, source: &str) -> Option<Name> {
        match self {
            MemberAccess::Field { name, .. } | MemberAccess::Method { name, .. } => Some((*name).clone()),
            MemberAccess::Index { index, .. } => {
                Some(Name { text: index.as_string()?.clone(), span: string_content_span(index.span, source)? })
            }
        }
    }

    pub fn is_method(&self) -> bool {
        matches!(self, MemberAccess::Method { .. })
    }
}

/// The replaceable contents of a string token, retaining its quote style and long-string delimiter.
pub fn string_content_span(span: Span, source: &str) -> Option<Span> {
    let raw = span.text(source);
    let bytes = raw.as_bytes();
    let first = *bytes.first()?;
    if matches!(first, b'\'' | b'"') && bytes.len() >= 2 && bytes.last() == Some(&first) {
        return Some(Span::new(span.start + 1, span.end - 1));
    }
    if first != b'[' {
        return None;
    }
    let level = bytes[1..].iter().take_while(|&&b| b == b'=').count();
    let delimiter_len = level + 2;
    if bytes.get(delimiter_len - 1) != Some(&b'[')
        || raw.len() < delimiter_len * 2
        || !raw.ends_with(&format!("]{}]", "=".repeat(level)))
    {
        return None;
    }
    let body = &raw[delimiter_len..raw.len() - delimiter_len];
    let newline_len = if body.starts_with("\r\n") { 2 } else { usize::from(body.starts_with('\n')) };
    Some(Span::new(span.start + (delimiter_len + newline_len) as u32, span.end - delimiter_len as u32))
}

pub struct CallSite<'a> {
    pub call: &'a Expr,
    pub base: &'a Expr,
    pub method: Option<&'a Name>,
    pub args: &'a [Expr],
    pub args_span: Span,
}

impl CallSite<'_> {
    /// The index of the argument the cursor is in, counting top-level commas before `offset`.
    pub fn active_argument(&self, source: &str, offset: u32) -> usize {
        let mut index = 0;
        for (i, arg) in self.args.iter().enumerate() {
            if arg.span.end >= offset {
                break;
            }
            let until = self.args.get(i + 1).map_or(offset, |next| next.span.start.min(offset));
            if Span::new(arg.span.end, until.max(arg.span.end)).text(source).contains(',') {
                index = i + 1;
            }
        }
        index
    }
}

struct Locator<'a> {
    offset: u32,
    member: Option<MemberAccess<'a>>,
    call: Option<CallSite<'a>>,
    string: Option<(&'a Expr, Option<(&'a Expr, usize)>)>,
    func_name: Option<(&'a FuncName, usize)>,
    table_in_call: Option<(&'a Expr, usize, &'a Expr)>,
}

impl<'a> Locator<'a> {
    fn visit_args(&mut self, call: &'a Expr, args: &'a [Expr]) {
        for (i, arg) in args.iter().enumerate() {
            if !arg.span.contains_inclusive(self.offset) {
                continue;
            }
            match &arg.kind {
                ExprKind::String(_) => self.string = Some((arg, Some((call, i)))),
                ExprKind::Table(_) => self.table_in_call = Some((call, i, arg)),
                _ => {}
            }
        }
    }
}

impl<'a> Visitor<'a> for Locator<'a> {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        if !stmt.span.contains_inclusive(self.offset) {
            return;
        }
        if let StmtKind::Function { name, .. } = &stmt.kind {
            let segments = std::iter::once(&name.base).chain(&name.path).chain(&name.method);
            for (i, segment) in segments.enumerate() {
                if segment.span.contains_inclusive(self.offset) {
                    self.func_name = Some((name, i));
                }
            }
        }
        visit::walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'a Expr) {
        if !expr.span.contains_inclusive(self.offset) {
            return;
        }
        match &expr.kind {
            ExprKind::Field { base, name, .. } if name.span.contains_inclusive(self.offset) => {
                self.member = Some(MemberAccess::Field { base, name });
            }
            ExprKind::Index { base, index, .. }
                if index.as_string().is_some() && index.span.contains_inclusive(self.offset) =>
            {
                self.member = Some(MemberAccess::Index { base, index });
            }
            ExprKind::MethodCall { base, method, args, args_span, .. } => {
                if method.span.contains_inclusive(self.offset) {
                    self.member = Some(MemberAccess::Method { base, name: method });
                }
                if args_span.start < self.offset && self.offset <= args_span.end {
                    self.call = Some(CallSite { call: expr, base, method: Some(method), args, args_span: *args_span });
                }
                self.visit_args(expr, args);
            }
            ExprKind::Call { callee, args, args_span, .. } => {
                if args_span.start < self.offset && self.offset <= args_span.end {
                    self.call = Some(CallSite { call: expr, base: callee, method: None, args, args_span: *args_span });
                }
                self.visit_args(expr, args);
            }
            ExprKind::String(_) if self.string.is_none_or(|(s, _)| !std::ptr::eq(s, expr)) => {
                self.string = Some((expr, None));
            }
            _ => {}
        }
        visit::walk_expr(self, expr);
    }
}

pub struct Located<'a> {
    pub member: Option<MemberAccess<'a>>,
    /// The innermost call whose argument list contains the cursor.
    pub call: Option<CallSite<'a>>,
    /// A string literal under the cursor, with the call and argument position it is passed to.
    pub string: Option<(&'a Expr, Option<(&'a Expr, usize)>)>,
    /// A segment of a `function a.b:c()` name under the cursor.
    pub func_name: Option<(&'a FuncName, usize)>,
    /// A table constructor under the cursor that is passed directly as a call argument.
    pub table_in_call: Option<(&'a Expr, usize, &'a Expr)>,
}

pub fn locate(chunk: &Chunk, offset: u32) -> Located<'_> {
    let mut locator = Locator { offset, member: None, call: None, string: None, func_name: None, table_in_call: None };
    locator.visit_block(&chunk.block);
    Located {
        member: locator.member,
        call: locator.call,
        string: locator.string,
        func_name: locator.func_name,
        table_in_call: locator.table_in_call,
    }
}
