//! On-demand, read-only search over the reference data already bundled with the server.

use std::collections::BTreeSet;
use std::sync::OnceLock;

use qbx_fivem_data::{
    controls, native, native_docs, natives, ped_config_flags, Native, CONTROLS_SOURCE_URL, PED_CONFIG_FLAGS_SOURCE_URL,
};
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};

use super::native_argument::{control_documentation, ped_flag_documentation};

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ReferenceKind {
    Native,
    Control,
    PedFlag,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum KindFilter {
    #[default]
    All,
    Native,
    Control,
    PedFlag,
}

impl KindFilter {
    fn includes(&self, kind: ReferenceKind) -> bool {
        matches!(
            (self, kind),
            (Self::All, _)
                | (Self::Native, ReferenceKind::Native)
                | (Self::Control, ReferenceKind::Control)
                | (Self::PedFlag, ReferenceKind::PedFlag)
        )
    }
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SideFilter {
    #[default]
    All,
    Client,
    Server,
    Shared,
}

impl SideFilter {
    fn includes(&self, side: &str) -> bool {
        match self {
            Self::All => true,
            Self::Client => matches!(side, "client" | "shared"),
            Self::Server => matches!(side, "server" | "shared"),
            Self::Shared => side == "shared",
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SearchParams {
    pub query: String,
    pub kind: KindFilter,
    pub side: SideFilter,
    pub namespace: Option<String>,
    pub offset: u64,
    pub limit: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReferenceItem {
    pub id: String,
    pub kind: ReferenceKind,
    pub name: &'static str,
    pub side: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hash: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub numeric_id: Option<u32>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResult {
    pub items: Vec<ReferenceItem>,
    pub total: usize,
    pub offset: usize,
    pub limit: usize,
    pub namespaces: Vec<&'static str>,
}

#[derive(Deserialize)]
pub struct DetailParams {
    pub id: String,
}

#[derive(Serialize)]
pub struct Parameter {
    pub name: &'static str,
    #[serde(rename = "type")]
    pub lua_type: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReferenceDetail {
    #[serde(flatten)]
    pub item: ReferenceItem,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<Vec<Parameter>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub returns: Option<Vec<&'static str>>,
    pub documentation: String,
    pub source_url: String,
    pub copy_text: String,
    pub insert_text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub insert_snippet: Option<String>,
}

struct Entry {
    item: ReferenceItem,
    fields: Vec<String>,
}

struct Catalog {
    entries: Vec<Entry>,
    /// Canonical names, documented aliases and synthesized N_0x hash spellings all resolve to one row.
    native_names: FxHashMap<String, usize>,
    namespaces: Vec<&'static str>,
}

fn native_item(native: Native) -> ReferenceItem {
    ReferenceItem {
        id: format!("native:{}", native.name),
        kind: ReferenceKind::Native,
        name: native.name,
        side: native.side.label(),
        namespace: Some(native.namespace),
        hash: Some(native.hash),
        numeric_id: None,
    }
}

fn numeric_item(kind: ReferenceKind, id: u32, name: &'static str) -> ReferenceItem {
    let prefix = if kind == ReferenceKind::Control { "control" } else { "pedFlag" };
    ReferenceItem {
        id: format!("{prefix}:{id}"),
        kind,
        name,
        side: "client",
        namespace: None,
        hash: None,
        numeric_id: Some(id),
    }
}

fn catalog() -> &'static Catalog {
    static CATALOG: OnceLock<Catalog> = OnceLock::new();
    CATALOG.get_or_init(|| {
        let mut entries = Vec::new();
        let mut native_names = FxHashMap::default();
        let mut namespaces = BTreeSet::new();
        for native in natives().filter(|native| native.alias_of.is_none()) {
            let name = native.name.to_ascii_lowercase();
            let hash = native.hash.to_ascii_lowercase();
            let hash_name = format!("n_{hash}");
            native_names.insert(name.clone(), entries.len());
            native_names.insert(hash_name.clone(), entries.len());
            namespaces.insert(native.namespace);
            entries.push(Entry {
                item: native_item(native),
                fields: vec![
                    name.replace('_', ""),
                    name,
                    hash.trim_start_matches("0x").to_owned(),
                    hash,
                    hash_name,
                    native.namespace.to_ascii_lowercase(),
                ],
            });
        }
        for alias in natives().filter(|native| native.alias_of.is_some()) {
            let target = alias.alias_of.unwrap().to_ascii_lowercase();
            if let Some(&index) = native_names.get(&target) {
                let name = alias.name.to_ascii_lowercase();
                entries[index].fields.push(name.clone());
                entries[index].fields.push(name.replace('_', ""));
                native_names.insert(name, index);
            }
        }
        for control in controls() {
            entries.push(Entry {
                item: numeric_item(ReferenceKind::Control, control.id, control.name),
                fields: vec![
                    control.name.to_ascii_lowercase(),
                    control.name.to_ascii_lowercase().replace('_', ""),
                    control.id.to_string(),
                    control.keyboard.to_ascii_lowercase(),
                    control.controller.to_ascii_lowercase(),
                ],
            });
        }
        for flag in ped_config_flags() {
            entries.push(Entry {
                item: numeric_item(ReferenceKind::PedFlag, flag.id, flag.name),
                fields: vec![
                    flag.name.to_ascii_lowercase(),
                    flag.name.to_ascii_lowercase().replace('_', ""),
                    flag.id.to_string(),
                ],
            });
        }
        Catalog { entries, native_names, namespaces: namespaces.into_iter().collect() }
    })
}

fn match_rank(fields: &[String], query: &str, compact_query: &str, tokens: &[&str]) -> Option<u8> {
    if query.is_empty() {
        return Some(0);
    }
    if fields.iter().any(|field| field == query || (!compact_query.is_empty() && field == compact_query)) {
        return Some(0);
    }
    if !tokens.iter().all(|token| fields.iter().any(|field| field.contains(token))) {
        return None;
    }
    Some(if tokens.iter().all(|token| fields.iter().any(|field| field.starts_with(token))) { 1 } else { 2 })
}

pub fn search(params: SearchParams) -> Result<SearchResult, String> {
    if params.query.chars().count() > 256 {
        return Err("reference query must not exceed 256 characters".into());
    }
    if params.namespace.as_ref().is_some_and(|value| value.chars().count() > 64) {
        return Err("reference namespace must not exceed 64 characters".into());
    }
    let catalog = catalog();
    let query = params.query.trim().to_ascii_lowercase();
    let compact_query = query.replace('_', "");
    let mut tokens: Vec<_> =
        query.split(|c: char| c.is_whitespace() || c == '_').filter(|token| !token.is_empty()).collect();
    if tokens.is_empty() && !query.is_empty() {
        tokens.push(&query);
    }
    let namespace = params.namespace.as_deref().unwrap_or("").trim();
    let mut matches: Vec<_> = catalog
        .entries
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| {
            if !params.kind.includes(entry.item.kind)
                || !params.side.includes(entry.item.side)
                || (entry.item.kind == ReferenceKind::Native
                    && !namespace.is_empty()
                    && !entry.item.namespace.is_some_and(|value| value.eq_ignore_ascii_case(namespace)))
            {
                return None;
            }
            Some((match_rank(&entry.fields, &query, &compact_query, &tokens)?, index))
        })
        .collect();
    // Catalog order breaks ties: native name order first, followed by controls/flags in numeric ID order.
    matches.sort_unstable();
    let total = matches.len();
    let offset = usize::try_from(params.offset).unwrap_or(usize::MAX).min(total);
    let limit = params.limit.unwrap_or(50).clamp(1, 100) as usize;
    let items =
        matches.iter().skip(offset).take(limit).map(|(_, index)| catalog.entries[*index].item.clone()).collect();
    Ok(SearchResult { items, total, offset, limit, namespaces: catalog.namespaces.clone() })
}

fn snippet_escape(text: &str) -> String {
    text.replace('\\', "\\\\").replace('$', "\\$").replace('}', "\\}")
}

pub fn detail(params: DetailParams) -> Result<Option<ReferenceDetail>, String> {
    if params.id.chars().count() > 512 {
        return Err("reference ID must not exceed 512 characters".into());
    }
    let Some((kind, value)) = params.id.split_once(':') else { return Ok(None) };
    let item = match kind {
        "native" => {
            catalog().native_names.get(&value.to_ascii_lowercase()).map(|&index| catalog().entries[index].item.clone())
        }
        "control" | "pedFlag" => {
            // Stable numeric IDs use canonical decimal spelling, not signed or zero-padded aliases.
            let number = value.parse::<u32>().ok().filter(|id| id.to_string() == value);
            number.and_then(|id| {
                if kind == "control" {
                    qbx_fivem_data::control(id).map(|row| numeric_item(ReferenceKind::Control, id, row.name))
                } else {
                    qbx_fivem_data::ped_config_flag(id).map(|row| numeric_item(ReferenceKind::PedFlag, id, row.name))
                }
            })
        }
        _ => None,
    };
    let Some(item) = item else { return Ok(None) };
    let mut detail = ReferenceDetail {
        item,
        signature: None,
        parameters: None,
        returns: None,
        documentation: String::new(),
        source_url: String::new(),
        copy_text: String::new(),
        insert_text: String::new(),
        insert_snippet: None,
    };
    if detail.item.kind == ReferenceKind::Native {
        let native = native(detail.item.name).expect("catalog comes from bundled native metadata");
        let params: Vec<_> = native.params().collect();
        detail.signature = Some(native.signature());
        detail.parameters = Some(params.iter().map(|&(name, lua_type)| Parameter { name, lua_type }).collect());
        detail.returns = Some(native.returns().collect());
        detail.documentation = native_docs(native.name)
            .filter(|doc| !doc.trim().is_empty())
            .unwrap_or_else(|| "No description is documented in the bundled Cfx native reference.".into());
        detail.source_url = format!("https://docs.fivem.net/natives/?_{}", native.hash);
        detail.copy_text = native.name.into();
        detail.insert_text =
            format!("{}({})", native.name, params.iter().map(|(name, _)| *name).collect::<Vec<_>>().join(", "));
        detail.insert_snippet = Some(format!(
            "{}({})$0",
            snippet_escape(native.name),
            params
                .iter()
                .enumerate()
                .map(|(index, (name, _))| format!("${{{}:{}}}", index + 1, snippet_escape(name)))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    } else {
        let id = detail.item.numeric_id.expect("numeric catalog entries have IDs");
        let (documentation, url) = if detail.item.kind == ReferenceKind::Control {
            (control_documentation(id), CONTROLS_SOURCE_URL)
        } else {
            (ped_flag_documentation(id), PED_CONFIG_FLAGS_SOURCE_URL)
        };
        detail.documentation = documentation.expect("catalog comes from bundled reference metadata");
        detail.source_url = url.into();
        detail.copy_text = id.to_string();
        detail.insert_text = id.to_string();
    }
    Ok(Some(detail))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_defaults_escape_vscode_snippet_metacharacters() {
        assert_eq!(snippet_escape(r"a$}\b"), r"a\$\}\\b");
    }
}
