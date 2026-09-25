use qbx_fivem_data::Side;
use qbx_lua_analysis::crossref::trigger_target;
use qbx_lua_syntax::ast::Expr;

use crate::document::Document;
use crate::index::EventKind;
use crate::types::{FunType, Param, Type};
use crate::workspace::Workspace;

pub struct EventCall {
    pub fun: FunType,
    pub event: String,
    /// Where the handler whose parameters are shown lives, e.g. `server/main.lua:12`.
    pub handler_location: String,
}

/// Leading arguments of `lib.callback` style calls that are not passed on to the handler.
fn callback_skip(call: &str) -> Option<usize> {
    match call {
        "lib.callback.await" => Some(2),
        "lib.callback" => Some(3),
        _ => None,
    }
}

/// For `TriggerServerEvent('name', ...)` and friends: the signature of the call with the payload
/// parameters taken from the handler registered for that event.
pub fn event_call(ws: &Workspace, doc: &Document, callee: &Expr, args: &[Expr], native: &FunType) -> Option<EventCall> {
    let call = callee.dotted_path()?;
    let name = args.first()?.as_string()?;
    let own_side = ws.index.file(doc.file).and_then(|f| f.side);
    let own_side =
        qbx_lua_analysis::side_guard::SideRegions::of(&doc.text, &doc.chunk).effective(callee.span.start, own_side);

    let (target, skip, wanted) = match trigger_target(&call, own_side) {
        Some((target, skip)) => (target, skip, [EventKind::NetEvent, EventKind::Handler]),
        None => {
            let target = own_side.map(|side| if side == Side::Server { Side::Client } else { Side::Server });
            (target, callback_skip(&call)?, [EventKind::Callback, EventKind::Callback])
        }
    };

    let candidates = ws
        .index
        .events()
        .filter(|(_, e)| e.name == *name && wanted.contains(&e.kind) && e.handler.is_some())
        .filter(|(_, e)| !matches!((target, e.side), (Some(target), Some(side)) if !side.is_available_on(target)));
    let (file, event) = candidates.max_by_key(
        |(_, event)| matches!((target, event.side), (Some(target), Some(side)) if side.is_available_on(target)),
    )?;
    let handler = event.handler.as_deref()?;
    let entry = ws.index.file(file)?;

    // Server callbacks receive the calling player as their first parameter.
    let is_callback = event.kind == EventKind::Callback;
    let drops_source = is_callback && event.side != Some(Side::Client) && !handler.params.is_empty();

    let mut params: Vec<Param> = native.params.iter().take(skip).cloned().collect();
    while params.len() < skip {
        params.push(Param { name: format!("arg{}", params.len() + 1).into(), ty: Type::Unknown, optional: false });
    }
    params.extend(handler.params.iter().skip(usize::from(drops_source)).cloned());

    let file_name = entry.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let resource = entry.resource.and_then(|id| ws.index.resource(id)).map(|r| format!("{}/", r.name));
    Some(EventCall {
        fun: FunType {
            params,
            returns: if is_callback { handler.returns.clone() } else { Vec::new() },
            is_method: false,
            generics: Vec::new(),
            overloads: Vec::new(),
        },
        event: name.to_string(),
        handler_location: format!("{}{file_name}:{}", resource.unwrap_or_default(), event.range.start.line + 1),
    })
}
