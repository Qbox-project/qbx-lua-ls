use std::sync::Arc;

use smol_str::SmolStr;

use crate::types::{FunType, Param, Type, TypeParser};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DocParam {
    pub name: SmolStr,
    pub ty: Type,
    pub optional: bool,
    pub description: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DocReturn {
    pub ty: Type,
    pub name: Option<SmolStr>,
    pub description: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DocField {
    pub name: SmolStr,
    pub ty: Type,
    pub optional: bool,
    pub description: String,
    /// Index of the doc line the field was declared on, used to locate it in the source.
    pub line: usize,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DocClass {
    pub name: SmolStr,
    pub parents: Vec<SmolStr>,
    pub fields: Vec<DocField>,
    pub index: Option<(Type, Type)>,
    pub call: Option<Arc<FunType>>,
    pub description: String,
    pub line: usize,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DocAlias {
    pub name: SmolStr,
    pub ty: Type,
    pub description: String,
    pub line: usize,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DocGroup {
    pub description: String,
    pub classes: Vec<DocClass>,
    pub aliases: Vec<DocAlias>,
    pub params: Vec<DocParam>,
    pub returns: Vec<DocReturn>,
    pub ty: Option<Type>,
    pub enum_name: Option<SmolStr>,
    pub enum_keys: bool,
    pub generics: Vec<SmolStr>,
    pub overloads: Vec<Arc<FunType>>,
    pub deprecated: Option<String>,
    pub is_async: bool,
    pub nodiscard: bool,
    pub is_meta: bool,
}

impl DocGroup {
    pub fn has_function_tags(&self) -> bool {
        !self.params.is_empty() || !self.returns.is_empty() || !self.overloads.is_empty()
    }

    /// Builds the function type from `@param`/`@return`, keeping the order of the real parameters.
    pub fn fun_type(&self, param_names: &[SmolStr], has_vararg: bool, is_method: bool) -> FunType {
        let mut params: Vec<Param> = param_names
            .iter()
            .map(|name| match self.params.iter().find(|p| p.name == *name) {
                Some(doc) => Param { name: name.clone(), ty: doc.ty.clone(), optional: doc.optional },
                None => Param { name: name.clone(), ty: Type::Unknown, optional: false },
            })
            .collect();
        if has_vararg {
            let ty = self.params.iter().find(|p| p.name == "...").map_or(Type::Any, |p| p.ty.clone());
            params.push(Param { name: "...".into(), ty, optional: false });
        }
        FunType {
            params,
            returns: self.returns.iter().map(|r| r.ty.clone()).collect(),
            is_method,
            generics: self.generics.clone(),
        }
    }

    pub fn param_description(&self, name: &str) -> Option<&str> {
        self.params.iter().find(|p| p.name == name).map(|p| p.description.as_str()).filter(|d| !d.is_empty())
    }
}

fn clean_description(text: &str) -> String {
    let text = text.trim();
    let text = text.strip_prefix('#').or_else(|| text.strip_prefix("--")).unwrap_or(text);
    text.trim().to_string()
}

fn split_tag(line: &str) -> Option<(&str, &str)> {
    let rest = line.trim_start().strip_prefix('@')?;
    let end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
    Some((&rest[..end], rest[end..].trim_start()))
}

/// Removes the `public`, `private`, `protected` and `package` keywords in front of a `@field` name.
fn strip_visibility(mut rest: &str) -> &str {
    for scope in ["public ", "private ", "protected ", "package "] {
        if let Some(stripped) = rest.strip_prefix(scope) {
            rest = stripped.trim_start();
        }
    }
    rest
}

/// The type on a `---| value` line that continues an `@alias`, without its `>` or `+` marker.
fn alias_member(line: &str) -> Option<&str> {
    Some(line.trim_start().strip_prefix('|')?.trim_start_matches(['>', '+', ' ']))
}

/// Splits attributes such as `(exact)`, `(partial)` or `(key)` off the front of a `@class` or `@enum`.
fn split_attributes(rest: &str) -> (&str, &str) {
    match rest.strip_prefix('(').and_then(|inner| inner.split_once(')')) {
        Some((attributes, rest)) => (attributes, rest.trim_start()),
        None => ("", rest),
    }
}

/// Parses the `---` lines of one contiguous doc comment; every line has its `---` prefix removed.
pub fn parse_doc_lines(lines: &[&str]) -> DocGroup {
    let mut group = DocGroup::default();
    let mut description: Vec<&str> = Vec::new();
    let mut open_alias = false;

    for (index, raw) in lines.iter().enumerate() {
        let line = raw.strip_prefix(' ').unwrap_or(raw);
        if let Some(member) = alias_member(line) {
            if open_alias {
                if let Some(alias) = group.aliases.last_mut() {
                    let mut parser = TypeParser::new(member);
                    let ty = parser.parse();
                    alias.ty = Type::union([std::mem::take(&mut alias.ty), ty]);
                }
                continue;
            }
        }
        let Some((tag, rest)) = split_tag(line) else {
            description.push(line);
            continue;
        };
        open_alias = false;
        match tag {
            "class" => {
                let (_, rest) = split_attributes(rest);
                let (head, parents) = rest.split_once(':').unwrap_or((rest, ""));
                let name = head.split_whitespace().next().unwrap_or("").split('<').next().unwrap_or("");
                if name.is_empty() {
                    continue;
                }
                let parents = parents
                    .split(',')
                    .filter_map(|p| p.split_whitespace().next())
                    .map(|p| SmolStr::new(p.split('<').next().unwrap_or(p)))
                    .collect();
                group.classes.push(DocClass {
                    name: SmolStr::new(name),
                    parents,
                    description: description.join("\n").trim().to_string(),
                    line: index,
                    ..DocClass::default()
                });
            }
            "field" => parse_field(rest, index, &mut group),
            "overload" if !group.classes.is_empty() && !group.has_function_tags() => {
                if let Type::Fun(fun) = TypeParser::new(rest).parse() {
                    if let Some(class) = group.classes.last_mut() {
                        class.call = Some(fun);
                    }
                }
            }
            "overload" => {
                if let Type::Fun(fun) = TypeParser::new(rest).parse() {
                    group.overloads.push(fun);
                }
            }
            "alias" => {
                let mut parser = TypeParser::new(rest);
                let Some(name) = parser.ident() else { continue };
                parser.skip_ws();
                let ty = if parser.rest().trim().is_empty() { Type::Unknown } else { parser.parse() };
                group.aliases.push(DocAlias {
                    name: SmolStr::new(name),
                    ty,
                    description: description.join("\n").trim().to_string(),
                    line: index,
                });
                open_alias = true;
            }
            "enum" => {
                let (attributes, rest) = split_attributes(rest);
                group.enum_name = rest.split_whitespace().next().map(SmolStr::new);
                group.enum_keys = attributes.split(',').any(|attribute| attribute.trim() == "key");
            }
            "param" => {
                let mut parser = TypeParser::new(rest);
                let name = if parser.rest().starts_with("...") {
                    parser = TypeParser::new(&rest[3..]);
                    "..."
                } else {
                    match parser.ident() {
                        Some(name) => name,
                        None => continue,
                    }
                };
                let optional = parser.rest().starts_with('?');
                let mut parser = TypeParser::new(parser.rest().trim_start_matches('?'));
                let ty = parser.parse();
                group.params.push(DocParam {
                    name: SmolStr::new(name),
                    optional: optional || matches!(&ty, Type::Union(types) if types.contains(&Type::Nil)),
                    ty,
                    description: clean_description(parser.rest()),
                });
            }
            "return" => {
                let mut parser = TypeParser::new(rest);
                let ty = parser.parse();
                parser.skip_ws();
                let remainder = parser.rest();
                let (name, desc) = match remainder.split_whitespace().next() {
                    Some(word) if !word.starts_with('#') && word.chars().all(|c| c.is_alphanumeric() || c == '_') => {
                        (Some(SmolStr::new(word)), &remainder[word.len()..])
                    }
                    _ => (None, remainder),
                };
                group.returns.push(DocReturn { ty, name, description: clean_description(desc) });
            }
            "type" => group.ty = Some(TypeParser::new(rest).parse()),
            "generic" => {
                let names = rest.split(',').filter_map(|g| g.trim().split([':', ' ']).next()).filter(|g| !g.is_empty());
                group.generics.extend(names.map(SmolStr::new));
            }
            "vararg" => {
                let ty = TypeParser::new(rest).parse();
                group.params.push(DocParam { name: "...".into(), ty, ..DocParam::default() });
            }
            "deprecated" => group.deprecated = Some(rest.trim().to_string()),
            "async" => group.is_async = true,
            "nodiscard" => group.nodiscard = true,
            "meta" => group.is_meta = true,
            _ => {}
        }
    }

    group.description = description.join("\n").trim().to_string();
    group
}

fn parse_field(rest: &str, line: usize, group: &mut DocGroup) {
    let Some(class) = group.classes.last_mut() else { return };
    let rest = strip_visibility(rest);
    if let Some(index) = rest.strip_prefix('[') {
        let mut parser = TypeParser::new(index);
        let key = parser.parse();
        let after = parser.rest().trim_start().strip_prefix(']').unwrap_or(parser.rest());
        let value = TypeParser::new(after).parse();
        match key {
            Type::StringLit(name) => class.fields.push(DocField { name, ty: value, line, ..DocField::default() }),
            key => class.index = Some((key, value)),
        }
        return;
    }
    let mut parser = TypeParser::new(rest);
    let Some(name) = parser.ident() else { return };
    let optional = parser.rest().starts_with('?');
    let mut parser = TypeParser::new(parser.rest().trim_start_matches('?'));
    let ty = parser.parse();
    class.fields.push(DocField {
        name: SmolStr::new(name),
        ty,
        optional,
        description: clean_description(parser.rest()),
        line,
    });
}

fn skip_name(rest: &str) -> Option<&str> {
    let mut parser = TypeParser::new(rest);
    parser.ident()?;
    Some(parser.rest())
}

/// Collects the class and alias names of one doc line. Every `rest` passed in is a suffix of
/// `line`, so it starts at `line.len() - rest.len()`.
struct TypeNames<'a> {
    line: &'a str,
    found: Vec<(usize, &'a str)>,
}

impl<'a> TypeNames<'a> {
    /// Records the name a `@class`, `@alias` or `@enum` declares and returns the text after it.
    fn declared(&mut self, rest: &'a str) -> Option<&'a str> {
        let mut parser = TypeParser::new(rest);
        let name = parser.ident()?;
        self.found.push((self.line.len() - parser.rest().len() - name.len(), name));
        Some(parser.rest())
    }

    /// Records the names in the type `rest` starts with and returns the text after it.
    fn ty(&mut self, rest: &'a str) -> &'a str {
        let start = self.line.len() - rest.len();
        let mut parser = TypeParser::new(rest);
        self.found.extend(parser.parse_names().into_iter().map(|(offset, name)| (start + offset, name)));
        parser.rest()
    }

    fn types(&mut self, rest: &'a str) {
        self.list(rest, |names, rest| Some(names.ty(rest)));
    }

    /// Walks a comma separated list, where `item` reads one entry and returns the text after it.
    fn list(&mut self, rest: &'a str, item: impl Fn(&mut Self, &'a str) -> Option<&'a str>) {
        let mut next = Some(rest);
        while let Some(after) = next.and_then(|rest| item(self, rest.trim_start())) {
            next = after.trim_start().strip_prefix(',');
        }
    }
}

/// The class or alias name at byte `offset` of a doc line whose `---` prefix is removed, with the
/// byte it starts at. Parameter, field and return names, literals and built-in types do not count.
pub fn type_name_at(line: &str, offset: usize) -> Option<(usize, &str)> {
    let (tag, rest) = match alias_member(line) {
        Some(member) => ("|", member),
        None => split_tag(line)?,
    };
    let mut names = TypeNames { line, found: Vec::new() };
    match tag {
        "class" => {
            let (_, rest) = split_attributes(rest);
            names.declared(rest);
            // Parsing the head as a type skips generic parameters such as the `T` of `Child<T>`.
            let mut head = TypeParser::new(rest);
            head.parse();
            if let Some(parents) = head.rest().trim_start().strip_prefix(':') {
                names.types(parents);
            }
        }
        "alias" => {
            if let Some(rest) = names.declared(rest) {
                names.types(rest);
            }
        }
        "enum" => {
            names.declared(split_attributes(rest).1);
        }
        "param" => {
            if let Some(rest) = rest.strip_prefix("...").or_else(|| skip_name(rest)) {
                names.types(rest.trim_start_matches('?'));
            }
        }
        "field" => {
            let rest = strip_visibility(rest);
            let value = match rest.strip_prefix('[') {
                Some(key) => names.ty(key).trim_start().strip_prefix(']'),
                None => skip_name(rest),
            };
            if let Some(value) = value {
                names.types(value.trim_start_matches('?'));
            }
        }
        "cast" => {
            if let Some(rest) = skip_name(rest) {
                names.list(rest, |names, rest| Some(names.ty(rest.trim_start_matches(['+', '-']))));
            }
        }
        "return" => names.list(rest, |names, rest| {
            let after = names.ty(rest);
            Some(skip_name(after).unwrap_or(after))
        }),
        "operator" => {
            if let Some(rest) = skip_name(rest).map(str::trim_start) {
                let rest = match rest.strip_prefix('(') {
                    Some(operand) => names.ty(operand).trim_start().strip_prefix(')'),
                    None => Some(rest),
                };
                if let Some(result) = rest.and_then(|rest| rest.trim_start().strip_prefix(':')) {
                    names.ty(result);
                }
            }
        }
        "generic" => names.list(rest, |names, rest| {
            let rest = skip_name(rest)?.trim_start();
            Some(rest.strip_prefix(':').map_or(rest, |constraint| names.ty(constraint)))
        }),
        "see" => {
            names.ty(rest);
        }
        "type" | "overload" | "vararg" | "as" | "|" => names.types(rest),
        _ => {}
    }
    names.found.into_iter().find(|(start, name)| (*start..=start + name.len()).contains(&offset))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> DocGroup {
        let lines: Vec<&str> = text.lines().map(|l| l.trim_start().strip_prefix("---").unwrap_or(l)).collect();
        parse_doc_lines(&lines)
    }

    #[test]
    fn function_docs() {
        let doc = parse(
            "---Spawns a vehicle.\n---Second line.\n---@param model string|integer the model\n---@param coords? vector4\n---@param ... any extra\n---@return integer netId # network id\n---@return string? err\n---@deprecated use other\n---@async",
        );
        assert_eq!(doc.description, "Spawns a vehicle.\nSecond line.");
        assert_eq!(doc.params.len(), 3);
        assert_eq!(doc.params[0].description, "the model");
        assert!(doc.params[1].optional);
        assert_eq!(doc.params[2].name, "...");
        assert_eq!(doc.returns[0].name.as_deref(), Some("netId"));
        assert_eq!(doc.returns[0].description, "network id");
        assert_eq!(doc.returns[1].ty.to_string(), "string?");
        assert_eq!(doc.deprecated.as_deref(), Some("use other"));
        assert!(doc.is_async);

        let fun = doc.fun_type(&["model".into(), "coords".into()], true, false);
        assert_eq!(
            fun.signature("spawn"),
            "function spawn(model: string|integer, coords?: vector4, ...: any): integer, string?"
        );
    }

    #[test]
    fn classes_fields_and_aliases() {
        let doc = parse(
            "---A player.\n---@class Player : Entity, Base\n---@field name string the name\n---@field private job? Job\n---@field [string] any\n---@field ['quoted-key'] number\n---@overload fun(id: integer): Player\n---@alias Side\n---| 'client' # runs on the client\n---| 'server'\n---@alias Id integer|string",
        );
        let class = &doc.classes[0];
        assert_eq!(class.name, "Player");
        assert_eq!(class.parents, ["Entity", "Base"]);
        assert_eq!(class.description, "A player.");
        assert_eq!(class.fields.len(), 3);
        assert!(class.fields[1].optional);
        assert_eq!(class.fields[2].name, "quoted-key");
        assert!(class.index.is_some());
        assert!(class.call.is_some());
        assert_eq!(doc.aliases[0].ty.to_string(), "\"client\"|\"server\"");
        assert_eq!(doc.aliases[1].ty.to_string(), "integer|string");
    }

    #[test]
    fn class_and_enum_attributes() {
        let doc = parse("---@class (partial) Player : Entity");
        assert_eq!(doc.classes[0].name, "Player");
        assert_eq!(doc.classes[0].parents, ["Entity"]);
        let doc = parse("---@enum (key) Side");
        assert_eq!(doc.enum_name.as_deref(), Some("Side"));
        assert!(doc.enum_keys);
        assert!(!parse("---@enum Side").enum_keys);
    }

    #[test]
    fn bracketed_descriptions_inside_field_types() {
        let doc = parse(
            "---@class Config.Bleeding\n---@field items table<string, {value: number [how much], anim: table {dict: string [dictionary]}}> [items]",
        );
        assert_eq!(doc.classes[0].fields[0].name, "items");
    }

    #[test]
    fn type_and_generics() {
        let doc = parse("---@generic T: table, K\n---@type table<string, fun(): boolean>");
        assert_eq!(doc.generics, ["T", "K"]);
        assert_eq!(doc.ty.unwrap().to_string(), "table<string, fun(): boolean>");
    }

    #[test]
    fn finds_type_names_in_annotations() {
        for line in [
            "@type Gar^age",
            "@type Garage^",
            "@type string, Gar^age",
            "@type (string|Gar^age)[]?",
            "@type table<string, Gar^age>",
            "@type fun(value: Gar^age): boolean",
            "@type fun(): Gar^age",
            "@type { value: Gar^age }",
            "@type { [Gar^age]: boolean }",
            "@type [string, Gar^age]",
            "@param value? Gar^age",
            "@param ... Gar^age",
            "@return string, Gar^age",
            "@return string name, Gar^age value",
            "@field private value? Gar^age",
            "@field [Gar^age] string",
            "@field ['value'] Gar^age",
            "@class Gar^age",
            "@class (exact) Child<T>: Parent, Gar^age",
            "@class (partial) Gar^age",
            "@class (partial) Child: Gar^age",
            "@alias Gar^age string",
            "@alias Value Gar^age",
            "| > Gar^age # description",
            "@enum Gar^age",
            "@enum (key) Gar^age",
            "@overload fun(): Gar^age",
            "@operator add(Gar^age): Vector",
            "@operator mul(number): Gar^age",
            "@operator call(): Gar^age",
            "@operator unm: Gar^age",
            "@generic T, U: table<string, Gar^age>",
            "@generic T: string, U: Gar^age",
            "@cast value +string, -Gar^age",
            "@vararg Gar^age",
            "@as Gar^age",
            "@see Gar^age",
            "@see Gar^age for details",
        ] {
            let offset = line.find('^').unwrap();
            let text = line.replace('^', "");
            assert_eq!(type_name_at(&text, offset), Some((text.find("Garage").unwrap(), "Garage")), "{line}");
        }
        let line = "@type Garage.Point[]";
        assert_eq!(type_name_at(line, line.find("Point").unwrap()), Some((6, "Garage.Point")));
    }

    #[test]
    fn ignores_non_type_names_in_annotations() {
        for line in [
            "A Gar^age in the description",
            "@pa^ram value Garage",
            "@param Gar^age string",
            "@field Gar^age string",
            "@field ['Gar^age'] string",
            "@field public^ value Garage",
            "@return string Gar^age",
            "@type string # Gar^age",
            "@type string Gar^age",
            "@type 'Gar^age'",
            "@type fun(Gar^age: string): boolean",
            "@type { Gar^age: string }",
            "@type str^ing",
            "@generic Gar^age: string",
            "@class Child<Gar^age>: Parent",
            "@class (Gar^age) Child",
            "@enum (Gar^age) Mode",
            "@cast Gar^age string",
            "@operator Gar^age(number): string",
            "@see string, see Gar^age",
        ] {
            let offset = line.find('^').unwrap();
            assert_eq!(type_name_at(&line.replace('^', ""), offset), None, "{line}");
        }
    }
}
