use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::Duration;

use lsp_server::{Connection, Message, Notification, Request, RequestId};
use lsp_types::Url;
use serde_json::{json, Value};

struct Client {
    connection: Connection,
    server: Option<JoinHandle<()>>,
    next_id: i32,
    diagnostics: HashMap<String, Value>,
    root: PathBuf,
}

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/resources")
}

impl Client {
    fn start(root: PathBuf) -> Self {
        let (server_side, client_side) = Connection::memory();
        let server = std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(move || qbx_lua_ls::server::run_connection(server_side).expect("server failed"))
            .unwrap();
        let mut client =
            Client { connection: client_side, server: Some(server), next_id: 0, diagnostics: HashMap::new(), root };
        let root_uri = Url::from_file_path(&client.root).unwrap();
        let result = client.request(
            "initialize",
            json!({ "processId": null, "rootUri": root_uri, "capabilities": {}, "workspaceFolders": [{ "uri": root_uri, "name": "fixture" }] }),
        );
        assert!(result["capabilities"]["completionProvider"].is_object());
        client.notify("initialized", json!({}));
        client
    }

    fn notify(&self, method: &str, params: Value) {
        self.connection.sender.send(Message::Notification(Notification { method: method.into(), params })).unwrap();
    }

    fn handle_incoming(&mut self, message: Message) -> Option<(RequestId, Value)> {
        match message {
            Message::Response(response) => {
                assert!(response.error.is_none(), "server returned an error: {:?}", response.error);
                return Some((response.id, response.result.unwrap_or(Value::Null)));
            }
            Message::Notification(n) if n.method == "textDocument/publishDiagnostics" => {
                let uri = n.params["uri"].as_str().unwrap().to_string();
                self.diagnostics.insert(uri, n.params["diagnostics"].clone());
            }
            Message::Request(request) => {
                let reply = lsp_server::Response { id: request.id, result: Some(Value::Null), error: None };
                self.connection.sender.send(Message::Response(reply)).unwrap();
            }
            Message::Notification(_) => {}
        }
        None
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = RequestId::from(self.next_id);
        self.connection
            .sender
            .send(Message::Request(Request { id: id.clone(), method: method.into(), params }))
            .unwrap();
        loop {
            let message =
                self.connection.receiver.recv_timeout(Duration::from_secs(20)).expect("server did not answer");
            if let Some((response_id, result)) = self.handle_incoming(message) {
                if response_id == id {
                    return result;
                }
            }
        }
    }

    fn uri(&self, relative: &str) -> Url {
        Url::from_file_path(self.root.join(relative)).unwrap()
    }

    fn open(&mut self, relative: &str) -> String {
        let text = std::fs::read_to_string(self.root.join(relative)).unwrap().replace("\r\n", "\n");
        self.open_with(relative, &text);
        text
    }

    fn open_with(&mut self, relative: &str, text: &str) {
        let uri = self.uri(relative);
        self.notify(
            "textDocument/didOpen",
            json!({ "textDocument": { "uri": uri, "languageId": "lua", "version": 1, "text": text } }),
        );
    }

    fn change(&mut self, relative: &str, version: i32, text: &str) {
        let uri = self.uri(relative);
        self.notify(
            "textDocument/didChange",
            json!({ "textDocument": { "uri": uri, "version": version }, "contentChanges": [{ "text": text }] }),
        );
    }

    /// Diagnostics are published once the server is idle, so round-trip a request first.
    fn diagnostics_for(&mut self, relative: &str) -> Vec<(String, u64)> {
        self.request("qbx/status", Value::Null);
        while let Ok(message) = self.connection.receiver.recv_timeout(Duration::from_millis(300)) {
            self.handle_incoming(message);
        }
        let uri = self.uri(relative).to_string();
        let list = self.diagnostics.get(&uri).cloned().unwrap_or(json!([]));
        list.as_array()
            .unwrap()
            .iter()
            .map(|d| (d["code"].as_str().unwrap().to_string(), d["range"]["start"]["line"].as_u64().unwrap()))
            .collect()
    }

    fn position_params(&self, relative: &str, line: u32, character: u32) -> Value {
        json!({ "textDocument": { "uri": self.uri(relative) }, "position": { "line": line, "character": character } })
    }

