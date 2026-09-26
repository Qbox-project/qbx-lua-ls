//! Bounded workspace dependency checks over the current index, without filesystem access.
use std::collections::BTreeMap;

use qbx_lua_analysis::project::is_manifest_file;
use serde::Serialize;

use crate::index::{FileOrigin, Index, ResourceEntry};

use super::resource_details::{display_text, identity, normalized, references, ResourceIdentity};

const ISSUE_LIMIT: usize = 500;
const TARGET_LIMIT: usize = 20;

#[derive(Debug, Default, Serialize)]
pub struct HealthCounts {
    pub duplicates: usize,
    pub missing: usize,
    pub ambiguous: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthIssue {
    pub kind: &'static str,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource: Option<ResourceIdentity>,
    pub targets: Vec<ResourceIdentity>,
    pub target_count: usize,
    pub kinds: Vec<&'static str>,
}

#[derive(Debug, Serialize)]
pub struct WorkspaceHealth {
    pub files: usize,
    pub resources: usize,
    pub counts: HealthCounts,
    pub issues: Vec<HealthIssue>,
    pub truncated: usize,
    pub notes: Vec<String>,
}

struct Providers {
    name: String,
    targets: Vec<ResourceIdentity>,
    count: usize,
}

fn bounded_identity(resource: &ResourceEntry, shortened: &mut bool) -> ResourceIdentity {
    let mut result = identity(resource);
    result.name = display_text(&result.name, 2048, shortened);
    result
}

/// Checks name resolution once per distinct resource; does not build per-resource symbol details.
pub fn health(index: &Index) -> WorkspaceHealth {
    let mut shortened = false;
    let mut resources: BTreeMap<_, &ResourceEntry> = BTreeMap::new();
    for resource in &index.resources {
        resources
            .entry(normalized(&resource.root))
            .and_modify(|selected| {
                if (&resource.root, &resource.manifest_path, &resource.name)
                    < (&selected.root, &selected.manifest_path, &selected.name)
                {
                    *selected = resource;
                }
            })
            .or_insert(resource);
    }
    let mut by_name: BTreeMap<String, Vec<&ResourceEntry>> = BTreeMap::new();
    for resource in resources.values() {
        by_name.entry(resource.name.to_ascii_lowercase()).or_default().push(resource);
    }
    // Build each candidate list only once, even when many resources reference the same name.
    let providers: BTreeMap<_, _> = by_name
        .into_iter()
        .map(|(key, mut entries)| {
            entries.sort_by(|a, b| (&a.name, &a.root).cmp(&(&b.name, &b.root)));
            let name = entries[0].name.to_string();
            let count = entries.len();
            let targets =
                entries.iter().take(TARGET_LIMIT).map(|entry| bounded_identity(entry, &mut shortened)).collect();
            (key, Providers { name, targets, count })
        })
        .collect();

    let mut counts = HealthCounts::default();
    let mut issues = Vec::new();
    let mut hidden_targets = 0;
    // Duplicate groups come first in normalized-name order, so a capped report still shows them.
    for group in providers.values().filter(|group| group.count > 1) {
        counts.duplicates += 1;
        if issues.len() < ISSUE_LIMIT {
            hidden_targets += group.count.saturating_sub(TARGET_LIMIT);
            issues.push(HealthIssue {
                kind: "duplicate",
                name: display_text(&group.name, 2048, &mut shortened),
                resource: None,
                targets: group.targets.clone(),
                target_count: group.count,
                kinds: Vec::new(),
            });
        }
    }
    let mut malformed = 0;
    for resource in resources.values() {
        let refs = references(resource);
        malformed += refs.malformed;
        for (key, (name, kinds)) in refs.names {
            let group = providers.get(&key);
            let target_count = group.map_or(0, |group| group.count);
            let kind = match target_count {
                0 => {
                    counts.missing += 1;
                    "missing"
                }
                1 => continue,
                _ => {
                    counts.ambiguous += 1;
                    "ambiguous"
                }
            };
            if issues.len() < ISSUE_LIMIT {
                hidden_targets += target_count.saturating_sub(TARGET_LIMIT);
                issues.push(HealthIssue {
                    kind,
                    name: display_text(&name, 2048, &mut shortened),
                    resource: Some(bounded_identity(resource, &mut shortened)),
                    targets: group.map_or_else(Vec::new, |group| group.targets.clone()),
                    target_count,
                    kinds: kinds.into_iter().collect(),
                });
            }
        }
    }
    let truncated = (counts.duplicates + counts.missing + counts.ambiguous).saturating_sub(issues.len());
    let mut notes = vec![
        "This is a local index snapshot, not live server state. Missing and ambiguous references describe indexed resource names; an import match does not verify that its individual file is available.".into(),
        "Save manifest changes before refreshing this report. Newly created or removed files need the normal watched-file update or an index refresh; this report does not scan folders.".into(),
        "File counts include indexed Lua sources, including modules and configured library files, but exclude manifests and built-in stubs. Excluded, oversized, unreadable and non-Lua files may be absent.".into(),
        "Dependencies beginning with / are runtime constraints and are excluded from these checks. Computed dependency names cannot be checked from the manifest index.".into(),
    ];
    if resources.values().any(|resource| {
        resource.manifest.directives.iter().any(|directive| matches!(directive.name.as_str(), "provide" | "provides"))
    }) {
        notes.push("The index does not retain provide/provides aliases. Replacement resources can affect dependency resolution beyond these direct name matches.".into());
    }
    if malformed > 0 {
        notes.push(format!("{malformed} empty dependency names or malformed import paths were omitted."));
    }
    if hidden_targets > 0 {
        notes.push(format!("{hidden_targets} candidate resource folders were omitted from the displayed lists; their counts still show the full totals."));
    }
    if truncated > 0 {
        notes.push(
            "Some issue rows were omitted to keep this report small. The counts still show the full indexed totals."
                .into(),
        );
    }
    if shortened {
        notes.push("Some long resource or dependency names were shortened for display. Folder and manifest links still identify the original resources.".into());
    }
    WorkspaceHealth {
        files: index
            .files()
            .filter(|(_, file)| file.origin != FileOrigin::Stub && !is_manifest_file(&file.path))
            .count(),
        resources: resources.len(),
        counts,
        issues,
        truncated,
        notes,
    }
}
