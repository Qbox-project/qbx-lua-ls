//! Bounded, read-only resource summaries from the existing workspace index.
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use lsp_types::{Location, Url};
use qbx_fivem_data::Side;
use qbx_lua_analysis::project::{is_manifest_file, split_import};
use serde::{Deserialize, Serialize};

use crate::index::{normalize_path, EventFamily, EventKind, FileOrigin, Index, ResourceEntry};
use crate::workspace::{path_to_uri, uri_to_path};

const SYMBOL_LIMIT: usize = 500;
const RELATION_LIMIT: usize = 200;
const TARGET_LIMIT: usize = 20;
const CONSTRAINT_LIMIT: usize = 200;

pub(super) fn display_text(value: &str, limit: usize, shortened: &mut bool) -> String {
    if value.chars().take(limit + 1).count() > limit {
        *shortened = true;
        value.chars().take(limit - 1).chain(std::iter::once('…')).collect()
    } else {
        value.to_owned()
    }
}

#[derive(Deserialize)]
pub struct DetailsParams {
    pub uri: Url,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceIdentity {
    pub name: String,
    pub uri: Url,
    pub manifest_uri: Url,
}

#[derive(Debug, Serialize)]
pub struct ResourceSymbol {
    pub name: String,
    pub kind: &'static str,
    pub side: &'static str,
    pub location: Location,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceRelation {
    pub name: String,
    pub kinds: Vec<&'static str>,
    pub status: &'static str,
    pub targets: Vec<ResourceIdentity>,
    pub target_count: usize,
}

#[derive(Debug, Default, Serialize)]
pub struct FileCounts {
    pub total: usize,
    pub client: usize,
    pub server: usize,
    pub shared: usize,
    pub module: usize,
}

#[derive(Debug, Serialize)]
pub struct SymbolCounts {
    pub events: usize,
    pub exports: usize,
}

#[derive(Debug, Serialize)]
pub struct OmittedCounts {
    pub events: usize,
    pub exports: usize,
    pub dependencies: usize,
    pub dependents: usize,
}

#[derive(Debug, Serialize)]
pub struct ResourceDetails {
    pub resource: ResourceIdentity,
    pub files: FileCounts,
    pub counts: SymbolCounts,
    pub events: Vec<ResourceSymbol>,
    pub exports: Vec<ResourceSymbol>,
    pub dependencies: Vec<ResourceRelation>,
    pub dependents: Vec<ResourceRelation>,
    pub constraints: Vec<String>,
    pub notes: Vec<String>,
    pub truncated: OmittedCounts,
}

pub(super) fn normalized(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            other => result.push(other.as_os_str()),
        }
    }
    normalize_path(&result)
}

pub(super) fn identity(resource: &ResourceEntry) -> ResourceIdentity {
    ResourceIdentity {
        name: resource.name.to_string(),
        uri: path_to_uri(&resource.root),
        manifest_uri: path_to_uri(&resource.manifest_path),
    }
}