    fn completion_labels(&mut self, relative: &str, line: u32, character: u32) -> Vec<String> {
        let result = self.request("textDocument/completion", self.position_params(relative, line, character));
        result["items"]
            .as_array()
            .map(|items| items.iter().map(|i| i["label"].as_str().unwrap().to_string()).collect())
            .unwrap_or_default()
    }

    fn hover_text(&mut self, relative: &str, line: u32, character: u32) -> String {
        let result = self.request("textDocument/hover", self.position_params(relative, line, character));
        result["contents"]["value"].as_str().unwrap_or_default().to_string()
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.next_id += 1;
        let shutdown = Request { id: RequestId::from(self.next_id), method: "shutdown".into(), params: Value::Null };
        let _ = self.connection.sender.send(Message::Request(shutdown));
        let _ = self.connection.receiver.recv_timeout(Duration::from_secs(5));
        self.notify("exit", Value::Null);
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
    }
}

/// Finds `needle` in `text` and returns the position `delta` characters into it.
fn pos(text: &str, needle: &str, delta: u32) -> (u32, u32) {
    let offset = text.find(needle).unwrap_or_else(|| panic!("{needle:?} not found"));
    let line = text[..offset].matches('\n').count() as u32;
    let col = (offset - text[..offset].rfind('\n').map_or(0, |i| i + 1)) as u32;
    (line, col + delta)
}

const CLIENT: &str = "myresource/client/main.lua";
const SERVER: &str = "myresource/server/main.lua";

#[test]
fn indexes_the_workspace_and_reports_status() {
    let mut client = Client::start(fixture_root());
    let status = client.request("qbx/status", Value::Null);
    assert_eq!(status["resources"], 5);
    assert!(status["files"].as_u64().unwrap() >= 10, "{status}");
}

#[test]
fn hover_shows_types_docs_and_natives() {
    let mut client = Client::start(fixture_root());
    let text = client.open(CLIENT);

    let (l, c) = pos(&text, "MyLib.round(1.2345", 7);
    let hover = client.hover_text(CLIENT, l, c);
    assert!(hover.contains("function MyLib.round(value: number, decimals?: integer): number"), "{hover}");
    assert!(hover.contains("Rounds a number"), "{hover}");
    assert!(hover.contains("`value`: the value to round"), "{hover}");

    let (l, c) = pos(&text, "local garage", 7);
    assert!(client.hover_text(CLIENT, l, c).contains("local garage: Garage"));

    let (l, c) = pos(&text, "local count", 7);
    assert!(client.hover_text(CLIENT, l, c).contains("local count: integer"));

    let (l, c) = pos(&text, "ok, reason", 4);
    assert!(client.hover_text(CLIENT, l, c).contains("local reason: string?"));

    let (l, c) = pos(&text, "local coords", 7);
    assert!(client.hover_text(CLIENT, l, c).contains("local coords: vector3"));

    let (l, c) = pos(&text, "local distance", 7);
    assert!(client.hover_text(CLIENT, l, c).contains("local distance: number"));

    let (l, c) = pos(&text, "GetEntityCoords", 3);
    let hover = client.hover_text(CLIENT, l, c);
    assert!(hover.contains("function GetEntityCoords(entity: Entity, alive: boolean): vector3"), "{hover}");
    assert!(hover.contains("native"), "{hover}");

    let (l, c) = pos(&text, "garage:getVehicleCount", 8);
    let hover = client.hover_text(CLIENT, l, c);
    assert!(hover.contains("Garage:getVehicleCount"), "{hover}");
    assert!(hover.contains("how many vehicles"), "{hover}");

    let (l, c) = pos(&text, "settings.maxGarages", 10);
    assert!(client.hover_text(CLIENT, l, c).contains("maxGarages: integer"));

    let (l, c) = pos(&text, "Config.Garages.legion.label", 23);
    assert!(client.hover_text(CLIENT, l, c).contains("label: string"));
}

#[test]
fn hover_resolves_exports_across_resources() {
    let mut client = Client::start(fixture_root());
    let text = client.open(SERVER);
    let (l, c) = pos(&text, "GetPlayer(src)", 3);
    let hover = client.hover_text(SERVER, l, c);
    assert!(hover.contains("GetPlayer(source: integer)"), "{hover}");
    assert!(hover.contains("Looks a player up"), "{hover}");

    let (l, c) = pos(&text, "player.name", 8);
    assert!(client.hover_text(SERVER, l, c).contains("name: string"));
}

