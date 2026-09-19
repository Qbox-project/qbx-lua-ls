use std::fmt;
use std::sync::Arc;

use smol_str::SmolStr;

#[derive(Clone, Debug, Default, PartialEq)]
pub enum Type {
    #[default]
    Unknown,
    Any,
    Nil,
    Boolean,
    Number,
    Integer,
    String,
    Table,
    Function,
    Thread,
    Userdata,
    BooleanLit(bool),
    StringLit(SmolStr),
    IntLit(i64),
    Named(SmolStr, Vec<Type>),
    Array(Box<Type>),
    Map(Box<Type>, Box<Type>),
    Tuple(Vec<Type>),
    Union(Vec<Type>),
    Fun(Arc<FunType>),
    Shape(Arc<Shape>),
    Variadic(Box<Type>),
    /// A global table addressed by its dotted path, e.g. `lib.callback`; members live in the index.
    GlobalTable(SmolStr),
    /// The `exports` object, or one resource of it once indexed (`exports.qbx_core`).
    Exports(Option<SmolStr>),
    /// The value returned by the module a `require` call names; resolved through the index on use.
    Require(SmolStr),
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Param {
    pub name: SmolStr,
    pub ty: Type,
    pub optional: bool,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct FunType {
    pub params: Vec<Param>,
    pub returns: Vec<Type>,
    pub is_method: bool,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct ShapeField {
    pub name: SmolStr,
    pub ty: Type,
    pub optional: bool,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Shape {
    pub fields: Vec<ShapeField>,
    pub index: Option<(Type, Type)>,
}

impl Type {
    pub fn named(name: &str) -> Type {
        match name {
            "any" => Type::Any,
            "nil" | "void" => Type::Nil,
            "boolean" | "bool" => Type::Boolean,
            "number" | "float" => Type::Number,
            "integer" | "int" => Type::Integer,
            "string" => Type::String,
            "table" => Type::Table,
            "function" => Type::Function,
            "thread" => Type::Thread,
            "userdata" | "lightuserdata" => Type::Userdata,
            "true" => Type::BooleanLit(true),
            "false" => Type::BooleanLit(false),
            "unknown" => Type::Unknown,
            _ => Type::Named(SmolStr::new(name), Vec::new()),
        }
    }

    /// How much a type tells us, used to pick the best of several declarations of one name.
    pub fn specificity(&self) -> u8 {
        match self {
            Type::Unknown => 0,
            Type::Any => 1,
            Type::Table | Type::Function | Type::Nil => 2,
            Type::Union(types) => types.iter().map(Type::specificity).max().unwrap_or(0),
            Type::Boolean | Type::Number | Type::Integer | Type::String | Type::Thread | Type::Userdata => 3,
            Type::BooleanLit(_) | Type::StringLit(_) | Type::IntLit(_) => 3,
            _ => 4,
        }
    }

    pub fn is_unknown(&self) -> bool {
        matches!(self, Type::Unknown)
    }

    pub fn union(types: impl IntoIterator<Item = Type>) -> Type {
        let mut flat: Vec<Type> = Vec::new();
        for ty in types {
            match ty {
                Type::Union(inner) => inner.into_iter().for_each(|t| push_unique(&mut flat, t)),
                other => push_unique(&mut flat, other),
            }
        }
        if flat.iter().any(|t| matches!(t, Type::Any)) {
            return Type::Any;
        }
        if flat.len() > 1 {
            flat.retain(|t| !t.is_unknown());
        }
        match flat.len() {
            0 => Type::Unknown,
            1 => flat.pop().unwrap_or_default(),
            _ => Type::Union(flat),
        }
    }

    pub fn optional(self) -> Type {
        Type::union([self, Type::Nil])
    }

    pub fn without_nil(&self) -> Type {
        match self {
            Type::Union(types) => Type::union(types.iter().filter(|t| !matches!(t, Type::Nil)).cloned()),
            other => other.clone(),
        }
    }

    /// Widens literal types the way a variable initialised with a literal should be shown.
    pub fn widen(&self) -> Type {
        match self {
            Type::BooleanLit(_) => Type::Boolean,
            Type::StringLit(_) => Type::String,
            Type::IntLit(_) => Type::Integer,
            Type::Union(types) => Type::union(types.iter().map(Type::widen)),
            other => other.clone(),
        }
    }

    pub fn as_fun(&self) -> Option<&Arc<FunType>> {
        match self {
            Type::Fun(fun) => Some(fun),
            Type::Union(types) => types.iter().find_map(Type::as_fun),
            _ => None,
        }
    }

    pub fn first_return(&self) -> Type {
        self.as_fun().and_then(|f| f.returns.first().cloned()).unwrap_or_default()
    }
}

fn push_unique(list: &mut Vec<Type>, ty: Type) {
    if !list.contains(&ty) {
        list.push(ty);
    }
}

impl FunType {
    /// How a call lines up with `params`, as `(parameters to skip, arguments to skip)`. Functions
    /// declared with `:` do not list `self`, while `fun(self, ...)` fields do.
    pub fn call_offsets(&self, via_colon: bool) -> (usize, usize) {
        let explicit_self = self.params.first().is_some_and(|p| p.name == "self");
        match (via_colon, self.is_method) {
            (true, false) if explicit_self => (1, 0),
            (false, true) => (0, 1),
            _ => (0, 0),
        }
    }

    pub fn signature(&self, name: &str) -> String {
        let params: Vec<String> = self.params.iter().map(Param::to_string).collect();
        let mut out = format!("function {name}({})", params.join(", "));
        if !self.returns.is_empty() {
            let returns: Vec<String> = self.returns.iter().map(Type::to_string).collect();
            out.push_str(": ");
            out.push_str(&returns.join(", "));
        }
        out
    }
}

impl fmt::Display for Param {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let optional = if self.optional { "?" } else { "" };
        if self.ty.is_unknown() {
            write!(f, "{}{optional}", self.name)
        } else {
            write!(f, "{}{optional}: {}", self.name, self.ty)
        }
    }
}

impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Type::Unknown => f.write_str("unknown"),
            Type::Any => f.write_str("any"),
            Type::Nil => f.write_str("nil"),
            Type::Boolean => f.write_str("boolean"),
            Type::Number => f.write_str("number"),
            Type::Integer => f.write_str("integer"),
            Type::String => f.write_str("string"),
            Type::Table => f.write_str("table"),
            Type::Function => f.write_str("function"),
            Type::Thread => f.write_str("thread"),
            Type::Userdata => f.write_str("userdata"),
            Type::BooleanLit(b) => write!(f, "{b}"),
            Type::StringLit(s) => write!(f, "\"{s}\""),
            Type::IntLit(i) => write!(f, "{i}"),
            Type::Named(name, args) if args.is_empty() => f.write_str(name),
            Type::Named(name, args) => {
                let args: Vec<String> = args.iter().map(Type::to_string).collect();
                write!(f, "{name}<{}>", args.join(", "))
            }
            Type::Array(inner) => match **inner {
                Type::Union(_) | Type::Fun(_) => write!(f, "({inner})[]"),
                _ => write!(f, "{inner}[]"),
            },
            Type::Map(k, v) => write!(f, "table<{k}, {v}>"),
            Type::Tuple(items) => {
                let items: Vec<String> = items.iter().map(Type::to_string).collect();
                write!(f, "[{}]", items.join(", "))
            }
            Type::Union(types) => {
                let has_nil = types.iter().any(|t| matches!(t, Type::Nil));
                let rest: Vec<String> = types
                    .iter()
                    .filter(|t| !matches!(t, Type::Nil))
                    .filter(|t| !matches!(t, Type::GlobalTable(owner) if owner.starts_with('%')) || types.len() == 1)
                    .map(|t| if matches!(t, Type::Fun(_)) { format!("({t})") } else { t.to_string() })
                    .collect();
                match (has_nil, rest.len()) {
                    (true, 1) => write!(f, "{}?", rest[0]),
                    (true, _) => write!(f, "{}|nil", rest.join("|")),
                    (false, _) => f.write_str(&rest.join("|")),
                }
            }
            Type::Fun(fun) => {
                let params: Vec<String> = fun.params.iter().map(Param::to_string).collect();
                write!(f, "fun({})", params.join(", "))?;
                if !fun.returns.is_empty() {
                    let returns: Vec<String> = fun.returns.iter().map(Type::to_string).collect();
                    write!(f, ": {}", returns.join(", "))?;
                }
                Ok(())
            }
            Type::Shape(shape) => {
                if shape.fields.is_empty() && shape.index.is_none() {
                    return f.write_str("table");
                }
                let mut parts: Vec<String> = shape
                    .fields
                    .iter()
                    .take(8)
                    .map(|field| format!("{}{}: {}", field.name, if field.optional { "?" } else { "" }, field.ty))
                    .collect();
                if let Some((k, v)) = &shape.index {
                    parts.push(format!("[{k}]: {v}"));
                }
                if shape.fields.len() > 8 {
                    parts.push("...".into());
                }
                write!(f, "{{ {} }}", parts.join(", "))
            }
            Type::Variadic(inner) => write!(f, "...{inner}"),
            Type::GlobalTable(path) if path.starts_with('%') => f.write_str("table"),
            Type::GlobalTable(path) => f.write_str(path),
            Type::Exports(None) => f.write_str("exports"),
            Type::Exports(Some(resource)) => write!(f, "exports.{resource}"),
            Type::Require(path) => write!(f, "module \"{path}\""),
        }
    }
}