#[derive(Default)]
pub(super) struct References {
    pub(super) names: BTreeMap<String, (String, BTreeSet<&'static str>)>,
    pub(super) constraints: BTreeSet<String>,
    pub(super) malformed: usize,
}

impl References {
    fn add(&mut self, name: &str, kind: &'static str) {
        if name.is_empty() {
            self.malformed += 1;
            return;
        }
        let entry = self.names.entry(name.to_ascii_lowercase()).or_insert_with(|| (name.to_owned(), BTreeSet::new()));
        entry.1.insert(kind);
    }
}

pub(super) fn references(resource: &ResourceEntry) -> References {
    let mut result = References::default();
    for dependency in &resource.manifest.dependencies {
        if dependency.value.starts_with('/') {
            result.constraints.insert(dependency.value.to_string());
        } else {
            result.add(&dependency.value, "dependency");
        }
    }
    for import in resource.manifest.imports() {
        if let Some((resource, _)) = split_import(&import.pattern) {
            result.add(resource, "import");
        } else {
            result.malformed += 1;
        }
    }
    result
}

fn symbol_order(a: &ResourceSymbol, b: &ResourceSymbol) -> std::cmp::Ordering {
    (
        &a.name,
        a.kind,
        a.side,
        a.location.uri.as_str(),
        a.location.range.start.line,
        a.location.range.start.character,
        a.location.range.end.line,
        a.location.range.end.character,
    )
        .cmp(&(
            &b.name,
            b.kind,
            b.side,
            b.location.uri.as_str(),
            b.location.range.start.line,
            b.location.range.start.character,
            b.location.range.end.line,
            b.location.range.end.character,
        ))
}

/// Accepts only an exact indexed resource directory or its selected manifest; never probes disk.
pub(super) fn selected_resource(index: &Index, params: DetailsParams) -> Result<&ResourceEntry, String> {
    if params.uri.as_str().len() > 16_384 || params.uri.query().is_some() || params.uri.fragment().is_some() {
        return Err("Use the file URI of an indexed resource folder or manifest without a query or fragment.".into());
    }
    let path = uri_to_path(&params.uri)
        .filter(|path| path.is_absolute())
        .ok_or("Resource Details requires an absolute file URI.")?;
    let path = normalized(&path);
    index
        .resources
        .iter()
        .find(|resource| normalized(&resource.root) == path || normalized(&resource.manifest_path) == path)
        .ok_or_else(|| {
            "This folder or manifest is not an indexed resource. Open its workspace and refresh the index first.".into()
        })
}

pub fn details(index: &Index, params: DetailsParams) -> Result<ResourceDetails, String> {
    let selected = selected_resource(index, params)?;
    let selected_root = normalized(&selected.root);

    // Preserve distinct roots with the same name; first-name lookup would hide ambiguity.
    let mut by_name: BTreeMap<String, BTreeMap<PathBuf, &ResourceEntry>> = BTreeMap::new();
    for resource in &index.resources {
        by_name.entry(resource.name.to_ascii_lowercase()).or_default().insert(normalized(&resource.root), resource);
    }
    let selected_references = references(selected);
    let mut shortened = false;
    let mut dependencies = Vec::new();
    let mut hidden_targets = 0;
    for (key, (name, kinds)) in &selected_references.names {
        let candidates = by_name.get(key);
        let target_count = candidates.map_or(0, BTreeMap::len);
        let mut targets: Vec<_> =
            candidates.into_iter().flat_map(BTreeMap::values).map(|resource| identity(resource)).collect();
        targets.sort_by(|a, b| (&a.name, a.uri.as_str()).cmp(&(&b.name, b.uri.as_str())));
        hidden_targets += targets.len().saturating_sub(TARGET_LIMIT);
        targets.truncate(TARGET_LIMIT);
        dependencies.push(ResourceRelation {
            name: display_text(name, 2048, &mut shortened),
            kinds: kinds.iter().copied().collect(),
            status: match target_count {
                0 => "missing",
                1 => "resolved",
                _ => "ambiguous",
            },
            targets,
            target_count,
        });
    }
    let selected_name = selected.name.to_ascii_lowercase();
    let selected_candidates = by_name.get(&selected_name).map_or(0, BTreeMap::len);
    let mut dependents_by_root = BTreeMap::new();
    for resource in &index.resources {
        let own_root = normalized(&resource.root);
        if own_root == selected_root {
            continue;
        }
        let own_references = references(resource);
        if let Some((_, kinds)) = own_references.names.get(&selected_name) {
            dependents_by_root.insert(
                own_root,
                ResourceRelation {
                    name: resource.name.to_string(),
                    kinds: kinds.iter().copied().collect(),
                    status: if selected_candidates > 1 { "ambiguous" } else { "resolved" },
                    targets: vec![identity(resource)],
                    target_count: 1,
                },
            );
        }
    }
    let mut dependents: Vec<_> = dependents_by_root.into_values().collect();
    dependents.sort_by(|a, b| (&a.name, a.targets[0].uri.as_str()).cmp(&(&b.name, b.targets[0].uri.as_str())));

    let mut files = FileCounts::default();
    let mut events = Vec::new();
    let mut exports = Vec::new();
    let mut dynamic_exports = false;
    for (_, file) in index.files().filter(|(_, file)| {
        file.origin != FileOrigin::Stub
            && !is_manifest_file(&file.path)
            && file
                .resource
                .and_then(|id| index.resource(id))
                .is_some_and(|owner| normalized(&owner.root) == selected_root)
    }) {
        files.total += 1;
        match file.side {
            Some(Side::Client) => files.client += 1,
            Some(Side::Server) => files.server += 1,
            Some(Side::Shared) => files.shared += 1,
            None => files.module += 1,
        }
        dynamic_exports |= file.index.dynamic_exports;
        for event in file.index.events.iter().filter(|event| event.kind != EventKind::Trigger) {
            let kind = match (event.family, event.kind) {
                (_, EventKind::NetEvent) => "Network event",
                (_, EventKind::Handler) => "Event handler",
                (EventFamily::QbCore, _) => "QB-Core callback",
                (EventFamily::Esx, _) => "ESX callback",
                (EventFamily::OxLib, _) => "ox_lib callback",
                _ => "Callback",
            };
            events.push(ResourceSymbol {
                name: display_text(&event.name, 2048, &mut shortened),
                kind,
                side: event.side.map_or("unknown", Side::label),
                location: Location::new(file.uri.clone(), event.range),
                signature: event
                    .handler
                    .as_ref()
                    .map(|handler| display_text(&handler.signature(&event.name), 8192, &mut shortened)),
            });
        }
        for export in &file.index.exports {
            exports.push(ResourceSymbol {
                name: display_text(&export.name, 2048, &mut shortened),
                kind: "Export",
                side: file.side.map_or("unknown", Side::label),
                location: Location::new(file.uri.clone(), export.range),
                signature: export
                    .ty
                    .as_fun()
                    .map(|function| display_text(&function.signature(&export.name), 8192, &mut shortened)),
            });
        }
    }
    events.sort_by(symbol_order);
    exports.sort_by(symbol_order);
    let counts = SymbolCounts { events: events.len(), exports: exports.len() };
    let truncated = OmittedCounts {
        events: events.len().saturating_sub(SYMBOL_LIMIT),
        exports: exports.len().saturating_sub(SYMBOL_LIMIT),
        dependencies: dependencies.len().saturating_sub(RELATION_LIMIT),
        dependents: dependents.len().saturating_sub(RELATION_LIMIT),
    };
    events.truncate(SYMBOL_LIMIT);
    exports.truncate(SYMBOL_LIMIT);
    dependencies.truncate(RELATION_LIMIT);
    dependents.truncate(RELATION_LIMIT);
    let constraint_count = selected_references.constraints.len();
    let constraints = selected_references
        .constraints
        .into_iter()
        .take(CONSTRAINT_LIMIT)
        .map(|constraint| display_text(&constraint, 2048, &mut shortened))
        .collect();
    let mut notes = vec![
        "This is a local index snapshot, not live server state. Dependencies are matched to indexed resource names; an import match does not verify that its individual file is available.".into(),
        "Save manifest changes to refresh their metadata and script sides. Unsaved Lua source changes are reflected after the normal index update.".into(),
        "Counts cover owned indexed Lua files and literal registrations. Computed names, excluded, oversized or unreadable files, and registrations created at runtime may be absent.".into(),
    ];
    if selected.escrowed {
        notes.push(
            "This resource contains escrowed or unreadable scripts; the visible symbols may be incomplete.".into(),
        );
    }
    if selected.manifest.has_non_lua_scripts() {
        notes.push("JavaScript and C# scripts are not represented in Lua file or symbol counts.".into());
    }
    if dynamic_exports {
        notes.push("Some export names are computed at runtime; only literal export registrations are listed.".into());
    }
    if selected
        .manifest
        .directives
        .iter()
        .any(|directive| matches!(directive.name.as_str(), "export" | "exports" | "server_export" | "server_exports"))
    {
        notes.push("Manifest export declarations are not included; this list contains export registrations indexed from Lua source.".into());
    }
    if index.resources.iter().any(|resource| {
        resource.manifest.directives.iter().any(|directive| matches!(directive.name.as_str(), "provide" | "provides"))
    }) {
        notes.push("The index does not retain provide/provides aliases. Replacement resources can affect dependency resolution beyond these direct name matches.".into());
    }
    if selected_references.malformed > 0 {
        notes.push(format!(
            "{} empty dependency names or malformed import paths were omitted.",
            selected_references.malformed
        ));
    }
    if selected_candidates > 1 {
        notes.push("Several indexed resources share this name. Inverse dependents are potential matches; their ambiguous status refers to which provider they use.".into());
    }
    if hidden_targets > 0 {
        notes.push(format!("{hidden_targets} candidate resource folders were omitted from the dependency lists; their counts still show the full totals."));
    }
    if constraint_count > CONSTRAINT_LIMIT {
        notes.push(format!("{} runtime constraints were omitted.", constraint_count - CONSTRAINT_LIMIT));
    }
    if truncated.events + truncated.exports + truncated.dependencies + truncated.dependents > 0 {
        notes.push(
            "Some list entries were omitted to keep this view small. The counts still show the full indexed totals."
                .into(),
        );
    }
    if shortened {
        notes.push("Some long names, signatures or constraints were shortened for display. Source links still open the original declarations.".into());
    }
    Ok(ResourceDetails {
        resource: identity(selected),
        files,
        counts,
        events,
        exports,
        dependencies,
        dependents,
        constraints,
        notes,
        truncated,
    })
}
