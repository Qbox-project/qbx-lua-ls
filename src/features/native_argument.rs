use std::sync::OnceLock;

use qbx_fivem_data::{
    control, native, natives, ped_config_flag, Native, CONTROLS_SOURCE_URL, PED_CONFIG_FLAGS_SOURCE_URL,
};
use qbx_lua_analysis::scope::Resolved;
use qbx_lua_syntax::ast::ExprKind;
use qbx_lua_syntax::{NumberValue, Span};

use crate::document::Document;
use crate::index::FileOrigin;
use crate::locate::locate;
use crate::workspace::Workspace;

enum ArgumentKind {
    Control,
    PedConfigFlag,
}

fn argument_kind(native: Native, index: usize) -> Option<ArgumentKind> {
    let (parameter, _) = native.params().nth(index)?;
    if native.namespace == "PAD" && parameter == "control" {
        Some(ArgumentKind::Control)
    } else if matches!(native.name, "SetPedConfigFlag" | "GetPedConfigFlag") && parameter == "flagId" {
        Some(ArgumentKind::PedConfigFlag)
    } else {
        None
    }
}

fn named_native(name: &str) -> Option<Native> {
    if let Some(found) = native(name) {
        return found.alias_of.and_then(native).or(Some(found));
    }
    // Known natives are callable by hash even when there is no separate alias row.
    let hex = name.strip_prefix("N_0x")?;
    if hex.is_empty() || hex.len() > 16 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let hash = u64::from_str_radix(hex, 16).ok()?;
    static HASHES: OnceLock<Vec<(u64, Native)>> = OnceLock::new();
    let hashes = HASHES.get_or_init(|| {
        natives()
            .filter(|native| {
                native.alias_of.is_none()
                    && native.params().enumerate().any(|(i, _)| argument_kind(*native, i).is_some())
            })
            .filter_map(|native| Some((u64::from_str_radix(native.hash.strip_prefix("0x")?, 16).ok()?, native)))
            .collect()
    });
    hashes.iter().find(|(value, _)| *value == hash).map(|(_, native)| *native)
}

fn literal_id(value: NumberValue) -> Option<u32> {
    match value {
        NumberValue::Int(value) => u32::try_from(value).ok(),
        NumberValue::Float(value) if value >= 0.0 && value <= u32::MAX as f64 && value.fract() == 0.0 => {
            Some(value as u32)
        }
        _ => None,
    }
}

fn binding(value: &str) -> String {
    if value.is_empty() {
        return "Not documented".to_owned();
    }
    if value.contains('`') {
        // CommonMark code spans need a longer fence and padding for literal backtick keys.
        let fence = "`".repeat(value.split(|c| c != '`').map(str::len).max().unwrap_or(0) + 1);
        format!("{fence} {value} {fence}")
    } else {
        format!("`{value}`")
    }
}

pub(super) fn control_documentation(id: u32) -> Option<String> {
    let control = control(id)?;
    Some(format!(
        "**{}** · control `{id}`\n\nDefault keyboard (QWERTY): {}  \nDefault Xbox controller: {}\n\n\
         These are documented defaults. The player's bindings may be remapped.\n\n\
         [Cfx controls reference]({CONTROLS_SOURCE_URL})",
        control.name,
        binding(control.keyboard),
        binding(control.controller)
    ))
}

pub(super) fn ped_flag_documentation(id: u32) -> Option<String> {
    let flag = ped_config_flag(id)?;
    let description = flag.description.unwrap_or("Behavior is not documented in the bundled Cfx reference.");
    Some(format!(
        "**Ped config flag `{id}`**\n\n`{}`\n\n{description}\n\n\
         The Cfx reference includes potential names and hash collisions.\n\n\
         [Cfx ped config flags reference]({PED_CONFIG_FLAGS_SOURCE_URL})",
        flag.name
    ))
}

/// Enum documentation belongs to a literal passed directly to a recognized native parameter.
/// Never infer an enum from a nearby token, arbitrary number or a user function of the same name.
pub fn hover(ws: &Workspace, doc: &Document, offset: u32) -> Option<(String, Span)> {
    let call = locate(&doc.chunk, offset).call?;
    if call.method.is_some() {
        return None;
    }
    let (index, argument) = call.args.iter().enumerate().find(|(_, arg)| arg.span.contains(offset))?;
    let argument = argument.unparen();
    if !argument.span.contains(offset) {
        return None;
    }
    let ExprKind::Number(value) = argument.kind else { return None };
    if doc.chunk.errors.iter().any(|error| error.span.start < argument.span.end && error.span.end > argument.span.start)
    {
        return None;
    }
    let id = literal_id(value)?;
    // A qualified or method call can be an unrelated wrapper, even when its last segment matches.
    let ExprKind::Name(name) = &call.base.kind else { return None };
    if !matches!(doc.resolution.resolve_at(name.span.start), Some(Resolved::Global(_)))
        || !ws.index.globals_named(&name.text, doc.file).is_empty()
        || doc.resolution.lookup_local_at("_ENV", name.span.start).is_some()
        || ws
            .index
            .globals_named("_ENV", doc.file)
            .iter()
            .any(|(file, _)| ws.index.file(*file).is_some_and(|entry| entry.origin != FileOrigin::Stub))
    {
        return None;
    }
    let native = named_native(&name.text)?;
    let text = match argument_kind(native, index)? {
        ArgumentKind::Control => control_documentation(id)?,
        ArgumentKind::PedConfigFlag => ped_flag_documentation(id)?,
    };
    Some((text, argument.span))
}