#[test]
fn completes_members_globals_natives_and_events() {
    let mut client = Client::start(fixture_root());
    let text = client.open(CLIENT);
    let lines = text.lines().count() as u32;

    let with_line = |client: &mut Client, extra: &str, version: i32| {
        let changed = format!("{text}{extra}");
        client.change(CLIENT, version, &changed);
        (lines, extra.len() as u32)
    };

    let (l, c) = with_line(&mut client, "MyLib.", 2);
    let labels = client.completion_labels(CLIENT, l, c);
    for expected in ["round", "createGarage", "math", "version"] {
        assert!(labels.contains(&expected.to_string()), "{expected} missing from {labels:?}");
    }

    let (l, c) = with_line(&mut client, "garage:", 3);
    let labels = client.completion_labels(CLIENT, l, c);
    assert!(labels.contains(&"getVehicleCount".to_string()) && labels.contains(&"store".to_string()), "{labels:?}");
    assert!(!labels.contains(&"kind".to_string()), "fields should not be offered after ':' {labels:?}");

    let (l, c) = with_line(&mut client, "garage.point.", 4);
    assert_eq!(client.completion_labels(CLIENT, l, c), ["coords", "label", "slots"]);

    let (l, c) = with_line(&mut client, "local s = ('x'):", 5);
    assert!(client.completion_labels(CLIENT, l, c).contains(&"format".to_string()));

    let (l, c) = with_line(&mut client, "exports.", 6);
    let mut resources = client.completion_labels(CLIENT, l, c);
    resources.sort();
    assert_eq!(resources, ["late", "mylib", "myresource", "shop", "vault"]);

    let (l, c) = with_line(&mut client, "exports.mylib:", 7);
    let labels = client.completion_labels(CLIENT, l, c);
    assert!(labels.contains(&"GetPlayer".to_string()) && labels.contains(&"Ping".to_string()), "{labels:?}");

    let (l, c) = with_line(&mut client, "GetEntityCo", 8);
    let labels = client.completion_labels(CLIENT, l, c);
    assert!(labels.contains(&"GetEntityCoords".to_string()), "{labels:?}");

    let (l, c) = with_line(&mut client, "local z = Conf", 9);
    assert!(client.completion_labels(CLIENT, l, c).contains(&"Config".to_string()));

    let (l, c) = with_line(&mut client, "local z = roun", 10);
    assert!(client.completion_labels(CLIENT, l, c).contains(&"rounded".to_string()));

    let (l, _) = with_line(&mut client, "TriggerServerEvent('')", 11);
    let labels = client.completion_labels(CLIENT, l, 20);
    assert!(labels.contains(&"myresource:server:ping".to_string()), "{labels:?}");

    let (l, _) = with_line(&mut client, "MyLib.createGarage('public', {  })", 12);
    let labels = client.completion_labels(CLIENT, l, 31);
    assert_eq!(labels, ["coords", "label", "slots"]);

    let (l, c) = with_line(&mut client, "---@type Gar", 13);
    let labels = client.completion_labels(CLIENT, l, c);
    assert!(labels.contains(&"Garage".to_string()) && labels.contains(&"GarageKind".to_string()), "{labels:?}");
}

#[test]
fn server_side_completion_hides_client_natives() {
    let mut client = Client::start(fixture_root());
    let text = client.open(SERVER);
    let changed = format!("{text}PlayerPed");
    client.change(SERVER, 2, &changed);
    let line = text.lines().count() as u32;
    let labels = client.completion_labels(SERVER, line, 9);
    assert!(!labels.contains(&"PlayerPedId".to_string()), "{labels:?}");

    let changed = format!("{text}GetPlayerIdent");
    client.change(SERVER, 3, &changed);
    assert!(client.completion_labels(SERVER, line, 14).contains(&"GetPlayerIdentifierByType".to_string()));
}

#[test]
fn goes_to_definitions_across_files() {
    let mut client = Client::start(fixture_root());
    let text = client.open(CLIENT);

    let (l, c) = pos(&text, "MyLib.round", 8);
    let result = client.request("textDocument/definition", client.position_params(CLIENT, l, c));
    assert!(result[0]["uri"].as_str().unwrap().ends_with("mylib/init.lua"), "{result}");
    assert_eq!(result[0]["range"]["start"]["line"], 16);

    let (l, c) = pos(&text, "require '@mylib.modules.settings'", 12);
    let result = client.request("textDocument/definition", client.position_params(CLIENT, l, c));
    assert!(result[0]["uri"].as_str().unwrap().ends_with("modules/settings.lua"), "{result}");

    let (l, c) = pos(&text, "'myresource:server:ping'", 5);
    let result = client.request("textDocument/definition", client.position_params(CLIENT, l, c));
    assert!(result[0]["uri"].as_str().unwrap().ends_with("server/main.lua"), "{result}");

    let (l, c) = pos(&text, "print(message, kind, count", 21);
    let result = client.request("textDocument/definition", client.position_params(CLIENT, l, c));
    assert_eq!(result[0]["range"]["start"]["line"], 2);
}

