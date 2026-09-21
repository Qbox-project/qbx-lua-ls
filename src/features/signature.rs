use lsp_types::{Documentation, ParameterInformation, ParameterLabel, Position, SignatureHelp, SignatureInformation};
use qbx_fivem_data::native_docs;
use qbx_lua_syntax::ast::ExprKind;

use super::event_call::event_call;
use super::{markdown, with_infer};
use crate::document::Document;
use crate::locate::locate;
use crate::workspace::Workspace;

pub fn signature_help(ws: &Workspace, doc: &Document, position: Position) -> Option<SignatureHelp> {
    let offset = doc.offset(position);
    let located = locate(&doc.chunk, offset);
    let site = located.call?;
    with_infer(ws, doc, |infer| {
        let (fun, member) = infer.callee_fun(site.base, site.method)?;
        let event = site.method.is_none().then(|| event_call(ws, doc, site.base, site.args, &fun)).flatten();
        let fun = event.as_ref().map_or(fun, |e| e.fun.clone().into());
        let (skip_params, skip_args) = fun.call_offsets(site.method.is_some());
        let params: Vec<_> = fun.params.iter().skip(skip_params).collect();

        let name = match (site.method, &site.base.kind) {
            (Some(method), _) => method.text.to_string(),
            (None, _) => site.base.dotted_path().unwrap_or_default(),
        };
        let labels: Vec<String> = params.iter().map(|p| p.to_string()).collect();
        let mut label = format!("{name}({})", labels.join(", "));
        if !fun.returns.is_empty() {
            let returns: Vec<String> = fun.returns.iter().map(|r| r.to_string()).collect();
            label.push_str(&format!(": {}", returns.join(", ")));
        }

        let mut documentation = member.and_then(|m| m.doc).map(|d| d.to_string());
        if documentation.is_none() {
            if let ExprKind::Name(global) = &site.base.kind {
                documentation = ws
                    .index
                    .globals_named(&global.text, doc.file)
                    .into_iter()
                    .find_map(|(_, s)| s.doc.as_ref().map(|d| d.to_string()))
                    .or_else(|| native_docs(&global.text));
            }
        }

        if let Some(event) = &event {
            let note = format!("Parameters of `{}` as handled in `{}`.", event.event, event.handler_location);
            documentation = Some(documentation.map_or(note.clone(), |d| {
                format!(
                    "{note}

{d}"
                )
            }));
        }

        let mut active = site.active_argument(&doc.text, offset).saturating_sub(skip_args);
        let is_variadic = params.last().is_some_and(|p| p.name == "...");
        if active >= params.len() && is_variadic {
            active = params.len() - 1;
        }
        Some(SignatureHelp {
            signatures: vec![SignatureInformation {
                label,
                documentation: documentation.map(|d| Documentation::MarkupContent(markdown(d))),
                parameters: Some(
                    labels
                        .into_iter()
                        .map(|l| ParameterInformation { label: ParameterLabel::Simple(l), documentation: None })
                        .collect(),
                ),
                active_parameter: Some(active as u32),
            }],
            active_signature: Some(0),
            active_parameter: Some(active as u32),
        })
    })
}