pub struct TypeParser<'a> {
    src: &'a str,
    pos: usize,
    depth: u32,
}

impl<'a> TypeParser<'a> {
    pub fn new(src: &'a str) -> Self {
        Self { src, pos: 0, depth: 0 }
    }

    pub fn rest(&self) -> &'a str {
        &self.src[self.pos.min(self.src.len())..]
    }

    pub fn position(&self) -> usize {
        self.pos
    }

    fn bytes(&self) -> &'a [u8] {
        self.src.as_bytes()
    }

    fn peek(&self) -> u8 {
        self.bytes().get(self.pos).copied().unwrap_or(0)
    }

    pub fn skip_ws(&mut self) {
        while matches!(self.peek(), b' ' | b'\t') {
            self.pos += 1;
        }
    }

    fn eat(&mut self, c: u8) -> bool {
        self.skip_ws();
        if self.peek() == c {
            self.pos += 1;
            return true;
        }
        false
    }

    pub fn ident(&mut self) -> Option<&'a str> {
        self.skip_ws();
        let start = self.pos;
        while matches!(self.peek(), b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'*')
            && !self.rest().starts_with("...")
        {
            self.pos += 1;
        }
        (self.pos > start).then(|| &self.src[start..self.pos])
    }

    pub fn parse(&mut self) -> Type {
        self.depth += 1;
        if self.depth > 32 {
            self.pos = self.src.len();
            return Type::Unknown;
        }
        let mut types = vec![self.postfix()];
        loop {
            self.skip_ws();
            if self.peek() == b'|' {
                self.pos += 1;
                types.push(self.postfix());
            } else {
                break;
            }
        }
        self.depth -= 1;
        Type::union(types)
    }

    /// Comma separated types, as used by `@return` and function returns.
    pub fn parse_list(&mut self) -> Vec<Type> {
        let mut types = vec![self.parse()];
        while self.eat(b',') {
            types.push(self.parse());
        }
        types
    }

    fn postfix(&mut self) -> Type {
        let mut ty = self.primary();
        loop {
            if self.rest().starts_with("[]") {
                self.pos += 2;
                ty = Type::Array(Box::new(ty));
            } else if self.peek() == b'?' {
                self.pos += 1;
                ty = ty.optional();
            } else {
                return ty;
            }
        }
    }

    fn primary(&mut self) -> Type {
        self.skip_ws();
        match self.peek() {
            b'(' => {
                self.pos += 1;
                let inner = self.parse();
                self.eat(b')');
                inner
            }
            b'{' => self.shape(),
            b'[' => {
                self.pos += 1;
                let mut items = Vec::new();
                while !self.eat(b']') && self.pos < self.src.len() {
                    items.push(self.parse());
                    self.eat(b',');
                }
                Type::Tuple(items)
            }
            quote @ (b'"' | b'\'') => {
                self.pos += 1;
                let start = self.pos;
                while self.pos < self.src.len() && self.peek() != quote {
                    self.pos += 1;
                }
                let value = SmolStr::new(&self.src[start..self.pos]);
                self.pos = (self.pos + 1).min(self.src.len());
                Type::StringLit(value)
            }
            b'`' => {
                self.pos += 1;
                let name = self.ident().unwrap_or("T");
                self.eat(b'`');
                Type::Named(SmolStr::new(name), Vec::new())
            }
            b'-' | b'0'..=b'9' => {
                let start = self.pos;
                self.pos += 1;
                while self.peek().is_ascii_digit() {
                    self.pos += 1;
                }
                self.src[start..self.pos].parse().map(Type::IntLit).unwrap_or(Type::Integer)
            }
            _ if self.rest().starts_with("...") => {
                self.pos += 3;
                let inner = if self.ident_follows() { self.postfix() } else { Type::Any };
                Type::Variadic(Box::new(inner))
            }
            _ => self.named(),
        }
    }

    fn ident_follows(&self) -> bool {
        matches!(self.peek(), b'a'..=b'z' | b'A'..=b'Z' | b'_' | b'{' | b'(')
    }

    fn named(&mut self) -> Type {
        let Some(name) = self.ident() else {
            return Type::Unknown;
        };
        if name == "fun" && self.peek() == b'(' {
            return self.fun();
        }
        if self.peek() != b'<' {
            return Type::named(name);
        }
        self.pos += 1;
        let mut args = Vec::new();
        while !self.eat(b'>') && self.pos < self.src.len() {
            args.push(self.parse());
            self.eat(b',');
        }
        match (name, args.len()) {
            ("table", 2) => {
                let value = args.pop().unwrap_or_default();
                let key = args.pop().unwrap_or_default();
                Type::Map(Box::new(key), Box::new(value))
            }
            ("table", 1) => Type::Array(Box::new(args.pop().unwrap_or_default())),
            _ => Type::Named(SmolStr::new(name), args),
        }
    }

    fn fun(&mut self) -> Type {
        self.pos += 1;
        let mut params = Vec::new();
        loop {
            self.skip_ws();
            if self.eat(b')') || self.pos >= self.src.len() {
                break;
            }
            if self.rest().starts_with("...") {
                self.pos += 3;
                let ty = if self.eat(b':') { self.parse() } else { Type::Any };
                params.push(Param { name: "...".into(), ty, optional: false });
            } else if let Some(name) = self.ident() {
                let optional = self.eat(b'?');
                let ty = if self.eat(b':') { self.parse() } else { Type::Unknown };
                params.push(Param { name: SmolStr::new(name), ty, optional });
            } else {
                self.pos += 1;
            }
            self.eat(b',');
        }
        let returns = if self.eat(b':') { self.parse_return_list() } else { Vec::new() };
        Type::Fun(Arc::new(FunType { params, returns, is_method: false }))
    }

    fn parse_return_list(&mut self) -> Vec<Type> {
        let mut types = vec![self.parse()];
        loop {
            let checkpoint = self.pos;
            if !self.eat(b',') {
                break;
            }
            self.skip_ws();
            let before = self.pos;
            let ty = self.parse();
            if self.pos == before || ty.is_unknown() {
                self.pos = checkpoint;
                break;
            }
            types.push(ty);
        }
        types
    }

    fn shape(&mut self) -> Type {
        self.pos += 1;
        let mut shape = Shape::default();
        loop {
            self.skip_ws();
            if self.eat(b'}') || self.pos >= self.src.len() {
                break;
            }
            if self.eat(b'[') {
                let key = self.parse();
                self.eat(b']');
                self.eat(b':');
                let value = self.parse();
                shape.index = Some((key, value));
            } else if let Some(name) = self.ident() {
                let optional = self.eat(b'?');
                let ty = if self.eat(b':') { self.parse() } else { Type::Unknown };
                shape.fields.push(ShapeField { name: SmolStr::new(name), ty, optional });
            } else {
                self.pos += 1;
            }
            if !self.eat(b',') {
                self.eat(b';');
            }
        }
        Type::Shape(Arc::new(shape))
    }
}

