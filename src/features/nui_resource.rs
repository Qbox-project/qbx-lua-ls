//! Local NUI metadata and literal Lua callback registrations from the existing index.
use lsp_types::Location;
use qbx_fivem_data::Side;
use qbx_lua_analysis::project::is_manifest_file;
use serde::Serialize;

use crate::index::{FileOrigin, Index};
use crate::nui_callbacks::REGISTRATION_GLOBALS;

use super::resource_details::{identity, normalized, selected_resource, DetailsParams, ResourceIdentity};

const CALLBACK_LIMIT: usize = 500;
const NAME_LIMIT: usize = 2048;
const PAGE_LIMIT: usize = 16_384;

#[derive(Debug, Serialize)]
pub struct NuiCallback {
    pub name: String,
    pub location: Location,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NuiResource {
    pub resource: ResourceIdentity,
    pub ui_page: Option<String>,
    pub callbacks: Vec<NuiCallback>,
    pub truncated: usize,
    pub notes: Vec<String>,
}

pub fn resource(index: &Index, params: DetailsParams) -> Result<NuiResource, String> {
    let selected = selected_resource(index, params)?;
    let root = normalized(&selected.root);
    let mut callbacks = Vec::new();
    let mut long_names = 0;
    for (id, file) in index.files().filter(|(_, file)| {
        file.origin != FileOrigin::Stub
            && !is_manifest_file(&file.path)
            && file.resource.and_then(|id| index.resource(id)).is_some_and(|owner| normalized(&owner.root) == root)
    }) {
        // Other open files can replace or restore a global without reindexing this callback file.
        let replaced = |name| {
            index.globals_named(name, id).iter().any(|(file, _)| {
                index
                    .file(*file)
                    .is_some_and(|entry| entry.origin != FileOrigin::Stub && entry.side != Some(Side::Server))
            })
        };
        if replaced("_ENV") {
            continue;
        }
        let globals = REGISTRATION_GLOBALS.map(|name| (name, replaced(name)));
        for callback in &file.index.nui_callbacks {
            if globals.iter().any(|(name, replaced)| *name == callback.registration && *replaced) {
                continue;
            }
            // These are actionable callback names: never turn a shortened label into another callback.
            if callback.name.chars().take(NAME_LIMIT + 1).count() > NAME_LIMIT {
                long_names += 1;
                continue;
            }
            callbacks.push(NuiCallback {
                name: callback.name.to_string(),
                location: Location::new(file.uri.clone(), callback.range),
            });
        }
    }
    callbacks.sort_by(|a, b| {
        (
            &a.name,
            a.location.uri.as_str(),
            a.location.range.start.line,
            a.location.range.start.character,
            a.location.range.end.line,
            a.location.range.end.character,
        )
            .cmp(&(
                &b.name,
                b.location.uri.as_str(),
                b.location.range.start.line,
                b.location.range.start.character,
                b.location.range.end.line,
                b.location.range.end.character,
            ))
    });
    let truncated = long_names + callbacks.len().saturating_sub(CALLBACK_LIMIT);
    callbacks.truncate(CALLBACK_LIMIT);
    let page = selected.manifest.ui_page.as_ref().map(|entry| entry.value.as_str());
    let ui_page = page.filter(|page| page.len() <= PAGE_LIMIT).map(str::to_owned);
    let mut notes = vec![
        "Preview runs browser UI with mocked callbacks. Test game behavior in FiveM.".into(),
        "Save manifest changes before reloading the UI page.".into(),
        "Callbacks include literal client-side RegisterNUICallback and RegisterNuiCallback calls. Dynamic names, aliases and imported code may be absent.".into(),
        "Unsaved Lua edits are included. Multiple registrations remain separate so you can choose a source.".into(),
    ];
    if page.is_some() && ui_page.is_none() {
        notes.push("The literal UI page exceeds 16,384 bytes and was omitted; its path was not shortened.".into());
    } else if page.is_none() {
        notes.push(
            "No literal ui_page is recorded in the saved manifest. Computed manifest values are not evaluated.".into(),
        );
    }
    if long_names > 0 {
        notes.push(format!(
            "{long_names} callback names longer than 2,048 characters were omitted rather than changed."
        ));
    }
    if truncated > long_names {
        notes
            .push(format!("{} additional callback rows were omitted to keep this view small.", truncated - long_names));
    }
    if selected.escrowed || selected.manifest.has_non_lua_scripts() {
        notes.push(
            "Escrowed, unreadable and non-Lua scripts are not fully represented in these callback results.".into(),
        );
    }
    Ok(NuiResource { resource: identity(selected), ui_page, callbacks, truncated, notes })
}
