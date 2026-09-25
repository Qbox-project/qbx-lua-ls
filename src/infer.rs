use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use lsp_types::Range;
use qbx_fivem_data::native;
use qbx_lua_analysis::env::leading_doc_lines;
use qbx_lua_analysis::scope::{LocalId, LocalKind, Resolution, Resolved};
use qbx_lua_syntax::ast::*;
use qbx_lua_syntax::{NumberValue, SmolStr};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::index::{FileId, Index, SymbolKind};
use crate::luacats::{parse_doc_lines, DocGroup};
use crate::types::{FunType, Param, Shape, ShapeField, Type};

const MAX_DEPTH: u32 = 24;
const MAX_SHAPE_FIELDS: usize = 96;

pub enum Decl<'a> {
    Local { stmt: &'a Stmt, index: usize },
    LocalFunction { stmt: &'a Stmt, func: &'a FuncBody },
    Param { func: &'a FuncBody, index: usize, doc_anchor: Option<u32>, expected: Option<Expected<'a>> },
    SelfParam { name: &'a FuncName },
    NumericFor,
    GenericFor { stmt: &'a Stmt, index: usize },
}

/// A function literal passed as a call argument: its parameters can be typed from the callee.
#[derive(Clone, Copy)]
pub struct Expected<'a> {
    pub call: &'a Expr,
    pub arg_index: usize,
}

pub struct FileContext<'a> {
    pub file: FileId,
    pub source: &'a str,
    pub chunk: &'a Chunk,
    pub resolution: &'a Resolution,
    decls: FxHashMap<u32, Decl<'a>>,
    docs: RefCell<FxHashMap<u32, Rc<DocGroup>>>,
}

impl<'a> FileContext<'a> {
    pub fn new(file: FileId, source: &'a str, chunk: &'a Chunk, resolution: &'a Resolution) -> Self {
        let mut collector = DeclCollector { decls: FxHashMap::default() };
        collector.block(&chunk.block);
        Self { file, source, chunk, resolution, decls: collector.decls, docs: RefCell::new(FxHashMap::default()) }
    }

    pub fn decl(&self, decl_start: u32) -> Option<&Decl<'a>> {
        self.decls.get(&decl_start)
    }

    pub fn doc_at(&self, stmt_start: u32) -> Rc<DocGroup> {
        if let Some(doc) = self.docs.borrow().get(&stmt_start) {
            return doc.clone();
        }
        let lines = leading_doc_lines(self.source, &self.chunk.comments, stmt_start);
        let doc = Rc::new(parse_doc_lines(&lines));
        self.docs.borrow_mut().insert(stmt_start, doc.clone());
        doc
    }

    pub fn local_owner_key(&self, decl_start: u32) -> SmolStr {
        SmolStr::new(format!("%f{}:{}", self.file, decl_start))
    }

    /// The declaration behind a `local_owner_key` of this file.
    fn local_owner_decl(&self, owner: &str) -> Option<u32> {
        let (file, decl_start) = owner.strip_prefix("%f")?.split_once(':')?;
        (file.parse::<FileId>().ok()? == self.file).then(|| decl_start.parse().ok()).flatten()
    }
}

struct DeclCollector<'a> {
    decls: FxHashMap<u32, Decl<'a>>,
}

impl<'a> DeclCollector<'a> {
    fn block(&mut self, block: &'a Block) {
        for stmt in &block.stmts {
            self.stmt(stmt);
        }
    }

    fn func(&mut self, func: &'a FuncBody, doc_anchor: Option<u32>, expected: Option<Expected<'a>>) {
        for (index, param) in func.params.iter().enumerate() {
            self.decls.insert(param.span.start, Decl::Param { func, index, doc_anchor, expected });
        }
        self.block(&func.body);
    }

    fn stmt(&mut self, stmt: &'a Stmt) {
        let anchor = Some(stmt.span.start);
        match &stmt.kind {
            StmtKind::Local { names, exprs, .. } => {
                for (index, name) in names.iter().enumerate() {
                    self.decls.insert(name.name.span.start, Decl::Local { stmt, index });
                }
                for expr in exprs {
                    self.expr(expr, anchor);
                }
            }
            StmtKind::LocalFunction { name, func } => {
                self.decls.insert(name.span.start, Decl::LocalFunction { stmt, func });
                self.func(func, anchor, None);
            }
            StmtKind::Function { name, func } => {
                if name.method.is_some() {
                    self.decls.insert(func.params_span.start, Decl::SelfParam { name });
                }
                self.func(func, anchor, None);
            }
            StmtKind::Assign { targets, exprs } => {
                targets.iter().for_each(|e| self.expr(e, None));
                exprs.iter().for_each(|e| self.expr(e, anchor));
            }
            StmtKind::CompoundAssign { target, expr, .. } => {
                self.expr(target, None);
                self.expr(expr, None);
            }
            StmtKind::Expr(expr) => self.expr(expr, None),
            StmtKind::Do(body) | StmtKind::Defer(body) => self.block(body),
            StmtKind::While { cond, body } => {
                self.expr(cond, None);
                self.block(body);
            }
            StmtKind::Repeat { body, cond } => {
                self.block(body);
                self.expr(cond, None);
            }
            StmtKind::If { branches, else_block } => {
                for branch in branches {
                    self.expr(&branch.cond, None);
                    self.block(&branch.block);
                }
                if let Some(block) = else_block {
                    self.block(block);
                }
            }
            StmtKind::NumericFor { var, start, limit, step, body } => {
                self.decls.insert(var.span.start, Decl::NumericFor);
                self.expr(start, None);
                self.expr(limit, None);
                if let Some(step) = step {
                    self.expr(step, None);
                }
                self.block(body);
            }
            StmtKind::GenericFor { names, exprs, body } => {
                for (index, name) in names.iter().enumerate() {
                    self.decls.insert(name.span.start, Decl::GenericFor { stmt, index });
                }
                exprs.iter().for_each(|e| self.expr(e, None));
                self.block(body);
            }
            StmtKind::Return(exprs) => exprs.iter().for_each(|e| self.expr(e, None)),
            StmtKind::Break | StmtKind::Goto(_) | StmtKind::Label(_) | StmtKind::Error => {}
        }
    }