#[test]
fn signature_help_tracks_the_active_parameter() {
    let mut client = Client::start(fixture_root());
    let text = client.open(CLIENT);
    let (l, c) = pos(&text, "MyLib.round(1.2345, 2)", 20);
    let result = client.request("textDocument/signatureHelp", client.position_params(CLIENT, l, c));
    assert_eq!(result["signatures"][0]["label"], "MyLib.round(value: number, decimals?: integer): number");
    assert_eq!(result["activeParameter"], 1);

    let (l, c) = pos(&text, "garage:store('ABC123')", 14);
    let result = client.request("textDocument/signatureHelp", client.position_params(CLIENT, l, c));
    assert_eq!(result["signatures"][0]["label"], "store(plate: string): boolean, string?");
}

#[test]
fn publishes_lint_diagnostics_with_resource_context() {
    let mut client = Client::start(fixture_root());
    client.open(CLIENT);
    assert_eq!(client.diagnostics_for(CLIENT), []);

    let text = client.open(SERVER);
    let found = client.diagnostics_for(SERVER);
    assert_eq!(found, [("fivem/import-not-declared".to_string(), 8), ("unused-argument".to_string(), 8)], "{found:?}");

    let broken = format!("{text}\nlocal ped = PlayerPedId()\nprint(notDefinedAnywhere)\n");
    client.change(SERVER, 2, &broken);
    let codes: Vec<String> = client.diagnostics_for(SERVER).into_iter().map(|(code, _)| code).collect();
    assert!(codes.contains(&"fivem/native-wrong-side".to_string()), "{codes:?}");
    assert!(codes.contains(&"undefined-global".to_string()), "{codes:?}");
    assert!(codes.contains(&"unused-local".to_string()), "{codes:?}");
}

#[test]
fn references_rename_and_symbols() {
    let mut client = Client::start(fixture_root());
    let text = client.open(CLIENT);

    let (l, c) = pos(&text, "local count", 7);
    let mut params = client.position_params(CLIENT, l, c);
    params["context"] = json!({ "includeDeclaration": true });
    let refs = client.request("textDocument/references", params);
    assert_eq!(refs.as_array().unwrap().len(), 2);

    let (l, c) = pos(&text, "Config.SpawnDistance", 2);
    let mut params = client.position_params(CLIENT, l, c);
    params["context"] = json!({ "includeDeclaration": true });
    let refs = client.request("textDocument/references", params);
    let files: Vec<&str> = refs.as_array().unwrap().iter().map(|r| r["uri"].as_str().unwrap()).collect();
    assert!(
        files.iter().any(|f| f.ends_with("shared/config.lua")) && files.iter().any(|f| f.ends_with("server/main.lua")),
        "{files:?}"
    );

    let mut params = client.position_params(CLIENT, l, c);
    params["newName"] = json!("Settings");
    let edit = client.request("textDocument/rename", params);
    assert!(edit["changes"].as_object().unwrap().len() >= 3, "{edit}");

    let symbols =
        client.request("textDocument/documentSymbol", json!({ "textDocument": { "uri": client.uri(CLIENT) } }));
    let names: Vec<&str> = symbols.as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"garage") && names.contains(&"RegisterNetEvent 'myresource:client:notify'"), "{names:?}");

    let found = client.request("workspace/symbol", json!({ "query": "garage" }));
    let names: Vec<&str> = found.as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"MyLib.createGarage") && names.contains(&"Garage"), "{names:?}");
}