pub fn parse_type(text: &str) -> Type {
    TypeParser::new(text).parse()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(text: &str) -> String {
        parse_type(text).to_string()
    }

    #[test]
    fn parses_common_luacats_types() {
        assert_eq!(roundtrip("string"), "string");
        assert_eq!(roundtrip("string|number"), "string|number");
        assert_eq!(roundtrip("string?"), "string?");
        assert_eq!(roundtrip("number[]"), "number[]");
        assert_eq!(roundtrip("(string|number)[]"), "(string|number)[]");
        assert_eq!(roundtrip("table<string, Player>"), "table<string, Player>");
        assert_eq!(roundtrip("table<Player>"), "Player[]");
        assert_eq!(roundtrip("fun(a: string, b?: number): boolean, string"), "fun(a: string, b?: number): boolean, string");
        assert_eq!(roundtrip("fun(...: any)"), "fun(...: any)");
        assert_eq!(roundtrip("{ name: string, age?: number }"), "{ name: string, age?: number }");
        assert_eq!(roundtrip("{ [string]: boolean }"), "{ [string]: boolean }");
        assert_eq!(roundtrip("'left'|'right'"), "\"left\"|\"right\"");
        assert_eq!(roundtrip("[number, number]"), "[number, number]");
        assert_eq!(roundtrip("`T`"), "T");
        assert_eq!(roundtrip("vector3|vector4"), "vector3|vector4");
        assert_eq!(roundtrip("OxPlayer?"), "OxPlayer?");
        assert_eq!(roundtrip("1|2|3"), "1|2|3");
        assert_eq!(roundtrip("any|string"), "any");
    }

    #[test]
    fn stops_at_the_description() {
        let mut parser = TypeParser::new("string|number the value to use");
        assert_eq!(parser.parse().to_string(), "string|number");
        assert_eq!(parser.rest().trim(), "the value to use");
    }

    #[test]
    fn malformed_types_do_not_hang() {
        for text in ["fun(", "{ a: ", "table<", "[", "((((", "fun(a: fun(b: fun(", "|||", ""] {
            let _ = parse_type(text);
        }
    }

    #[test]
    fn widening_and_nil_removal() {
        assert_eq!(parse_type("'a'|1|true").widen().to_string(), "string|integer|boolean");
        assert_eq!(parse_type("string?").without_nil().to_string(), "string");
    }
}