    fn expr(&mut self, expr: &'a Expr, doc_anchor: Option<u32>) {
        match &expr.kind {
            ExprKind::Function(func) => self.func(func, doc_anchor, None),
            ExprKind::Call { callee, args, .. } => {
                self.expr(callee, None);
                self.call_args(expr, args);
            }
            ExprKind::MethodCall { base, args, .. } => {
                self.expr(base, None);
                self.call_args(expr, args);
            }
            ExprKind::Index { base, index, .. } => {
                self.expr(base, None);
                self.expr(index, None);
            }
            ExprKind::Field { base, .. } => self.expr(base, None),
            ExprKind::Binary { lhs, rhs, .. } => {
                self.expr(lhs, None);
                self.expr(rhs, None);
            }
            ExprKind::Unary { expr, .. } | ExprKind::Paren(expr) => self.expr(expr, None),
            ExprKind::Table(fields) => {
                for field in fields {
                    match field {
                        TableField::Positional(value) | TableField::Named { value, .. } => self.expr(value, None),
                        TableField::Keyed { key, value } => {
                            self.expr(key, None);
                            self.expr(value, None);
                        }
                        TableField::SetMember(_) => {}
                    }
                }
            }
            _ => {}
        }
    }

    fn call_args(&mut self, call: &'a Expr, args: &'a [Expr]) {
        for (arg_index, arg) in args.iter().enumerate() {
            match &arg.kind {
                ExprKind::Function(func) => self.func(func, None, Some(Expected { call, arg_index })),
                _ => self.expr(arg, None),
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct MemberInfo {
    pub name: SmolStr,
    pub ty: Type,
    pub doc: Option<Arc<str>>,
    pub deprecated: bool,
    pub literal: Option<SmolStr>,
    pub kind: SymbolKind,
    pub location: Option<(FileId, Range)>,
}

pub struct Infer<'a> {
    pub ctx: &'a FileContext<'a>,
    pub index: &'a Index,
    locals: RefCell<FxHashMap<LocalId, Type>>,
    in_progress: RefCell<FxHashSet<LocalId>>,
    depth: Cell<u32>,
}

/// Natives call their handles `Vehicle`, `Ped` and so on. They are integers, and resources (ox_lib,
/// qbx_core) declare unrelated classes under the same names, so they must not resolve as classes.
const NATIVE_HANDLE_TYPES: &[&str] =
    &["Vehicle", "Ped", "Entity", "Object", "Player", "Hash", "Cam", "Blip", "Pickup", "ScrHandle", "FireId"];

fn native_type(name: &str) -> Type {
    if NATIVE_HANDLE_TYPES.contains(&name) {
        return Type::Handle(SmolStr::new(name));
    }
    Type::named(name)
}

pub fn native_fun_type(native: &qbx_fivem_data::Native) -> FunType {
    FunType {
        params: native
            .params()
            .map(|(name, ty)| Param { name: SmolStr::new(name), ty: native_type(ty), optional: false })
            .collect(),
        returns: native.returns().map(native_type).collect(),
        is_method: false,
        generics: Vec::new(),
    }
}

impl<'a> Infer<'a> {
    pub fn new(ctx: &'a FileContext<'a>, index: &'a Index) -> Self {
        Self {
            ctx,
            index,
            locals: RefCell::new(FxHashMap::default()),
            in_progress: RefCell::new(FxHashSet::default()),
            depth: Cell::new(0),
        }
    }

    fn guarded<T: Default>(&self, f: impl FnOnce() -> T) -> T {
        if self.depth.get() >= MAX_DEPTH {
            return T::default();
        }
        self.depth.set(self.depth.get() + 1);
        let out = f();
        self.depth.set(self.depth.get() - 1);
        out
    }

    pub fn expr(&self, expr: &Expr) -> Type {
        self.expr_multi(expr).into_iter().next().unwrap_or_default()
    }

    pub fn expr_multi(&self, expr: &Expr) -> Vec<Type> {
        self.guarded(|| match &expr.kind {
            ExprKind::Call { callee, args, .. } => self.call(callee, None, args),
            ExprKind::MethodCall { base, method, args, .. } => self.call(base, Some(method), args),
            _ => vec![self.single(expr)],
        })
    }

    fn single(&self, expr: &Expr) -> Type {
        match &expr.kind {
            ExprKind::Nil => Type::Nil,
            ExprKind::True => Type::BooleanLit(true),
            ExprKind::False => Type::BooleanLit(false),
            ExprKind::Number(NumberValue::Int(i)) => Type::IntLit(*i),
            ExprKind::Number(NumberValue::Float(_)) => Type::Number,
            ExprKind::String(s) => Type::StringLit(s.clone()),
            ExprKind::JenkinsHash(_) => Type::Integer,
            ExprKind::Vararg => Type::Any,
            ExprKind::Function(func) => Type::Fun(Arc::new(self.fun_type(func, None, false))),
            ExprKind::Name(name) => self.name(name),
            ExprKind::Paren(inner) => self.expr(inner),
            ExprKind::Field { base, name, .. } => {
                if let ExprKind::Name(root) = &base.kind {
                    let is_env = matches!(root.text.as_str(), "_ENV" | "_G")
                        && matches!(self.ctx.resolution.resolve_at(root.span.start), Some(Resolved::Global(_)));
                    if is_env {
                        return self.global_type(&name.text);
                    }
                }
                let base_ty = self.expr(base);
                self.member(&base_ty, &name.text).map(|m| m.ty).unwrap_or_default()
            }
            ExprKind::Index { base, index, .. } => self.index_expr(base, index),
            ExprKind::Table(fields) => self.table(fields),
            ExprKind::Binary { op, lhs, rhs, .. } => self.binary(*op, lhs, rhs),
            ExprKind::Unary { op, expr } => match op {
                UnOp::Not => Type::Boolean,
                UnOp::Len => match self.expr(expr) {
                    Type::Named(name, _) if name.starts_with("vector") => Type::Number,
                    _ => Type::Integer,
                },
                UnOp::BNot => Type::Integer,
                UnOp::Neg => self.expr(expr).widen(),
            },
            ExprKind::Call { .. } | ExprKind::MethodCall { .. } | ExprKind::Error => Type::Unknown,
        }
    }

    fn name(&self, name: &Name) -> Type {
        match self.ctx.resolution.resolve_at(name.span.start) {
            Some(Resolved::Local(id)) => self.local_type(id),
            _ => self.global_type(&name.text),
        }
    }

    pub fn global_type(&self, name: &str) -> Type {
        if name == "exports" {
            return Type::Exports(None);
        }
        let symbols = self.index.globals_named(name, self.ctx.file);
        let known = symbols
            .iter()
            .map(|(_, s)| &s.ty)
            .filter(|ty| !ty.is_unknown())
            .max_by_key(|ty| (matches!(ty, Type::GlobalTable(_) | Type::Named(..)), ty.specificity()));
        if let Some(ty) = known.filter(|ty| !(matches!(ty, Type::Table) && self.index.has_members(name))) {
            // `local lib = {}` published with `_ENV.lib = lib` and then extended as `function lib.x()`
            // elsewhere keeps its members under two owners.
            let aliased = matches!(ty, Type::GlobalTable(owner) if owner != name) && self.index.has_members(name);
            return if aliased { Type::union([ty.clone(), Type::GlobalTable(SmolStr::new(name))]) } else { ty.clone() };
        }
        if self.index.has_members(name) {
            return Type::GlobalTable(SmolStr::new(name));
        }
        match native(name) {
            Some(native) => Type::Fun(Arc::new(native_fun_type(&native))),
            None => Type::Unknown,
        }
    }

    pub fn local_type(&self, id: LocalId) -> Type {
        if let Some(ty) = self.locals.borrow().get(&id) {
            return ty.clone();
        }
        if !self.in_progress.borrow_mut().insert(id) {
            return Type::Unknown;
        }
        let ty = self.guarded(|| self.compute_local(id));
        self.in_progress.borrow_mut().remove(&id);
        self.locals.borrow_mut().insert(id, ty.clone());
        ty
    }

    fn compute_local(&self, id: LocalId) -> Type {
        let local = self.ctx.resolution.local(id);
        if local.kind == LocalKind::ImplicitSelf {
            return match self.ctx.decl(local.decl.start) {
                Some(Decl::SelfParam { name }) => self.func_name_owner_type(name),
                _ => Type::Unknown,
            };
        }
        let Some(decl) = self.ctx.decl(local.decl.start) else { return Type::Unknown };
        match decl {
            Decl::Local { stmt, index } => self.local_stmt_type(stmt, *index, local.func == 0),
            Decl::LocalFunction { stmt, func } => {
                Type::Fun(Arc::new(self.fun_type(func, Some(stmt.span.start), false)))
            }
            Decl::Param { func, index, doc_anchor, expected } => {
                let name = &func.params[*index].text;
                if let Some(anchor) = doc_anchor {
                    let doc = self.ctx.doc_at(*anchor);
                    if let Some(param) = doc.params.iter().find(|p| p.name == *name) {
                        return if param.optional { param.ty.clone().optional() } else { param.ty.clone() };
                    }
                }
                expected.as_ref().and_then(|e| self.expected_param(e, *index)).unwrap_or_default()
            }
            Decl::SelfParam { name } => self.func_name_owner_type(name),
            Decl::NumericFor => Type::Number,
            Decl::GenericFor { stmt, index } => self.for_in_type(stmt, *index),
        }
    }

    fn local_stmt_type(&self, stmt: &Stmt, index: usize, top_level: bool) -> Type {
        let StmtKind::Local { names, exprs, in_unpack } = &stmt.kind else { return Type::Unknown };
        let doc = self.ctx.doc_at(stmt.span.start);
        if let Some(class) = doc.classes.last() {
            return Type::Named(class.name.clone(), Vec::new());
        }
        if let Some(ty) = &doc.ty {
            return ty.clone();
        }
        if *in_unpack {
            let base = exprs.first().map(|e| self.expr(e)).unwrap_or_default();
            return self.member(&base, &names[index].name.text).map(|m| m.ty).unwrap_or_default();
        }
        if let Some(expr) = exprs.get(index) {
            let is_last = index + 1 == exprs.len();
            if top_level && table_fields(expr).is_some() {
                return Type::GlobalTable(self.ctx.local_owner_key(names[index].name.span.start));
            }
            let ty =
                if is_last { self.expr_multi(expr).into_iter().next().unwrap_or_default() } else { self.expr(expr) };
            return ty.widen();
        }
        match exprs.last() {
            Some(last) if last.is_multi_value() => {
                self.expr_multi(last).into_iter().nth(index + 1 - exprs.len()).unwrap_or_default().widen()
            }
            _ => Type::Unknown,
        }
    }

    /// `lib.onCache('vehicle', function(value, oldValue)`: both parameters are `cache.vehicle`.
    fn on_cache_param(&self, expected: &Expected) -> Option<Type> {
        let ExprKind::Call { callee, args, .. } = &expected.call.kind else { return None };
        if callee.dotted_path().as_deref() != Some("lib.onCache") {
            return None;
        }
        let key = args.first()?.as_string()?;
        self.member(&self.global_type("cache"), key).map(|m| m.ty)
    }

    fn expected_param(&self, expected: &Expected, index: usize) -> Option<Type> {
        if let Some(ty) = self.on_cache_param(expected).filter(|_| index < 2) {
            return Some(ty);
        }
        let (fun, args, via_method) = match &expected.call.kind {
            ExprKind::Call { callee, args, .. } => (self.expr(callee).as_fun().cloned(), args, false),
            ExprKind::MethodCall { base, method, args, .. } => {
                let member = self.member(&self.expr(base), &method.text);
                (member.and_then(|m| m.ty.as_fun().cloned()), args, true)
            }
            _ => return None,
        };
        let fun = fun?;
        let (skip_params, skip_args) = fun.call_offsets(via_method);
        let param_index = (expected.arg_index + skip_params).checked_sub(skip_args)?;
        let callback = fun.params.get(param_index)?.ty.as_fun()?.clone();
        let ty = &callback.params.get(index)?.ty;
        Some(substitute(ty, &self.bind_generics(&fun, args, via_method, false)))
    }

    /// The type `self` has inside `function a.b:c()`, which is also the owner of `c`.
    pub fn func_name_owner_type(&self, name: &FuncName) -> Type {
        let mut ty = self.name(&name.base);
        for segment in &name.path {
            ty = self.member(&ty, &segment.text).map(|m| m.ty).unwrap_or_default();
        }
        ty
    }

    fn for_in_type(&self, stmt: &Stmt, index: usize) -> Type {
        let StmtKind::GenericFor { exprs, .. } = &stmt.kind else { return Type::Unknown };
        let Some(first) = exprs.first() else { return Type::Unknown };
        let iterated = match &first.kind {
            ExprKind::Call { callee, args, .. } => match (callee.dotted_path().as_deref(), args.first()) {
                (Some(iterator @ ("pairs" | "ipairs" | "next")), Some(arg)) => Some((arg, iterator == "ipairs")),
                _ => None,
            },
            // `for k, v in next, t`
            _ if first.dotted_path().as_deref() == Some("next") => exprs.get(1).map(|arg| (arg, false)),
            _ => None,
        };
        if let Some((arg, ipairs)) = iterated {
            let (key, value) = match &arg.unparen().kind {
                ExprKind::Table(fields) => self.inline_table_key_values(fields, ipairs),
                _ => self.key_value_types(&self.expr(arg), ipairs),
            };
            let key = if ipairs { Type::Integer } else { key };
            return if index == 0 {
                key
            } else if index == 1 {
                value
            } else {
                Type::Unknown
            };
        }
        self.expr(first).as_fun().and_then(|f| f.returns.get(index).cloned()).unwrap_or_default()
    }

    /// `pairs({ 'male', 'female' })`: nothing else can reach a table built in the call, so its keys
    /// and values keep their literal types instead of widening to `string`.
    fn inline_table_key_values(&self, fields: &[TableField], positional_only: bool) -> (Type, Type) {
        let mut keys = Vec::new();
        let mut values = Vec::new();
        for field in fields.iter().take(MAX_SHAPE_FIELDS) {
            let (key, value) = match field {
                TableField::Positional(value) => (Type::Integer, self.expr(value)),
                _ if positional_only => continue,
                TableField::Named { name, value } => (Type::StringLit(name.text.clone()), self.expr(value)),
                TableField::Keyed { key, value } => (self.expr(key), self.expr(value)),
                TableField::SetMember(name) => (Type::StringLit(name.text.clone()), Type::BooleanLit(true)),
            };
            keys.push(key);
            values.push(value);
        }
        (Type::union(keys), Type::union(values))
    }

    /// What `pairs` yields for a value of type `ty`, or with `array_only` what `ipairs` yields.
    pub fn key_value_types(&self, ty: &Type, array_only: bool) -> (Type, Type) {
        let resolved = self.resolve_alias(ty);
        let (keys, values): (Vec<Type>, Vec<Type>) = match resolved {
            Type::Array(inner) => return (Type::Integer, *inner),
            Type::Tuple(items) => return (Type::Integer, Type::union(items)),
            Type::Map(k, v) => return (*k, *v),
            Type::Shape(shape) => {
                let fields = shape.fields.iter().filter(|_| !array_only).map(|f| (Type::String, f.ty.clone()));
                fields.chain(shape.index.clone()).unzip()
            }
            Type::Named(ref name, _) => {
                let index = self.index.class(name).and_then(|(_, c)| c.index.clone());
                // An instance holds its data; the methods its class provides are not visited.
                let fields = self
                    .members(&resolved)
                    .into_iter()
                    .filter(|m| !array_only && !matches!(m.kind, SymbolKind::Method | SymbolKind::Function))
                    .map(|m| (Type::String, m.ty));
                let (keys, values): (Vec<Type>, Vec<Type>) = fields.chain(index).unzip();
                if keys.is_empty() {
                    return (Type::String, Type::Unknown);
                }
                (keys, values)
            }
            Type::GlobalTable(owner) => return self.global_table_key_values(&owner, array_only),
            Type::Union(types) => types
                .iter()
                .filter(|t| !matches!(t, Type::Nil))
                .map(|t| self.guarded(|| self.key_value_types(t, array_only)))
                .unzip(),
            _ => return (Type::Unknown, Type::Unknown),
        };
        (Type::union(keys), Type::union(values))
    }

    /// The index holds the named fields of a table. Its array part and other keys are only in the
    /// constructor, which is still at hand for a top-level local of this file.
    fn global_table_key_values(&self, owner: &SmolStr, array_only: bool) -> (Type, Type) {
        let mut keys = Vec::new();
        let mut values = Vec::new();
        if !array_only {
            for member in self.members(&Type::GlobalTable(owner.clone())) {
                keys.push(Type::String);
                values.push(member.ty);
            }
        }
        for field in self.local_table_fields(owner).unwrap_or_default().iter().take(MAX_SHAPE_FIELDS) {
            match field {
                TableField::Positional(value) => {
                    keys.push(Type::Integer);
                    values.push(self.expr(value).widen());
                }
                TableField::Keyed { key, value } if !array_only && !matches!(key.kind, ExprKind::String(_)) => {
                    keys.push(self.expr(key).widen());
                    values.push(self.expr(value).widen());
                }
                _ => {}
            }
        }
        if keys.is_empty() {
            return (Type::String, Type::Unknown);
        }
        (Type::union(keys), Type::union(values))
    }

    fn local_table_fields(&self, owner: &str) -> Option<&'a [TableField]> {
        let Decl::Local { stmt, index } = self.ctx.decl(self.ctx.local_owner_decl(owner)?)? else { return None };
        let StmtKind::Local { exprs, .. } = &stmt.kind else { return None };
        exprs.get(*index).and_then(table_fields)
    }

    pub fn resolve_alias(&self, ty: &Type) -> Type {
        let mut current = ty.clone();
        for _ in 0..8 {
            match &current {
                Type::Named(name, _) if self.index.class(name).is_none() => match self.index.alias(name) {
                    Some((_, alias)) => current = alias.ty.clone(),
                    None => break,
                },
                Type::Require(path) => match self.module_type(path) {
                    Some(ty) => current = ty,
                    None => break,
                },
                _ => break,
            }
        }
        current
    }

    fn module_type(&self, path: &str) -> Option<Type> {
        let file = self.index.resolve_require(path, self.ctx.file)?;
        self.index.file(file)?.index.module_return.clone()
    }

    fn index_expr(&self, base: &Expr, index: &Expr) -> Type {
        let base_ty = self.expr(base);
        let key_ty = self.expr(index);
        let mut keys = Vec::new();
        if self.literal_keys(&key_ty, 0, &mut keys) && !keys.is_empty() {
            let fields: Option<Vec<Type>> = keys.iter().map(|key| self.member(&base_ty, key).map(|m| m.ty)).collect();
            if let Some(fields) = fields {
                return Type::union(fields);
            }
        }
        match self.resolve_alias(&base_ty.without_nil()) {
            Type::Array(inner) => *inner,
            Type::Map(_, value) => *value,
            Type::Tuple(items) => match &index.kind {
                ExprKind::Number(NumberValue::Int(i)) => {
                    items.get((*i as usize).wrapping_sub(1)).cloned().unwrap_or_default()
                }
                _ => Type::union(items),
            },
            Type::Shape(shape) => shape.index.as_ref().map(|(_, v)| v.clone()).unwrap_or_default(),
            Type::Named(name, _) => {
                self.index.class(&name).and_then(|(_, c)| c.index.as_ref().map(|(_, v)| v.clone())).unwrap_or_default()
            }
            // `list[i]` on a top-level `local list = { ... }`, whose array part is not in the index.
            Type::GlobalTable(owner) if matches!(key_ty.widen(), Type::Integer | Type::Number) => {
                self.global_table_key_values(&owner, true).1
            }
            Type::String | Type::StringLit(_) => Type::Unknown,
            _ => Type::Unknown,
        }
    }

    /// The field names a key can be: `'male'`, or every name of a `"male"|"female"` loop variable or
    /// alias. False when any part of the key is not a string literal.
    fn literal_keys(&self, key: &Type, depth: u32, out: &mut Vec<SmolStr>) -> bool {
        if depth > 8 {
            return false;
        }
        match self.resolve_alias(key) {
            Type::StringLit(name) => {
                out.push(name);
                true
            }
            Type::Union(types) => {
                types.iter().filter(|t| !matches!(t, Type::Nil)).all(|t| self.literal_keys(t, depth + 1, out))
            }
            _ => false,
        }
    }

    fn table(&self, fields: &[TableField]) -> Type {
        let mut shape = Shape::default();
        let mut positional = Vec::new();
        let (mut keys, mut values) = (Vec::new(), Vec::new());
        for field in fields.iter().take(MAX_SHAPE_FIELDS) {
            match field {
                TableField::Named { name, value } => shape.fields.push(ShapeField {
                    name: name.text.clone(),
                    ty: self.expr(value).widen(),
                    optional: false,
                }),
                TableField::Keyed { key, value } => match &key.kind {
                    ExprKind::String(name) => shape.fields.push(ShapeField {
                        name: name.clone(),
                        ty: self.expr(value).widen(),
                        optional: false,
                    }),
                    _ => {
                        keys.push(self.expr(key).widen());
                        values.push(self.expr(value).widen());
                    }
                },
                TableField::SetMember(name) => {
                    shape.fields.push(ShapeField { name: name.text.clone(), ty: Type::Boolean, optional: false })
                }
                TableField::Positional(value) => positional.push(self.expr(value).widen()),
            }
        }
        if shape.fields.is_empty() && keys.is_empty() && !positional.is_empty() {
            return Type::Array(Box::new(Type::union(positional)));
        }
        if fields.is_empty() {
            return Type::Table;
        }
        // A mixed table keeps its array part and every `[key]` next to the named fields.
        if !positional.is_empty() {
            keys.push(Type::Integer);
            values.extend(positional);
        }
        if !keys.is_empty() {
            shape.index = Some((Type::union(keys), Type::union(values)));
        }
        Type::Shape(Arc::new(shape))
    }

    fn binary(&self, op: BinOp, lhs: &Expr, rhs: &Expr) -> Type {
        match op {
            BinOp::Concat => Type::String,
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => Type::Boolean,
            BinOp::And => Type::union([self.expr(rhs).widen(), Type::BooleanLit(false)]).widen(),
            BinOp::Or => {
                let left = self.expr(lhs).without_nil().widen();
                let left = match left {
                    Type::Boolean => Type::Unknown,
                    other => other,
                };
                Type::union([left, self.expr(rhs).widen()])
            }
            BinOp::BAnd | BinOp::BOr | BinOp::BXor | BinOp::Shl | BinOp::Shr | BinOp::IDiv => Type::Integer,
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod | BinOp::Pow => {
                let (left, right) = (self.expr(lhs).widen(), self.expr(rhs).widen());
                let is_vector = |t: &Type| matches!(t, Type::Named(n, _) if n.starts_with("vector") || n == "quat");
                if is_vector(&left) {
                    left
                } else if is_vector(&right) {
                    right
                } else if left == Type::Integer && right == Type::Integer && !matches!(op, BinOp::Div | BinOp::Pow) {
                    Type::Integer
                } else {
                    Type::Number
                }
            }
        }
    }

    /// The callee's function type, taking `base:method` lookups into account.
    pub fn callee_fun(&self, base: &Expr, method: Option<&Name>) -> Option<(Arc<FunType>, Option<MemberInfo>)> {
        match method {
            Some(method) => {
                let member = self.member(&self.expr(base), &method.text)?;
                Some((member.ty.as_fun()?.clone(), Some(member)))
            }
            None => {
                let ty = self.expr(base);
                if let Some(fun) = ty.as_fun() {
                    return Some((fun.clone(), None));
                }
                match self.resolve_alias(&ty) {
                    Type::Named(name, _) => {
                        self.index.class(&name).and_then(|(_, c)| c.call.clone()).map(|f| (f, None))
                    }
                    _ => None,
                }
            }
        }
    }

    fn call(&self, base: &Expr, method: Option<&Name>, args: &[Expr]) -> Vec<Type> {
        if method.is_none() {
            match (base.dotted_path().as_deref(), args.first()) {
                (Some("require" | "lib.require" | "lib.load"), Some(arg)) => {
                    if let Some(path) = arg.as_string() {
                        // `require 'glm'` returns the built-in library, not a file of the resource.
                        if path == "glm" {
                            return vec![self.global_type("glm")];
                        }
                        return vec![Type::Require(path.clone())];
                    }
                }
                (Some("setmetatable"), Some(arg)) => return vec![self.expr(arg)],
                (Some("tostring"), _) => return vec![Type::String],
                (Some("tonumber"), _) => return vec![Type::Number.optional()],
                _ => {}
            }
        }
        let Some((fun, _)) = self.callee_fun(base, method) else { return Vec::new() };
        let generics = self.bind_generics(&fun, args, method.is_some(), true);
        fun.returns.iter().map(|ret| substitute(ret, &generics)).collect()
    }

    /// Binds the generics of `fun` from the arguments of a call. Function literals go last, and only
    /// `with_callbacks`: their parameters are typed from what the other arguments bound, and their
    /// returns bind the rest, as `RV` and `RK` in `fun(value: V, key: K): RV, RK`.
    fn bind_generics(
        &self,
        fun: &FunType,
        args: &[Expr],
        via_method: bool,
        with_callbacks: bool,
    ) -> Vec<(SmolStr, Type)> {
        let mut bound = Vec::new();
        let (skip_params, skip_args) = fun.call_offsets(via_method);
        let pairs: Vec<(&Param, &Expr)> =
            fun.params.iter().skip(skip_params).zip(args.iter().skip(skip_args)).collect();
        let is_callback = |arg: &Expr| matches!(arg.unparen().kind, ExprKind::Function(_));
        for (param, arg) in pairs.iter().filter(|(_, arg)| !is_callback(arg)) {
            self.unify(fun, &param.ty, &self.expr(arg).widen(), &mut bound, 0);
        }
        if with_callbacks {
            for (param, arg) in pairs.iter().filter(|(_, arg)| is_callback(arg)) {
                self.unify(fun, &param.ty, &self.expr(arg), &mut bound, 0);
            }
        }
        // A declared generic that no argument decides is unknown, not a type named `RV`.
        for name in &fun.generics {
            if !bound.iter().any(|(n, _)| n == name) {
                bound.push((name.clone(), Type::Unknown));
            }
        }
        bound
    }

    /// Matches a parameter type against the type of its argument, binding the generics in it.
    fn unify(&self, fun: &FunType, param: &Type, arg: &Type, bound: &mut Vec<(SmolStr, Type)>, depth: u32) {
        if depth > 8 || arg.is_unknown() {
            return;
        }
        match param {
            Type::Named(name, args) if args.is_empty() && self.is_generic(fun, name) => {
                if !bound.iter().any(|(n, _)| n == name) {
                    bound.push((name.clone(), arg.clone()));
                }
            }
            Type::Array(inner) => self.unify(fun, inner, &self.key_value_types(arg, true).1, bound, depth + 1),
            Type::Map(key, value) => {
                let (arg_key, arg_value) = self.key_value_types(arg, false);
                self.unify(fun, key, &arg_key, bound, depth + 1);
                self.unify(fun, value, &arg_value, bound, depth + 1);
            }
            Type::Union(types) => {
                for part in types.iter().filter(|t| !matches!(t, Type::Nil)) {
                    self.unify(fun, part, &arg.without_nil(), bound, depth + 1);
                }
            }
            Type::Fun(expected) => {
                if let Some(given) = arg.as_fun() {
                    for (want, got) in expected.returns.iter().zip(&given.returns) {
                        self.unify(fun, want, got, bound, depth + 1);
                    }
                }
            }
            _ => {}
        }
    }

    /// A name `fun` declares with `@generic`, or a short one such as `T` that is neither a class nor
    /// an alias.
    fn is_generic(&self, fun: &FunType, name: &str) -> bool {
        fun.generics.iter().any(|g| g == name)
            || (name.len() <= 2 && self.index.class(name).is_none() && self.index.alias(name).is_none())
    }

    /// Builds the type of a function literal from its doc comment, inferring returns when undocumented.
    pub fn fun_type(&self, func: &FuncBody, doc_anchor: Option<u32>, is_method: bool) -> FunType {
        let names: Vec<SmolStr> = func.params.iter().map(|p| p.text.clone()).collect();
        let doc = doc_anchor.map(|anchor| self.ctx.doc_at(anchor));
        let mut fun = match &doc {
            Some(doc) => doc.fun_type(&names, func.vararg.is_some(), is_method),
            None => DocGroup::default().fun_type(&names, func.vararg.is_some(), is_method),
        };
        if fun.returns.is_empty() {
            if let Some(exprs) = first_return(&func.body) {
                let mut returns: Vec<Type> = Vec::new();
                for (i, expr) in exprs.iter().enumerate() {
                    if i + 1 == exprs.len() {
                        returns.extend(self.expr_multi(expr).into_iter().map(|t| t.widen()));
                    } else {
                        returns.push(self.expr(expr).widen());
                    }
                }
                if returns.iter().any(|t| !t.is_unknown()) {
                    fun.returns = returns;
                }
            }
        }
        fun
    }

    pub fn member(&self, ty: &Type, name: &str) -> Option<MemberInfo> {
        self.guarded(|| {
            let mut found = self.members_matching(ty, Some(name));
            let best = (0..found.len()).max_by_key(|i| (found[*i].ty.specificity(), std::cmp::Reverse(*i)))?;
            Some(found.swap_remove(best))
        })
    }

    pub fn members(&self, ty: &Type) -> Vec<MemberInfo> {
        let mut members = self.guarded(|| self.members_matching(ty, None));
        let mut seen = FxHashSet::default();
        members.retain(|m| seen.insert(m.name.clone()));
        members
    }

    fn members_matching(&self, ty: &Type, filter: Option<&str>) -> Vec<MemberInfo> {
        let wanted = |name: &str| filter.is_none_or(|f| f == name);
        let mut out = Vec::new();
        match ty {
            Type::Union(types) => {
                for part in types.iter().filter(|t| !matches!(t, Type::Nil)) {
                    out.extend(self.guarded(|| self.members_matching(part, filter)));
                }
            }
            Type::Shape(shape) => {
                for field in shape.fields.iter().filter(|f| wanted(&f.name)) {
                    out.push(MemberInfo {
                        name: field.name.clone(),
                        ty: if field.optional { field.ty.clone().optional() } else { field.ty.clone() },
                        doc: None,
                        deprecated: false,
                        literal: None,
                        kind: SymbolKind::Field,
                        location: None,
                    });
                }
            }
            Type::Named(name, _) => self.class_members(name, filter, &mut out, 0),
            Type::GlobalTable(owner) => self.owner_members(owner, filter, &mut out),
            Type::String | Type::StringLit(_) => {
                let library = self.global_type("string");
                if !matches!(library, Type::String | Type::StringLit(_)) {
                    out.extend(self.guarded(|| self.members_matching(&library, filter)));
                }
            }
            Type::Require(_) => {
                let resolved = self.resolve_alias(ty);
                if resolved != *ty {
                    out.extend(self.guarded(|| self.members_matching(&resolved, filter)));
                }
            }
            Type::Exports(None) => {
                for resource in self.index.resources.iter().filter(|r| wanted(&r.name)) {
                    out.push(MemberInfo {
                        name: resource.name.clone(),
                        ty: Type::Exports(Some(resource.name.clone())),
                        doc: Some(Arc::from(format!("Exports of the `{}` resource.", resource.name))),
                        deprecated: false,
                        literal: None,
                        kind: SymbolKind::Table,
                        location: None,
                    });
                }
                if let Some(name) = filter.filter(|_| out.is_empty()) {
                    out.push(MemberInfo {
                        name: SmolStr::new(name),
                        ty: Type::Exports(Some(SmolStr::new(name))),
                        doc: None,
                        deprecated: false,
                        literal: None,
                        kind: SymbolKind::Table,
                        location: None,
                    });
                }
            }
            Type::Exports(Some(resource)) => {
                for (file, symbol) in self.index.exports_of(resource) {
                    if wanted(&symbol.name) {
                        out.push(member_from_symbol(file, symbol));
                    }
                }
            }
            _ => {}
        }
        out
    }

    fn owner_members(&self, owner: &str, filter: Option<&str>, out: &mut Vec<MemberInfo>) {
        for (file, symbol) in self.index.members_of(owner, self.ctx.file) {
            if filter.is_none_or(|f| f == symbol.name) {
                let mut member = member_from_symbol(file, symbol);
                if matches!(member.ty, Type::Table | Type::Unknown) {
                    let nested = format!("{owner}.{}", symbol.name);
                    if self.index.has_members(&nested) {
                        member.ty = Type::GlobalTable(SmolStr::new(nested));
                    }
                }
                out.push(member);
            }
        }
        if let Some(name) = filter {
            let nested = format!("{owner}.{name}");
            if out.is_empty() && self.index.has_members(&nested) {
                out.push(MemberInfo {
                    name: SmolStr::new(name),
                    ty: Type::GlobalTable(SmolStr::new(nested)),
                    doc: None,
                    deprecated: false,
                    literal: None,
                    kind: SymbolKind::Table,
                    location: None,
                });
            }
        }
    }

    fn class_members(&self, name: &str, filter: Option<&str>, out: &mut Vec<MemberInfo>, depth: u32) {
        if depth > 8 {
            return;
        }
        let defs = self.index.class_defs(name);
        if defs.is_empty() {
            if let Some((_, alias)) = self.index.alias(name) {
                out.extend(self.guarded(|| self.members_matching(&alias.ty, filter)));
            }
            return;
        }
        for (file, class) in &defs {
            for field in class.fields.iter().filter(|f| filter.is_none_or(|n| n == f.name)) {
                out.push(member_from_symbol(*file, field));
            }
        }
        self.owner_members(name, filter, out);
        for (_, class) in defs {
            for parent in &class.parents {
                self.class_members(parent, filter, out, depth + 1);
            }
        }
    }
}

fn member_from_symbol(file: FileId, symbol: &crate::index::Symbol) -> MemberInfo {
    MemberInfo {
        name: symbol.name.clone(),
        ty: symbol.ty.clone(),
        doc: symbol.doc.clone(),
        deprecated: symbol.deprecated,
        literal: symbol.literal.clone(),
        kind: symbol.kind,
        location: Some((file, symbol.range)),
    }
}

fn substitute(ty: &Type, generics: &[(SmolStr, Type)]) -> Type {
    if generics.is_empty() {
        return ty.clone();
    }
    match ty {
        Type::Named(name, args) if args.is_empty() => {
            generics.iter().find(|(n, _)| n == name).map_or_else(|| ty.clone(), |(_, bound)| bound.clone())
        }
        Type::Named(name, args) => Type::Named(name.clone(), args.iter().map(|t| substitute(t, generics)).collect()),
        Type::Array(inner) => Type::Array(Box::new(substitute(inner, generics))),
        Type::Tuple(items) => Type::Tuple(items.iter().map(|t| substitute(t, generics)).collect()),
        Type::Variadic(inner) => Type::Variadic(Box::new(substitute(inner, generics))),
        // `V?` with `V` unbound is unknown, not `nil`.
        Type::Union(types) => {
            let parts: Vec<Type> = types.iter().map(|t| substitute(t, generics)).collect();
            if parts.iter().any(Type::is_unknown) {
                Type::Unknown
            } else {
                Type::union(parts)
            }
        }
        Type::Map(k, v) => Type::Map(Box::new(substitute(k, generics)), Box::new(substitute(v, generics))),
        Type::Fun(fun) => Type::Fun(Arc::new(FunType {
            params: fun.params.iter().map(|p| Param { ty: substitute(&p.ty, generics), ..p.clone() }).collect(),
            returns: fun.returns.iter().map(|t| substitute(t, generics)).collect(),
            is_method: fun.is_method,
            generics: Vec::new(),
        })),
        other => other.clone(),
    }
}

/// The constructor behind a table-valued initialiser, looking through `setmetatable({...}, mt)`.
pub fn table_fields(expr: &Expr) -> Option<&[TableField]> {
    match &expr.unparen().kind {
        ExprKind::Table(fields) => Some(fields),
        ExprKind::Call { callee, args, .. } if callee.dotted_path().as_deref() == Some("setmetatable") => {
            args.first().and_then(table_fields)
        }
        _ => None,
    }
}

/// The expressions of the first `return` that belongs to this function body itself.
fn first_return(block: &Block) -> Option<&[Expr]> {
    for stmt in &block.stmts {
        let found = match &stmt.kind {
            StmtKind::Return(exprs) if !exprs.is_empty() => Some(exprs.as_slice()),
            StmtKind::Do(body) | StmtKind::While { body, .. } | StmtKind::Repeat { body, .. } => first_return(body),
            StmtKind::NumericFor { body, .. } | StmtKind::GenericFor { body, .. } => first_return(body),
            StmtKind::If { branches, else_block } => branches
                .iter()
                .find_map(|b| first_return(&b.block))
                .or_else(|| else_block.as_ref().and_then(first_return)),
            _ => None,
        };
        if found.is_some() {
            return found;
        }
    }
    None
}
