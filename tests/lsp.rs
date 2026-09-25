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
    registrations: Vec<Value>,
    logs: Vec<String>,
    root: PathBuf,
}

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/resources")
}

impl Client {
    fn start(root: PathBuf) -> Self {
        Self::start_with_capabilities(
            root,
            json!({
                "workspace": { "didChangeWatchedFiles": { "dynamicRegistration": true } },
                "textDocument": { "completion": { "completionItem": { "snippetSupport": true } } }
            }),
        )
    }

    fn start_with_capabilities(root: PathBuf, capabilities: Value) -> Self {
        let (server_side, client_side) = Connection::memory();
        let server = std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(move || qbx_lua_ls::server::run_connection(server_side).expect("server failed"))
            .unwrap();
        let mut client = Client {
            connection: client_side,
            server: Some(server),
            next_id: 0,
            diagnostics: HashMap::new(),
            registrations: Vec::new(),
            logs: Vec::new(),
            root,
        };
        let root_uri = Url::from_file_path(&client.root).unwrap();
        let result = client.request(
            "initialize",
            json!({ "processId": null, "rootUri": root_uri, "capabilities": capabilities, "workspaceFolders": [{ "uri": root_uri, "name": "fixture" }] }),
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
            Message::Notification(n) if n.method == "window/logMessage" => {
                self.logs.push(n.params["message"].as_str().unwrap_or_default().to_string());
            }
            Message::Request(request) => {
                if request.method == "client/registerCapability" {
                    self.registrations.extend(request.params["registrations"].as_array().unwrap().iter().cloned());
                }
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
fn hover_shows_annotation_type_details_and_ranges() {
    let mut client = Client::start(fixture_root());
    let declarations = "\
---A named parking spot.
---@class Test.Point: GaragePoint
---@field name string
---@field locate fun(): Test.Point

---The result of a lookup.
---@alias Test.Result Test.Point|nil

---@enum Test.Mode
local modes = { active = 'active', closed = 'closed' }
";
    client.open_with("myresource/types.lua", declarations);
    let text = "local label = '🚗' ---@type Test.Point|Test.Result|Test.Mode|Test.Point\n";
    client.open_with(CLIENT, text);
    let cases: &[(&str, &[&str])] = &[
        (
            "Test.Point",
            &[
                "(class) Test.Point : GaragePoint",
                "name: string",
                "locate: fun(): Test.Point",
                "coords: vector3",
                "slots: integer?",
                "A named parking spot.",
            ],
        ),
        ("Test.Result", &["type Test.Result = Test.Point?", "The result of a lookup."]),
        ("Test.Mode", &["type Test.Mode = \"active\"|\"closed\""]),
    ];

    // Columns count UTF-16 units past the emoji; the last `Test.Point` must get its own range.
    let column = |name: &str| text[..text.rfind(name).unwrap()].encode_utf16().count() as u32;

    for &(name, expected) in cases {
        let column = column(name);
        let result = client.request("textDocument/hover", client.position_params(CLIENT, 0, column + 1));
        let hover = result["contents"]["value"].as_str().unwrap_or_default();
        for part in expected {
            assert!(hover.contains(*part), "{name}: missing {part:?} in {result}");
        }
        assert_eq!(
            result["range"],
            json!({
                "start": { "line": 0, "character": column },
                "end": { "line": 0, "character": column + name.len() as u32 }
            })
        );
    }

    client.change("myresource/types.lua", 2, &declarations.replace("name string", "name integer"));
    let hover = client.hover_text(CLIENT, 0, column("Test.Point") + 1);
    assert!(hover.contains("name: integer"), "{hover}");
}

#[test]
fn key_enums_are_the_union_of_their_keys() {
    let mut client = Client::start(fixture_root());
    let declarations = "---@enum (key) Test.Side\nlocal sides = { client = 1, ['server'] = 2 }\n";
    client.open_with("myresource/types.lua", declarations);
    client.open_with(CLIENT, "---@type Test.Side\n");
    let hover = client.hover_text(CLIENT, 0, 12);
    assert!(hover.contains("type Test.Side = \"client\"|\"server\""), "{hover}");
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

    let text = client.open(CLIENT);
    let line = text.lines().count() as u32;
    client.change(CLIENT, 2, &format!("{text}if IsDuplicityVersion() then\n    GetPlayerIdent\nend"));
    let labels = client.completion_labels(CLIENT, line + 1, 18);
    assert!(labels.contains(&"GetPlayerIdentifierByType".to_string()), "server branch of a client file: {labels:?}");
    client.change(CLIENT, 3, &format!("{text}if IsDuplicityVersion() then\n    TriggerClientEvent('a', -1)\nend"));
    let found = client.diagnostics_for(CLIENT);
    assert!(!found.iter().any(|(code, _)| code == "fivem/native-wrong-side"), "{found:?}");
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
fn annotation_definitions_and_hovers_use_closed_files() {
    let mut client = Client::start(fixture_root());
    let annotations = [
        ("---@type GaragePoint[]", "GaragePoint", 0),
        ("---@type table<string, Garage>", "Garage", 36),
        ("---@param garage Garage", "Garage", 36),
        ("---@field kind GarageKind", "GarageKind", 5),
        ("---@class TestGarage: Garage", "Garage", 36),
        ("---@alias GarageList Garage[]", "Garage", 36),
        ("---@return Garage garage, GarageKind kind", "GarageKind", 5),
        ("---@overload fun(point: GaragePoint): Garage", "Garage", 36),
        ("---@operator add(GaragePoint): Garage", "GaragePoint", 0),
        ("---@see Garage", "Garage", 36),
        ("local garage = {} --[[@as Garage]]", "Garage", 36),
    ];
    let text = annotations.iter().map(|(line, ..)| *line).collect::<Vec<_>>().join("\n");
    client.open_with(CLIENT, &text);

    for (line, (annotation, name, definition_line)) in annotations.iter().enumerate() {
        let character = annotation.rfind(*name).unwrap() as u32 + 1;
        let result = client.request("textDocument/definition", client.position_params(CLIENT, line as u32, character));
        assert_eq!(result.as_array().map(Vec::len), Some(1), "{annotation}: {result}");
        assert_eq!(result[0]["uri"], client.uri("[core]/mylib/init.lua").as_str(), "{annotation}");
        assert_eq!(result[0]["range"]["start"]["line"], *definition_line, "{annotation}");
        let hover = client.hover_text(CLIENT, line as u32, character);
        assert!(hover.contains(*name), "{annotation}: {hover}");
    }
}

#[test]
fn goes_to_namespaced_types_and_enums_in_unsaved_files() {
    let mut client = Client::start(fixture_root());
    let declarations = "---@class Test.Point\n\n---@alias Test.Result Test.Point\n\n---@enum Test.Mode\nlocal modes = { active = 'active' }\n";
    client.open_with("myresource/types.lua", declarations);
    let text = "local label = '🚗' ---@type Test.Point|Test.Result|Test.Mode\n";
    client.open_with(CLIENT, text);
    // One character into the name, counted in UTF-16 units past the emoji.
    let column = |name: &str| text[..text.find(name).unwrap() + 1].encode_utf16().count() as u32;

    for (name, line, character) in [("Test.Point", 0, 0), ("Test.Result", 2, 0), ("Test.Mode", 5, 6)] {
        let result = client.request("textDocument/definition", client.position_params(CLIENT, 0, column(name)));
        assert_eq!(result.as_array().map(Vec::len), Some(1), "{name}: {result}");
        assert_eq!(result[0]["uri"], client.uri("myresource/types.lua").as_str());
        assert_eq!(result[0]["range"]["start"], json!({ "line": line, "character": character }));
    }

    client.change("myresource/types.lua", 2, &format!("\n{declarations}"));
    let result = client.request("textDocument/definition", client.position_params(CLIENT, 0, column("Test.Point")));
    assert_eq!(result[0]["range"]["start"]["line"], 1);

    client.open_with("myresource/extra-types.lua", "---@class Test.Point\n---@alias Test.Result string\n");
    for name in ["Test.Point", "Test.Result"] {
        let result = client.request("textDocument/definition", client.position_params(CLIENT, 0, column(name)));
        let locations = result.as_array().unwrap();
        assert_eq!(locations.len(), 2, "{name}: {result}");
        for file in ["myresource/types.lua", "myresource/extra-types.lua"] {
            assert!(locations.iter().any(|location| location["uri"] == client.uri(file).as_str()), "{result}");
        }
    }
}

#[test]
fn annotation_features_ignore_names_outside_type_positions() {
    let mut client = Client::start(fixture_root());
    let lines = [
        "---@param Garage string",
        "---@field Garage string",
        "---@return string Garage",
        "---@type 'Garage'",
        "---@type fun(Garage: string): boolean",
        "---@type { Garage: string }",
        "---@type string # Garage",
        "---Garage is a class.",
        "-- Garage",
        "local text = '---@type Garage'",
        "local text = [[---@type Garage]]",
        "--[[---@type Garage]]",
        "---@type MissingGarage",
    ];
    client.open_with(CLIENT, &lines.join("\n"));

    for (line, text) in lines.iter().enumerate() {
        let column = text.find("Garage").unwrap() as u32 + 1;
        for method in ["textDocument/definition", "textDocument/hover"] {
            let result = client.request(method, client.position_params(CLIENT, line as u32, column));
            assert!(result.is_null(), "{method} on {text}: {result}");
        }
    }
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
fn trigger_calls_show_the_parameters_of_the_handler() {
    let mut client = Client::start(fixture_root());
    let text = client.open(SHOP_CLIENT);
    let (l, c) = pos(&text, "TriggerServerEvent('shop:buy', 'water'", 31);
    let result = client.request("textDocument/signatureHelp", client.position_params(SHOP_CLIENT, l, c));
    let signature = &result["signatures"][0];
    assert_eq!(signature["label"], "TriggerServerEvent(eventName: string, item, amount)");
    assert_eq!(result["activeParameter"], 1);
    let note = signature["documentation"]["value"].as_str().unwrap_or_default();
    assert!(note.contains("shop/server.lua:1"), "{note}");

    let hints = client.request(
        "textDocument/inlayHint",
        json!({ "textDocument": { "uri": client.uri(SHOP_CLIENT) }, "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 40, "character": 0 } } }),
    );
    let labels: Vec<&str> = hints.as_array().unwrap().iter().filter_map(|h| h["label"].as_str()).collect();
    assert!(labels.contains(&"item:") && labels.contains(&"amount:"), "{labels:?}");
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
fn event_completion_replaces_the_whole_name_across_colons() {
    for snippets in [false, true] {
        let mut client = Client::start_with_capabilities(
            fixture_root(),
            json!({ "textDocument": { "completion": { "completionItem": { "snippetSupport": snippets } } } }),
        );
        client.open_with(CLIENT, "");
        let mut version = 1;
        for (call, name) in [
            ("TriggerServerEvent", "myresource:server:ping"),
            ("TriggerLatentServerEvent", "myresource:server:ping"),
            ("TriggerEvent", "shop:bought"),
            ("RegisterNetEvent", "myresource:server:ping"),
            ("lib.callback.await", "myresource:getGarages"),
        ] {
            for quote in ['\'', '"'] {
                for suffix in ["", "stale"] {
                    for length in 0..=name.len() {
                        let prefix = &name[..length];
                        let head = format!("local label = '🚗'; {call}({quote}");
                        let tail = format!("{suffix}{quote}, 42)");
                        let text = format!("{head}{prefix}{tail}");
                        let start = head.encode_utf16().count() as u32;
                        let cursor = start + prefix.len() as u32;
                        version += 1;
                        client.change(CLIENT, version, &text);
                        let mut params = client.position_params(CLIENT, 0, cursor);
                        params["context"] = if prefix.ends_with(':') {
                            json!({ "triggerKind": 2, "triggerCharacter": ":" })
                        } else {
                            json!({ "triggerKind": 1 })
                        };
                        let result = client.request("textDocument/completion", params);
                        let item = result["items"].as_array().unwrap().iter().find(|i| i["label"] == name).unwrap();
                        assert_eq!(
                            item["textEdit"],
                            json!({
                                "range": { "start": { "line": 0, "character": start },
                                    "end": { "line": 0, "character": cursor + suffix.len() as u32 } },
                                "newText": name
                            }),
                            "{text} at {cursor}"
                        );
                        assert_eq!(item["insertTextFormat"], Value::Null);
                    }
                }
                let head = format!("{call}({quote}");
                let prefix = name.rsplit_once(':').unwrap().0.to_string() + ":";
                let text = format!("{head}{prefix}");
                version += 1;
                client.change(CLIENT, version, &text);
                let result =
                    client.request("textDocument/completion", client.position_params(CLIENT, 0, text.len() as u32));
                let item = result["items"].as_array().unwrap().iter().find(|i| i["label"] == name).unwrap();
                assert_eq!(
                    item["textEdit"]["range"],
                    json!({
                        "start": { "line": 0, "character": head.len() },
                        "end": { "line": 0, "character": text.len() }
                    }),
                    "unterminated string: {text}"
                );
            }
        }
    }
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

fn renamed_text(client: &mut Client, relative: &str, text: &str, needle: &str, delta: u32) -> String {
    let (line, column) = pos(text, needle, delta);
    let params = client.position_params(relative, line, column);
    let prepared = client.request("textDocument/prepareRename", params.clone());
    assert!(prepared.is_object(), "rename should be available on {needle}: {prepared}");
    let mut params = params;
    params["newName"] = json!("renamed");
    let result = client.request("textDocument/rename", params);
    let edit: lsp_types::WorkspaceEdit = serde_json::from_value(result.clone()).expect("rename edit");
    let changes = edit.changes.unwrap();
    assert_eq!(changes.len(), 1, "unrelated objects must not be renamed: {result}");
    let mut edits = changes[&client.uri(relative)].clone();
    edits.sort_by_key(|edit| std::cmp::Reverse(edit.range.start));
    let lines = qbx_lua_syntax::LineIndex::new(text);
    let mut output = text.to_string();
    let mut previous_start = text.len() as u32;
    for edit in edits {
        let start = lines.offset_utf16(
            text,
            qbx_lua_syntax::LineCol { line: edit.range.start.line, col: edit.range.start.character },
        );
        let end = lines
            .offset_utf16(text, qbx_lua_syntax::LineCol { line: edit.range.end.line, col: edit.range.end.character });
        assert!(end <= previous_start, "rename edits must not overlap: {result}");
        previous_start = start;
        output.replace_range(start as usize..end as usize, &edit.new_text);
    }
    assert!(qbx_lua_syntax::parse(&output).errors.is_empty(), "renamed Lua must parse: {output}");
    output
}

#[test]
fn rename_static_string_reads_and_writes_preserves_delimiters() {
    let mut client = Client::start(fixture_root());
    let text = "Audit = { foo = 1 }\nAudit['foo'] = 2\nprint(Audit.foo, Audit[\"foo\"], Audit[ [=[foo]=] ], Audit['f\\111o'])\nOther = { foo = 3 }\nprint(Other['foo'])\n";
    client.open_with(SHOP_CLIENT, text);
    let expected = "Audit = { renamed = 1 }\nAudit['renamed'] = 2\nprint(Audit.renamed, Audit[\"renamed\"], Audit[ [=[renamed]=] ], Audit['renamed'])\nOther = { foo = 3 }\nprint(Other['foo'])\n";
    assert_eq!(renamed_text(&mut client, SHOP_CLIENT, text, "Audit.foo", 7), expected);
    assert_eq!(renamed_text(&mut client, SHOP_CLIENT, text, "Audit['foo']", 8), expected);
    let (line, column) = pos(text, "Audit['foo']", 8);
    let mut params = client.position_params(SHOP_CLIENT, line, column);
    params["context"] = json!({ "includeDeclaration": true });
    let refs = client.request("textDocument/references", params);
    assert_eq!(refs.as_array().unwrap().len(), 6, "{refs}");
    let highlights =
        client.request("textDocument/documentHighlight", client.position_params(SHOP_CLIENT, line, column));
    assert_eq!(highlights.as_array().unwrap().len(), 6, "{highlights}");
}

#[test]
fn rename_static_string_declarations_and_nested_paths() {
    let mut client = Client::start(fixture_root());
    let cases = [
        ("Audit = { ['foo'] = 1 }\nprint(Audit.foo, Audit['foo'])\n",
         "['foo']", 3,
         "Audit = { ['renamed'] = 1 }\nprint(Audit.renamed, Audit['renamed'])\n"),
        ("Audit = {}\nAudit['foo'] = 1\nprint(Audit.foo, Audit['foo'])\n",
         "Audit.foo", 7,
         "Audit = {}\nAudit['renamed'] = 1\nprint(Audit.renamed, Audit['renamed'])\n"),
        ("Audit = { ['nested'] = { [ [=[\nfoo]=] ] = 1 } }\nprint(Audit['nested'].foo, Audit.nested['foo'])\n",
         ".foo", 2,
         "Audit = { ['nested'] = { [ [=[\nrenamed]=] ] = 1 } }\nprint(Audit['nested'].renamed, Audit.nested['renamed'])\n"),
    ];
    for (version, (text, needle, delta, expected)) in (1..).zip(cases) {
        if version == 1 {
            client.open_with(SHOP_CLIENT, text);
        } else {
            client.change(SHOP_CLIENT, version, text);
        }
        assert_eq!(renamed_text(&mut client, SHOP_CLIENT, text, needle, delta), expected);
    }
}

#[test]
fn rename_annotation_fields_updates_declarations_and_typed_constructors() {
    let mut client = Client::start(fixture_root());
    let text = "---@class RenameOptions\n---@field private foo? number foo description\nlocal audit = { ['foo'] = 1 }\n---@type RenameOptions\nlocal other = { foo = 2 }\nprint(audit.foo, other['foo'])\n";
    client.open_with(SHOP_CLIENT, text);
    let expected = "---@class RenameOptions\n---@field private renamed? number foo description\nlocal audit = { ['renamed'] = 1 }\n---@type RenameOptions\nlocal other = { renamed = 2 }\nprint(audit.renamed, other['renamed'])\n";
    assert_eq!(renamed_text(&mut client, SHOP_CLIENT, text, "audit.foo", 7), expected);
    assert_eq!(renamed_text(&mut client, SHOP_CLIENT, text, "private foo", 9), expected);

    let text = "---@class RenameOptions\n---@field ['foo'] number foo description\nlocal audit = { foo = 1 }\nprint(audit['foo'])\n";
    client.change(SHOP_CLIENT, 2, text);
    let expected = "---@class RenameOptions\n---@field ['renamed'] number foo description\nlocal audit = { renamed = 1 }\nprint(audit['renamed'])\n";
    assert_eq!(renamed_text(&mut client, SHOP_CLIENT, text, "audit['foo']", 8), expected);
}

#[test]
fn rename_finds_escaped_keys_in_closed_files_and_aborts_if_a_file_is_unreadable() {
    struct Fixture(PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            if let (Ok(root), Ok(temp)) = (self.0.canonicalize(), std::env::temp_dir().canonicalize()) {
                if root.parent() == Some(temp.as_path()) {
                    let _ = std::fs::remove_dir_all(root);
                }
            }
        }
    }
    let fixture = Fixture(std::env::temp_dir().join(format!(
        "qbx-rename-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
    )));
    std::fs::create_dir(&fixture.0).unwrap();
    std::fs::write(
        fixture.0.join("fxmanifest.lua"),
        "fx_version 'cerulean'\ngame 'gta5'\nshared_scripts { 'main.lua', 'closed.lua' }\n",
    )
    .unwrap();
    std::fs::write(fixture.0.join("main.lua"), "Audit = { foo = 1 }\nprint(Audit.foo)\n").unwrap();
    std::fs::write(fixture.0.join("closed.lua"), "print(Audit['\\102\\111\\111'])\n").unwrap();
    let mut client = Client::start(fixture.0.clone());
    let text = client.open("main.lua");
    let (line, column) = pos(&text, "Audit.foo", 7);
    let mut params = client.position_params("main.lua", line, column);
    params["newName"] = json!("renamed");
    let result = client.request("textDocument/rename", params.clone());
    let changes = result["changes"].as_object().expect("rename edit");
    assert_eq!(changes.len(), 2, "escaped references in closed files must be found: {result}");
    let edits = changes[client.uri("closed.lua").as_str()].as_array().unwrap();
    assert_eq!(edits.len(), 1, "{result}");
    assert_eq!(edits[0]["range"], json!({"start": {"line": 0, "character": 13}, "end": {"line": 0, "character": 25}}));
    std::fs::remove_file(fixture.0.join("closed.lua")).unwrap();
    assert_eq!(
        client.request("textDocument/rename", params),
        Value::Null,
        "an unreadable indexed file must not result in partial edits"
    );
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
fn lua_ls_config_supplies_lint_settings_but_not_formatting() {
    struct Fixture(PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            if let (Ok(root), Ok(temp)) = (self.0.canonicalize(), std::env::temp_dir().canonicalize()) {
                if root.parent() == Some(temp.as_path()) {
                    let _ = std::fs::remove_dir_all(root);
                }
            }
        }
    }
    let fixture = Fixture(std::env::temp_dir().join(format!(
        "qbx-luarc-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
    )));
    std::fs::create_dir(&fixture.0).unwrap();
    std::fs::write(fixture.0.join("fxmanifest.lua"), "fx_version 'cerulean'\ngame 'gta5'\nclient_script 'main.lua'\n")
        .unwrap();
    std::fs::write(
        fixture.0.join(".luarc.json"),
        r#"{ "diagnostics.globals": ["Config"], "diagnostics.disable": ["lowercase-global"] }"#,
    )
    .unwrap();
    let text = "helper = function() return Config end\nif helper   then\nprint( helper )\nend\n";
    std::fs::write(fixture.0.join("main.lua"), text).unwrap();
    let mut client = Client::start(fixture.0.clone());
    client.open_with("main.lua", text);
    assert_eq!(client.diagnostics_for("main.lua"), []);
    let fallback =
        |logs: &[String]| logs.iter().filter(|l| l.contains("falling back") && l.contains(".luarc.json")).count();
    assert_eq!(fallback(&client.logs), 1, "{:?}", client.logs);

    let params =
        json!({ "textDocument": { "uri": client.uri("main.lua") }, "options": { "tabSize": 2, "insertSpaces": true } });
    let edits = client.request("textDocument/formatting", params);
    assert_eq!(
        edits[0]["newText"], "helper = function() return Config end\nif helper then\n  print(helper)\nend\n",
        "the editor's indentation applies without a qbxlint.toml"
    );

    let watchers: Vec<&str> = client
        .registrations
        .iter()
        .flat_map(|r| r["registerOptions"]["watchers"].as_array().into_iter().flatten())
        .filter_map(|w| w["globPattern"].as_str())
        .collect();
    assert!(watchers.contains(&"**/.luarc.json") && watchers.contains(&"**/.emmyrc.json"), "{watchers:?}");
    std::fs::write(fixture.0.join(".luarc.json"), r#"{ "diagnostics.globals": ["Config"] }"#).unwrap();
    client.notify(
        "workspace/didChangeWatchedFiles",
        json!({ "changes": [{ "uri": client.uri(".luarc.json"), "type": 2 }] }),
    );
    assert_eq!(
        client.diagnostics_for("main.lua"),
        [("lowercase-global".to_string(), 0)],
        "a changed .luarc.json applies"
    );
    assert_eq!(fallback(&client.logs), 2, "a reloaded fallback is logged again: {:?}", client.logs);

    std::fs::write(fixture.0.join(".luarc.json"), r#"{ "diagnostics.globals": ["#).unwrap();
    client.notify(
        "workspace/didChangeWatchedFiles",
        json!({ "changes": [{ "uri": client.uri(".luarc.json"), "type": 2 }] }),
    );
    let diagnostics = client.diagnostics_for("main.lua");
    assert!(diagnostics.iter().any(|(code, _)| code == "undefined-global"), "{diagnostics:?}");
    assert!(client.logs.iter().any(|l| l.starts_with("skipped") && l.contains(".luarc.json")), "{:?}", client.logs);
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
fn snippets_outrank_the_plain_name() {
    let mut client = Client::start(fixture_root());
    let text = client.open(CLIENT);
    let line = text.lines().count() as u32;
    let mut version = 1;
    let mut first_item = |typed: &str| {
        version += 1;
        client.change(CLIENT, version, &format!("{text}{typed}"));
        let params = client.position_params(CLIENT, line, typed.len() as u32);
        let result = client.request("textDocument/completion", params);
        let mut items = result["items"].as_array().cloned().unwrap_or_default();
        items.sort_by_key(|i| i["sortText"].as_str().unwrap_or_default().to_string());
        items.into_iter().next().unwrap_or(Value::Null)
    };

    let thread = first_item("CreateThread");
    assert_eq!(thread["labelDetails"]["description"], "snippet", "{thread}");
    let preview = thread["documentation"]["value"].as_str().unwrap();
    assert!(preview.contains("Wait(0)") && !preview.contains('$'), "{preview}");
    let body = thread["insertText"].as_str().unwrap();
    assert!(body.contains("while true do") && body.contains("Wait(${1:0})"), "{body}");

    let on_cache = first_item("oncache");
    let body = on_cache["insertText"].as_str().unwrap_or_default();
    assert!(body.starts_with("lib.onCache('${1|ped,"), "falls back to the usual keys without ox_lib: {on_cache}");

    let member = first_item("lib.onCa");
    assert!(member["insertText"].as_str().unwrap_or_default().starts_with("onCache('${1|"), "{member}");
}

#[test]
fn minimal_clients_receive_plain_completions_and_no_dynamic_watch_registration() {
    for capabilities in [
        json!({}),
        json!({
            "workspace": { "didChangeWatchedFiles": { "dynamicRegistration": false } },
            "textDocument": { "completion": { "completionItem": { "snippetSupport": false } } }
        }),
    ] {
        let mut client = Client::start_with_capabilities(fixture_root(), capabilities);
        client.request("qbx/status", Value::Null);
        assert!(client.registrations.is_empty(), "{:?}", client.registrations);

        let cases = [
            (CLIENT, "CreateThread", 0, 12, Some("CreateThread")),
            (CLIENT, "local Useful = 1\nUse", 1, 3, Some("Useful")),
            (CLIENT, "lib.onCa", 0, 8, None),
            (CLIENT, "oncache", 0, 7, None),
            (CLIENT, "---@par", 0, 7, Some("param")),
            ("myresource/fxmanifest.lua", "fx_v", 0, 4, Some("fx_version")),
        ];
        for (relative, text, line, column, expected) in cases {
            client.open_with(relative, text);
            let result = client.request("textDocument/completion", client.position_params(relative, line, column));
            let items = result["items"].as_array().unwrap();
            if let Some(label) = expected {
                assert!(items.iter().any(|item| item["label"] == label), "{result}");
            }
            for item in items {
                assert_ne!(item["insertTextFormat"], 2, "{item}");
                assert_ne!(item["labelDetails"]["description"], "snippet", "{item}");
                assert!(!item["insertText"].as_str().unwrap_or_default().contains('$'), "{item}");
            }
            if text == "---@par" {
                assert_eq!(items.iter().find(|item| item["label"] == "param").unwrap()["insertText"], "param");
            }
            client.notify("textDocument/didClose", json!({"textDocument": {"uri": client.uri(relative)}}));
        }
    }
}

#[test]
fn capable_clients_keep_file_watches_and_annotation_and_manifest_snippets() {
    let mut client = Client::start(fixture_root());
    client.request("qbx/status", Value::Null);
    assert_eq!(client.registrations.len(), 1);
    let registration = &client.registrations[0];
    assert_eq!(registration["method"], "workspace/didChangeWatchedFiles");
    assert!(registration["registerOptions"]["watchers"]
        .as_array()
        .unwrap()
        .iter()
        .any(|watch| watch["globPattern"] == "**/*.lua"));

    for (relative, text, label) in [(CLIENT, "---@par", "param"), ("myresource/fxmanifest.lua", "fx_v", "fx_version")] {
        client.open_with(relative, text);
        let result = client.request("textDocument/completion", client.position_params(relative, 0, text.len() as u32));
        let item = result["items"].as_array().unwrap().iter().find(|item| item["label"] == label).unwrap();
        assert_eq!(item["insertTextFormat"], 2, "{item}");
        assert!(item["insertText"].as_str().unwrap().contains("${1"), "{item}");
    }
}

#[test]
fn knows_glm_and_keeps_native_handle_names() {
    let mut client = Client::start(fixture_root());
    let text = client.open(CLIENT);
    let line = text.lines().count() as u32;
    let added = "local veh = GetVehiclePedIsIn(PlayerPedId(), false)\nlocal dir = glm.normalize(vector3(1, 2, 3))\nprint(veh, dir, glm.pi)\nglm.quatLook\nlocal g = require 'glm'\nlocal zone = g.polygon.new({ vector3(0, 0, 0) })\nprint(zone:contains(vector3(0, 0, 0), 2), g.tointeger(1.0))\nveh.";
    client.change(CLIENT, 2, &format!("{text}{added}"));

    let hover = client.hover_text(CLIENT, line, 7);
    assert!(hover.contains("local veh: Vehicle"), "{hover}");
    let native = client.hover_text(CLIENT, line, 16);
    assert!(native.contains("ped: Ped") && native.contains("): Vehicle"), "{native}");
    assert_eq!(client.completion_labels(CLIENT, line + 7, 4), Vec::<String>::new(), "a handle has no members");

    let normalize = client.hover_text(CLIENT, line + 1, 20);
    assert!(normalize.contains("glm.normalize") && normalize.contains("length 1"), "{normalize}");
    assert!(client.hover_text(CLIENT, line + 2, 22).contains("number"));
    assert!(client.completion_labels(CLIENT, line + 3, 12).contains(&"quatLookAt".to_string()));

    let zone = client.hover_text(CLIENT, line + 5, 7);
    assert!(zone.contains("local zone: glm.polygon"), "require 'glm' is the built-in library: {zone}");
    let contains = client.hover_text(CLIENT, line + 6, 13);
    assert!(contains.contains("thickness?: number") && contains.contains("boolean"), "{contains}");

    let found = client.diagnostics_for(CLIENT);
    assert!(!found.iter().any(|(code, l)| code == "undefined-global" && *l >= u64::from(line)), "{found:?}");
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