#[test]
fn code_actions_inlay_hints_tokens_and_folding() {
    let mut client = Client::start(fixture_root());
    let text = "Citizen.CreateThread(function()\n    local hash = GetHashKey('adder')\n    SetEntityCoords(hash, 1.0, 2.0, 3.0, false, false, false, true)\nend)\n";
    client.open_with(CLIENT, text);
    client.diagnostics_for(CLIENT);
    let uri = client.uri(CLIENT).to_string();
    let diagnostics = client.diagnostics[&uri].clone();
    let actions = client.request(
        "textDocument/codeAction",
        json!({ "textDocument": { "uri": uri }, "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 3, "character": 0 } }, "context": { "diagnostics": diagnostics } }),
    );
    let titles: Vec<&str> = actions.as_array().unwrap().iter().map(|a| a["title"].as_str().unwrap()).collect();
    assert!(titles.contains(&"Replace with 'CreateThread'"), "{titles:?}");
    assert!(titles.contains(&"Convert to a compile-time hash literal"), "{titles:?}");
    assert!(titles.iter().any(|t| t.starts_with("Disable fivem/hash-literal")), "{titles:?}");

    let hints = client.request(
        "textDocument/inlayHint",
        json!({ "textDocument": { "uri": uri }, "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 4, "character": 0 } } }),
    );
    let labels: Vec<&str> = hints.as_array().unwrap().iter().map(|h| h["label"].as_str().unwrap()).collect();
    assert_eq!(&labels[..3], ["string:", "xPos:", "yPos:"], "{labels:?}");

    let tokens = client.request("textDocument/semanticTokens/full", json!({ "textDocument": { "uri": uri } }));
    assert!(tokens["data"].as_array().unwrap().len() >= 5 * 6);

    let folds = client.request("textDocument/foldingRange", json!({ "textDocument": { "uri": uri } }));
    assert_eq!(folds[0]["startLine"], 0);
}

#[test]
fn survives_garbage_input_while_typing() {
    let mut client = Client::start(fixture_root());
    let text = client.open(CLIENT);
    for (version, cut) in (2..).zip((0..text.len()).step_by(37)) {
        if !text.is_char_boundary(cut) {
            continue;
        }
        client.change(CLIENT, version, &text[..cut]);
        let line = text[..cut].matches('\n').count() as u32;
        let col = (cut - text[..cut].rfind('\n').map_or(0, |i| i + 1)) as u32;
        client.request("textDocument/completion", client.position_params(CLIENT, line, col));
        client.request("textDocument/hover", client.position_params(CLIENT, line, col.saturating_sub(1)));
        client.request("textDocument/signatureHelp", client.position_params(CLIENT, line, col));
    }
}

#[test]
fn reports_problems_for_files_that_are_not_open() {
    let mut client = Client::start(fixture_root());
    let found = client.diagnostics_for(SERVER);
    assert_eq!(found, [("fivem/import-not-declared".to_string(), 8)], "{found:?}");
    assert_eq!(client.diagnostics_for(CLIENT), []);
}

#[test]
fn event_completion_follows_the_call_direction() {
    let mut client = Client::start(fixture_root());
    let text = client.open(CLIENT);
    let line = text.lines().count() as u32;

    client.change(CLIENT, 2, &format!("{text}TriggerServerEvent('')"));
    let labels = client.completion_labels(CLIENT, line, 20);
    assert_eq!(labels, ["myresource:server:ping", "shop:buy", "shop:refund"], "only server handlers are reachable");

    client.change(CLIENT, 3, &format!("{text}TriggerEvent('')"));
    let labels = client.completion_labels(CLIENT, line, 14);
    assert_eq!(labels, ["myresource:client:notify", "shop:bought"]);
}

#[test]
fn reports_the_side_of_a_file() {
    let mut client = Client::start(fixture_root());
    let info = client.request("qbx/fileInfo", json!({ "uri": client.uri(CLIENT) }));
    assert_eq!(info, json!({ "side": "client", "resource": "myresource" }));
    let info = client.request("qbx/fileInfo", json!({ "uri": client.uri("myresource/shared/config.lua") }));
    assert_eq!(info["side"], "shared");
    let info = client.request("qbx/fileInfo", json!({ "uri": client.uri("[core]/mylib/modules/settings.lua") }));
    assert_eq!(info["side"], "module");
}

const SHOP_CLIENT: &str = "shop/client.lua";
const SHOP_SERVER: &str = "shop/server.lua";

#[test]
fn cross_file_rules_run_in_the_editor() {
    let mut client = Client::start(fixture_root());
    let found = client.diagnostics_for(SHOP_CLIENT);
    let expected = [
        ("qbox/unknown-locale-key", 4),
        ("fivem/event-argument-count", 7),
        ("fivem/event-wrong-side", 8),
        ("fivem/export-argument-count", 9),
        ("manifest/missing-dependency", 9),
    ];
    for (code, line) in expected {
        assert!(found.contains(&(code.to_string(), line)), "{code} on line {line} missing from {found:?}");
    }

    let codes: Vec<String> = client.diagnostics_for(SHOP_SERVER).into_iter().map(|(code, _)| code).collect();
    for code in ["security/client-supplied-source", "security/unvalidated-event-argument", "security/sql-concatenation"]
    {
        assert!(codes.contains(&code.to_string()), "{code} missing from {codes:?}");
    }
    assert!(codes.contains(&"fivem/event-argument-count".to_string()), "shop:bought takes one argument: {codes:?}");

    let unused = client.diagnostics_for("shop/locales/en.json");
    assert_eq!(unused, [("qbox/unused-locale-key".to_string(), 3), ("qbox/unused-locale-key".to_string(), 5)]);
}

#[test]
fn completes_locale_keys_convars_and_state_bags() {
    let mut client = Client::start(fixture_root());
    let text = client.open(SHOP_CLIENT);
    let line = text.lines().count() as u32;

    client.change(SHOP_CLIENT, 2, &format!("{text}print(locale(''))"));
    assert_eq!(client.completion_labels(SHOP_CLIENT, line, 14), ["buy.success", "buy.failed", "never_used"]);

    client.change(SHOP_CLIENT, 3, &format!("{text}print(GetConvarInt(''))"));
    assert_eq!(client.completion_labels(SHOP_CLIENT, line, 20), ["shop_debug"]);

    client.change(SHOP_CLIENT, 4, &format!("{text}print(LocalPlayer.state.)"));
    assert!(client.completion_labels(SHOP_CLIENT, line, 24).contains(&"isShopping".to_string()));

    let (l, c) = pos(&text, "'buy.success'", 3);
    assert!(client.hover_text(SHOP_CLIENT, l, c).contains("You bought %s"));
    let definition = client.request("textDocument/definition", client.position_params(SHOP_CLIENT, l, c));
    assert!(definition[0]["uri"].as_str().unwrap().ends_with("locales/en.json"), "{definition}");
    assert_eq!(definition[0]["range"]["start"]["line"], 2);
}

#[test]
fn finds_and_renames_fields_across_files() {
    let mut client = Client::start(fixture_root());
    let text = client.open(SHOP_CLIENT);
    let (l, c) = pos(&text, "Shop.getPrice", 7);

    let mut params = client.position_params(SHOP_CLIENT, l, c);
    params["context"] = json!({ "includeDeclaration": true });
    let refs = client.request("textDocument/references", params);
    let mut files: Vec<String> = refs
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["uri"].as_str().unwrap().rsplit('/').next().unwrap().to_string())
        .collect();
    files.sort();
    assert_eq!(files, ["client.lua", "server.lua", "shared.lua"], "{refs}");

    let mut params = client.position_params(SHOP_CLIENT, l, c);
    params["newName"] = json!("priceOf");
    let edit = client.request("textDocument/rename", params);
    assert_eq!(edit["changes"].as_object().unwrap().len(), 3, "{edit}");

    let (l, c) = pos(&text, "exports.mylib:Ping", 15);
    let prepared = client.request("textDocument/prepareRename", client.position_params(SHOP_CLIENT, l, c));
    assert!(prepared.is_object(), "exports defined in the workspace can be renamed: {prepared}");
}

#[test]
fn formats_documents() {
    let mut client = Client::start(fixture_root());
    client.open_with(SHOP_CLIENT, "local   a=1\nif a   then\nprint( a )\nend\n");
    let params = json!({ "textDocument": { "uri": client.uri(SHOP_CLIENT) }, "options": { "tabSize": 2, "insertSpaces": true } });
    let edits = client.request("textDocument/formatting", params);
    assert_eq!(edits[0]["newText"], "local a = 1\nif a then\n  print(a)\nend\n");

    client.change(SHOP_CLIENT, 2, "local a = 1\n");
    let params = json!({ "textDocument": { "uri": client.uri(SHOP_CLIENT) }, "options": { "tabSize": 4, "insertSpaces": true } });
    assert_eq!(client.request("textDocument/formatting", params), json!([]));
}

#[test]
fn server_cfg_start_order_settles_dependencies() {
    let mut client = Client::start(fixture_root());
    let late = client.diagnostics_for("late/server.lua");
    assert_eq!(late, [], "server.cfg ensures [core] before late, so mylib is already running");

    let uri = client.uri(SHOP_CLIENT).to_string();
    client.diagnostics_for(SHOP_CLIENT);
    let messages: Vec<String> = client.diagnostics[&uri]
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| d["code"] == "manifest/missing-dependency")
        .map(|d| d["message"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(messages.len(), 1, "shop is ensured before [core]: {messages:?}");
    assert!(messages[0].contains("server.cfg does not start it earlier"), "{messages:?}");
}

#[test]
fn bridge_code_is_not_a_dependency_but_missing_resources_are_reported() {
    let mut client = Client::start(fixture_root());
    let bridge = client.diagnostics_for("late/bridge.lua");
    assert_eq!(bridge, [("fivem/resource-not-found".to_string(), 10)], "only the unconditional call matters");
    assert_eq!(client.diagnostics_for("late/guarded.lua"), [], "everything after the selector guard is optional");
}

#[test]
fn exports_of_escrowed_resources_are_not_second_guessed() {
    let mut client = Client::start(fixture_root());
    assert_eq!(
        client.diagnostics_for("late/hidden.lua"),
        [("fivem/unknown-export".to_string(), 1)],
        "vault has an encrypted file that may register anything; mylib is fully readable"
    );
}

#[test]
fn completes_resources_and_exports_in_both_spellings() {
    let mut client = Client::start(fixture_root());
    let text = client.open(CLIENT);
    let line = text.lines().count() as u32;

    client.change(CLIENT, 2, &format!("{text}exports['']"));
    let mut resources = client.completion_labels(CLIENT, line, 9);
    resources.sort();
    assert_eq!(resources, ["late", "mylib", "myresource", "shop", "vault"]);

    client.change(CLIENT, 3, &format!("{text}exports['mylib']:"));
    let labels = client.completion_labels(CLIENT, line, 17);
    assert!(labels.contains(&"GetPlayer".to_string()) && labels.contains(&"Ping".to_string()), "{labels:?}");

    client.change(CLIENT, 4, &format!("{text}exports.mylib:GetPlayer(1)"));
    let hover = client.hover_text(CLIENT, line, 16);
    assert!(hover.contains("GetPlayer(source: integer)") && hover.contains("Looks a player up"), "{hover}");
}

#[test]
fn table_hover_lists_only_the_fields_in_scope() {
    let mut client = Client::start(fixture_root());
    let text = client.open(CLIENT);
    let (l, c) = pos(&text, "Config.SpawnDistance", 2);
    let hover = client.hover_text(CLIENT, l, c);
    let expected = "```lua\n(global) Config: {\n    Debug: boolean = true,\n    SpawnDistance: number = 25.0,\n    Garages: table,\n    isDebug: function,\n}\n```";
    assert!(hover.starts_with(expected), "{hover}");
    assert!(!hover.contains("ShopName"), "the shop resource has its own Config: {hover}");
    assert!(hover.contains("myresource/shared/config.lua"), "{hover}");

    let (l, c) = pos(&text, "Config.SpawnDistance", 10);
    assert!(client.hover_text(CLIENT, l, c).contains("(field) Config.SpawnDistance: number = 25.0"));

    let shop = client.open(SHOP_CLIENT);
    client.change(SHOP_CLIENT, 2, &format!("{shop}print(Config)"));
    let hover = client.hover_text(SHOP_CLIENT, shop.lines().count() as u32, 8);
    assert!(
        hover.contains("ShopName: string = 'General Store'") && hover.contains("OpenAtNight: boolean = false"),
        "{hover}"
    );
    assert!(!hover.contains("SpawnDistance"), "{hover}");
}

#[test]
fn escrow_encrypted_files_are_ignored() {
    let mut client = Client::start(fixture_root());
    assert_eq!(client.diagnostics_for("vault/escrowed.lua"), [], "closed escrowed files are not linted");

    client.open_with("vault/escrowed.lua", "FXAP\u{1}\u{fffd}\u{fffd}garbage(((");
    assert_eq!(client.diagnostics_for("vault/escrowed.lua"), [], "nor are they when opened in the editor");

    client.open_with(SHOP_CLIENT, &"local = = =\n".repeat(200));
    let found = client.diagnostics_for(SHOP_CLIENT);
    assert_eq!(found.len(), 11, "syntax errors are capped at ten plus a summary: {found:?}");
}
