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

    fn request_error(&mut self, method: &str, params: Value) -> lsp_server::ResponseError {
        self.next_id += 1;
        let id = RequestId::from(self.next_id);
        self.connection
            .sender
            .send(Message::Request(Request { id: id.clone(), method: method.into(), params }))
            .unwrap();
        loop {
            let message =
                self.connection.receiver.recv_timeout(Duration::from_secs(20)).expect("server did not answer");
            if let Message::Response(response) = &message {
                if response.id == id {
                    return response.error.clone().expect("expected request validation error");
                }
            }
            self.handle_incoming(message);
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
fn reference_search_is_offline_paginated_and_available_without_open_documents() {
    let mut client = Client::start_with_capabilities(fixture_root(), json!({}));
    let before = client.request("qbx/status", Value::Null);
    let first = client.request("qbx/referenceSearch", Value::Null);
    assert_eq!(first["items"].as_array().unwrap().len(), 50);
    assert_eq!(first["offset"], 0);
    assert_eq!(first["limit"], 50);
    assert!(first["total"].as_u64().unwrap() > 7000);
    let namespaces = first["namespaces"].as_array().unwrap();
    assert!(namespaces.iter().any(|value| value == "PAD"));
    assert!(namespaces.windows(2).all(|pair| pair[0].as_str() < pair[1].as_str()));
    assert!(first["items"].as_array().unwrap().iter().all(|item| item.get("documentation").is_none()));
    assert_eq!(first, client.request("qbx/referenceSearch", json!({})), "default search order is deterministic");
    let second = client.request("qbx/referenceSearch", json!({ "offset": 50 }));
    let first_ids: Vec<_> = first["items"].as_array().unwrap().iter().map(|item| &item["id"]).collect();
    assert!(second["items"].as_array().unwrap().iter().all(|item| !first_ids.contains(&&item["id"])));
    let capped = client.request("qbx/referenceSearch", json!({ "limit": 1000000 }));
    assert_eq!(capped["items"].as_array().unwrap().len(), 100);
    assert_eq!(capped["limit"], 100);
    let minimum = client.request("qbx/referenceSearch", json!({ "limit": 0 }));
    assert_eq!(minimum["limit"], 1);
    assert_eq!(minimum["items"].as_array().unwrap().len(), 1);
    let past_end = client.request("qbx/referenceSearch", json!({ "offset": u64::MAX }));
    assert!(past_end["items"].as_array().unwrap().is_empty());
    assert_eq!(past_end["offset"], past_end["total"]);
    let after = client.request("qbx/status", Value::Null);
    assert_eq!(before, after, "reference requests do not open files or change the workspace index");
}

#[test]
fn reference_search_matches_names_hashes_aliases_ids_and_default_bindings() {
    let mut client = Client::start(fixture_root());
    for (query, kind, expected) in [
        ("GetEntityCoords", "native", "native:GetEntityCoords"),
        ("gEtEnTiTyCoOrDs", "native", "native:GetEntityCoords"),
        ("SET_PED_CONFIG_FLAG", "native", "native:SetPedConfigFlag"),
        ("0x3FEF770D40960D5A", "native", "native:GetEntityCoords"),
        ("3fef770d40960d5a", "native", "native:GetEntityCoords"),
        ("N_0x580417101DDB492F", "native", "native:IsControlJustPressed"),
        ("N_0xe8a25867fba3b05e", "native", "native:SetControlNormal"),
        ("GetLastInputMethod", "native", "native:IsUsingKeyboard"),
        ("38", "control", "control:38"),
        ("input_pickup", "control", "control:38"),
        ("input pickup", "control", "control:38"),
        ("pickup e", "control", "control:38"),
        ("51 DPAD RIGHT", "control", "control:51"),
        ("48", "pedFlag", "pedFlag:48"),
        ("BlockWeaponSwitching", "pedFlag", "pedFlag:48"),
    ] {
        let result = client.request("qbx/referenceSearch", json!({ "query": query, "kind": kind }));
        assert_eq!(result["items"][0]["id"], expected, "{query}: {result}");
    }
    let result = client.request("qbx/referenceSearch", json!({ "query": "38" }));
    assert_eq!(result["items"][0]["id"], "control:38", "exact IDs outrank substrings in hashes");
    assert_eq!(result["items"][1]["id"], "pedFlag:38");
    let result = client.request("qbx/referenceSearch", json!({ "query": "IsControl", "kind": "native" }));
    assert!(result["items"][0]["name"].as_str().unwrap().starts_with("IsControl"));
    let result = client.request("qbx/referenceSearch", json!({ "query": "not-a-real-native-or-control" }));
    assert_eq!(result["total"], 0);
    assert!(result["items"].as_array().unwrap().is_empty());
    let result = client.request("qbx/referenceSearch", json!({ "kind": "native" }));
    let canonical_count = qbx_fivem_data::natives().filter(|native| native.alias_of.is_none()).count();
    assert_eq!(result["total"], canonical_count, "default listings deduplicate documented aliases");
}

#[test]
fn reference_search_filters_side_namespace_and_catalog_without_conflating_them() {
    let mut client = Client::start(fixture_root());
    for side in ["client", "server", "shared"] {
        let result = client.request("qbx/referenceSearch", json!({ "side": side, "limit": 100 }));
        assert!(result["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["side"] == side || side != "shared" && item["side"] == "shared"));
        let shared = client
            .request("qbx/referenceSearch", json!({ "query": "GetEntityCoords", "side": side, "kind": "native" }));
        assert_eq!(shared["items"][0]["id"], "native:GetEntityCoords", "shared native remains available on {side}");
    }
    let result =
        client.request("qbx/referenceSearch", json!({ "kind": "native", "namespace": "pad", "side": "server" }));
    assert_eq!(result["total"], 0, "PAD natives are client-only");
    let result = client.request("qbx/referenceSearch", json!({ "kind": "native", "namespace": "PAD", "limit": 100 }));
    assert!(result["total"].as_u64().unwrap() > 20);
    assert!(result["items"].as_array().unwrap().iter().all(|item| item["namespace"] == "PAD"));
    let all = client.request("qbx/referenceSearch", json!({ "namespace": "PAD" }));
    assert_eq!(
        all["total"].as_u64().unwrap(),
        result["total"].as_u64().unwrap()
            + qbx_fivem_data::controls().count() as u64
            + qbx_fivem_data::ped_config_flags().count() as u64,
        "all-catalog search retains numeric catalogs when filtering native namespace"
    );
    for (kind, count) in
        [("control", qbx_fivem_data::controls().count()), ("pedFlag", qbx_fivem_data::ped_config_flags().count())]
    {
        let result = client.request("qbx/referenceSearch", json!({ "kind": kind, "namespace": "PED" }));
        assert_eq!(result["total"], count, "native namespace filters do not hide other catalogs");
        assert!(result["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["kind"] == kind && item["side"] == "client" && item.get("namespace").is_none()));
        let result = client.request("qbx/referenceSearch", json!({ "kind": kind, "side": "server" }));
        assert_eq!(result["total"], 0);
    }
}

#[test]
fn reference_details_return_documentation_and_safe_lua_insertion_text() {
    let mut client = Client::start(fixture_root());
    let detail = client.request("qbx/referenceDetail", json!({ "id": "native:GetEntityCoords" }));
    assert_eq!(detail["id"], "native:GetEntityCoords");
    assert_eq!(detail["kind"], "native");
    assert_eq!(detail["side"], "shared");
    assert_eq!(detail["namespace"], "ENTITY");
    assert_eq!(detail["hash"], "0x3FEF770D40960D5A");
    assert_eq!(detail["signature"], "function GetEntityCoords(entity: Entity, alive: boolean): vector3");
    assert_eq!(
        detail["parameters"],
        json!([{ "name": "entity", "type": "Entity" }, { "name": "alive", "type": "boolean" }])
    );
    assert_eq!(detail["returns"], json!(["vector3"]));
    assert!(!detail["documentation"].as_str().unwrap().is_empty());
    assert_eq!(detail["sourceUrl"], "https://docs.fivem.net/natives/?_0x3FEF770D40960D5A");
    assert_eq!(detail["copyText"], "GetEntityCoords");
    assert_eq!(detail["insertText"], "GetEntityCoords(entity, alive)");
    assert_eq!(detail["insertSnippet"], "GetEntityCoords(${1:entity}, ${2:alive})$0");
    let alias = client.request("qbx/referenceDetail", json!({ "id": "native:GetLastInputMethod" }));
    assert_eq!(alias["id"], "native:IsUsingKeyboard");
    let hash = client.request("qbx/referenceDetail", json!({ "id": "native:N_0x580417101DDB492F" }));
    assert_eq!(hash["id"], "native:IsControlJustPressed");
    let no_args = client.request("qbx/referenceDetail", json!({ "id": "native:PlayerPedId" }));
    assert_eq!(no_args["insertText"], "PlayerPedId()");
    assert_eq!(no_args["insertSnippet"], "PlayerPedId()$0");

    for (id, expected) in [
        ("control:38", "INPUT_PICKUP"),
        ("control:243", "`` ~ / ` ``"),
        ("control:360", "Not documented"),
        ("pedFlag:48", "CPED_CONFIG_FLAG_BlockWeaponSwitching"),
    ] {
        let detail = client.request("qbx/referenceDetail", json!({ "id": id }));
        assert_eq!(detail["id"], id);
        assert!(detail["documentation"].as_str().unwrap().contains(expected), "{detail}");
        let number = id.split_once(':').unwrap().1;
        assert_eq!(detail["copyText"], number);
        assert_eq!(detail["insertText"], number);
        assert!(detail.get("insertSnippet").is_none());
        assert!(detail.get("signature").is_none());
        if id.starts_with("pedFlag:") {
            assert!(detail["documentation"].as_str().unwrap().contains("Behavior is not documented"));
            assert!(detail["documentation"].as_str().unwrap().contains("potential names and hash collisions"));
        }
    }
    for id in [
        "unknown:1",
        "native:MissingNative",
        "native:PED",
        "control:9999",
        "control:-1",
        "control:038",
        "pedFlag:4294967296",
        "",
        "control:38:other",
    ] {
        assert_eq!(client.request("qbx/referenceDetail", json!({ "id": id })), Value::Null, "{id}");
    }
}

#[test]
fn reference_requests_reject_invalid_params_without_disrupting_lsp() {
    let mut client = Client::start(fixture_root());
    for params in [
        json!({ "query": "a".repeat(257) }),
        json!({ "namespace": "a".repeat(65) }),
        json!({ "offset": -1 }),
        json!({ "offset": 0.5 }),
        json!({ "limit": -1 }),
        json!({ "kind": "invalid" }),
        json!({ "side": "invalid" }),
        json!({ "query": 38 }),
        json!([]),
    ] {
        let error = client.request_error("qbx/referenceSearch", params);
        assert_eq!(error.code, -32602);
    }
    for params in
        [json!({ "id": "a".repeat(513) }), json!({ "id": 38 }), Value::Null, json!(["native:GetEntityCoords"])]
    {
        let error = client.request_error("qbx/referenceDetail", params);
        assert_eq!(error.code, -32602);
    }
    let text = "IsControlJustPressed(0, 38)";
    client.open_with(CLIENT, text);
    let (line, character) = pos(text, "38", 0);
    assert!(client.hover_text(CLIENT, line, character).contains("INPUT_PICKUP"));
    assert!(client.request("qbx/status", Value::Null)["natives"].as_u64().unwrap() > 7000);
}

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
fn native_argument_hovers_show_defaults_flags_and_exact_ranges() {
    // Numeric hovers also work for clients that advertise no optional hover capabilities.
    for capabilities in [json!({}), json!({ "textDocument": { "hover": { "contentFormat": ["markdown"] } } })] {
        let mut client = Client::start_with_capabilities(fixture_root(), capabilities);
        let text = "IsControlJustPressed(0, 38)\nSetPedConfigFlag(PlayerPedId(), 48, true)\nGetControlNormal(0, 360)\nGetControlNormal(0, 3)\nGetControlNormal(0, 243)\n";
        client.open_with(CLIENT, text);
        for (literal, symbol) in [("38", "INPUT_PICKUP"), ("48", "CPED_CONFIG_FLAG_BlockWeaponSwitching")] {
            let (line, character) = pos(text, literal, 0);
            let hover = client.request("textDocument/hover", client.position_params(CLIENT, line, character));
            let content = hover["contents"]["value"].as_str().unwrap();
            assert!(content.contains(symbol), "{content}");
            assert!(content.contains("https://"), "reference should link its source: {content}");
            assert_eq!(hover["contents"]["kind"], "markdown");
            assert_eq!(
                hover["range"],
                json!({
                    "start": { "line": line, "character": character },
                    "end": { "line": line, "character": character + 2 }
                })
            );
            if literal == "38" {
                assert!(content.contains("Default keyboard (QWERTY): `E`"), "{content}");
                assert!(content.contains("Default Xbox controller: `LB`"), "{content}");
                assert!(content.contains("remapped"), "{content}");
            } else {
                assert!(content.contains("Behavior is not documented"), "do not infer behavior from a name: {content}");
                assert!(content.contains("potential names and hash collisions"), "{content}");
            }
            // Hovering the comma/space after a literal must not inherit its enum documentation.
            assert!(client.hover_text(CLIENT, line, character + 2).is_empty());
        }
        let hover = client.hover_text(CLIENT, 0, 3);
        assert!(hover.contains("function IsControlJustPressed"), "ordinary native hover is preserved: {hover}");
        let (line, character) = pos(text, "360", 0);
        let hover = client.hover_text(CLIENT, line, character);
        assert!(hover.contains("INPUT_HUDMARKER_SELECT"), "{hover}");
        assert!(hover.contains("Default keyboard (QWERTY): Not documented"), "{hover}");
        assert!(hover.contains("Default Xbox controller: Not documented"), "{hover}");
        let (line, character) = pos(text, ", 3)", 2);
        let hover = client.hover_text(CLIENT, line, character);
        assert!(hover.contains("Default keyboard (QWERTY): `(NONE)`"), "explicit unbound values stay intact: {hover}");
        let (line, character) = pos(text, "243", 0);
        let hover = client.hover_text(CLIENT, line, character);
        assert!(
            hover.contains("Default keyboard (QWERTY): `` ~ / ` ``"),
            "backtick keys must remain valid Markdown: {hover}"
        );
    }
}

#[test]
fn native_argument_hovers_cover_control_variants_hashes_and_numeric_literals() {
    let mut client = Client::start(fixture_root());
    let cases = [
        ("IsControlEnabled(0, 38)", "38", "INPUT_PICKUP"),
        ("IsControlJustReleased(0, 38)", "38", "INPUT_PICKUP"),
        ("IsControlPressed(0, 38)", "38", "INPUT_PICKUP"),
        ("IsControlReleased(0, 38)", "38", "INPUT_PICKUP"),
        ("IsDisabledControlJustPressed(0, 38)", "38", "INPUT_PICKUP"),
        ("IsDisabledControlJustReleased(0, 51)", "51", "INPUT_CONTEXT"),
        ("IsDisabledControlPressed(0, 38)", "38", "INPUT_PICKUP"),
        ("IsDisabledControlReleased(0, 38)", "38", "INPUT_PICKUP"),
        ("GetControlValue(0, 38)", "38", "INPUT_PICKUP"),
        ("GetControlNormal(2, 0)", "0", "INPUT_NEXT_CAMERA"),
        ("GetControlUnboundNormal(0, 38)", "38", "INPUT_PICKUP"),
        ("GetDisabledControlNormal(0, 38)", "38", "INPUT_PICKUP"),
        ("GetDisabledControlUnboundNormal(0, 38)", "38", "INPUT_PICKUP"),
        ("GetControlInstructionalButton(0, 38, true)", "38", "INPUT_PICKUP"),
        ("DisableControlAction(0, 38, true)", "38", "INPUT_PICKUP"),
        ("EnableControlAction(0, 38, true)", "38", "INPUT_PICKUP"),
        ("SetControlNormal(0, 38, 0.5)", "38", "INPUT_PICKUP"),
        ("SetInputExclusive(0, 38)", "38", "INPUT_PICKUP"),
        ("IsControlJustPressed(0, 0X26)", "0X26", "INPUT_PICKUP"),
        ("IsControlJustPressed(0, 38.0)", "38.0", "INPUT_PICKUP"),
        ("IsControlJustPressed(0, 3.8e1)", "3.8e1", "INPUT_PICKUP"),
        ("IsControlJustPressed(0, 0x26p0)", "0x26p0", "INPUT_PICKUP"),
        ("N_0xe8a25867fba3b05e(0, 38, 0.5)", "38", "INPUT_PICKUP"),
        ("N_0x580417101DDB492F(0, 0x26)", "0x26", "INPUT_PICKUP"),
        ("GetPedConfigFlag(PlayerPedId(), 32, true)", "32", "CPED_CONFIG_FLAG_WillFlyThroughWindscreen"),
        ("N_0x1913FE4CBF41C463(PlayerPedId(), 0x30, true)", "0x30", "CPED_CONFIG_FLAG_BlockWeaponSwitching"),
        ("N_0x7ee53118c892b513(PlayerPedId(), 48, true)", "48", "CPED_CONFIG_FLAG_BlockWeaponSwitching"),
    ];
    client.open_with(CLIENT, cases[0].0);
    for (version, (text, literal, symbol)) in cases.into_iter().enumerate() {
        client.change(CLIENT, version as i32 + 2, text);
        let (line, character) = pos(text, literal, 0);
        let hover = client.request("textDocument/hover", client.position_params(CLIENT, line, character));
        assert!(hover["contents"]["value"].as_str().unwrap_or_default().contains(symbol), "{text}: {hover}");
        assert_eq!(hover["range"]["start"]["character"], character, "{text}: {hover}");
        assert_eq!(hover["range"]["end"]["character"], character + literal.len() as u32, "{text}: {hover}");
    }
}

#[test]
fn native_argument_hovers_respect_ast_nesting_comments_and_argument_positions() {
    let mut client = Client::start(fixture_root());
    let text = "\
local label = 'é🎮'; Consume(IsControlJustPressed(0, 38))
SetPedConfigFlag(
    GetPed(48), -- a nested number is not a flag
    ( -- flag 48 in a comment is not a literal
      0x30 -- end of actual flag
    ),
    true
)
IsControlJustPressed(38, 51)
GetControlGroupInstructionalButton(0, 38, true)
SetControlGroupColor(0, 38, 0, 0)
SetPedResetFlag(PlayerPedId(), 48, true)
SetControlNormal(0, 38, 51)
GetPedConfigFlag(32, 48, 32)
";
    client.open_with(CLIENT, text);
    // LSP ranges use UTF-16, including when non-ASCII text precedes the number.
    let first_line = text.lines().next().unwrap();
    let offset = first_line.find("38").unwrap();
    let character = first_line[..offset].encode_utf16().count() as u32;
    let hover = client.request("textDocument/hover", client.position_params(CLIENT, 0, character));
    assert!(hover["contents"]["value"].as_str().unwrap().contains("INPUT_PICKUP"), "{hover}");
    assert_eq!(
        hover["range"],
        json!({
            "start": { "line": 0, "character": character },
            "end": { "line": 0, "character": character + 2 }
        })
    );
    let (line, character) = pos(text, "0x30", 0);
    let hover = client.request("textDocument/hover", client.position_params(CLIENT, line, character));
    assert!(hover["contents"]["value"].as_str().unwrap().contains("CPED_CONFIG_FLAG_BlockWeaponSwitching"), "{hover}");
    assert_eq!(hover["range"]["start"]["character"], character);
    assert_eq!(hover["range"]["end"]["character"], character + 4);
    for (needle, literal) in [
        ("GetPed(48)", "48"),
        ("flag 48", "48"),
        ("IsControlJustPressed(38", "38"),
        ("GetControlGroupInstructionalButton(0, 38", "38"),
        ("SetControlGroupColor(0, 38", "38"),
        ("SetPedResetFlag(PlayerPedId(), 48", "48"),
        ("SetControlNormal(0, 38, 51)", "51"),
        ("GetPedConfigFlag(32", "32"),
        ("48, 32)", "32"),
    ] {
        let (line, character) = pos(text, needle, needle.rfind(literal).unwrap() as u32);
        assert!(client.hover_text(CLIENT, line, character).is_empty(), "unrelated argument/comment: {needle}");
    }
    let (line, character) = pos(text, "38, 51)", 4);
    assert!(client.hover_text(CLIENT, line, character).contains("INPUT_CONTEXT"));
}

#[test]
fn native_argument_hovers_ignore_expressions_unknown_ids_and_unrelated_functions() {
    let mut client = Client::start(fixture_root());
    let cases = [
        ("local value = 38", "38"),
        ("CustomControl(0, 38)", "38"),
        ("controls.IsControlJustPressed(0, 38)", "38"),
        ("controls:IsControlJustPressed(0, 38)", "38"),
        ("_G.IsControlJustPressed(0, 38)", "38"),
        ("Citizen.InvokeNative(0x580417101DDB492F, 0, 38)", "38"),
        ("IsControlJustPressed(0, 38 + 1)", "38"),
        ("IsControlJustPressed(0, 38 | 1)", "38"),
        ("IsControlJustPressed(0, -38)", "38"),
        ("IsControlJustPressed(0, tonumber(38))", "38"),
        ("IsControlJustPressed(0, {38})", "38"),
        ("IsControlJustPressed(0, '38')", "38"),
        ("IsControlJustPressed(0, 38.5)", "38.5"),
        ("IsControlJustPressed(0, 9999)", "9999"),
        ("IsControlJustPressed(0, 38oops)", "38oops"),
        ("IsControlJustPressed(0, 0xZZ)", "0xZZ"),
        ("GetPedResetFlag(PlayerPedId(), 48)", "48"),
        ("SetPedConfigFlag(PlayerPedId(), 9999, true)", "9999"),
        ("SetPedConfigFlag(PlayerPedId(), 48 + 1, true)", "48"),
        ("N_0x0000000000000000(0, 38)", "38"),
    ];
    client.open_with(CLIENT, cases[0].0);
    for (version, (text, literal)) in cases.into_iter().enumerate() {
        client.change(CLIENT, version as i32 + 2, text);
        let (line, character) = pos(text, literal, 0);
        assert!(client.hover_text(CLIENT, line, character).is_empty(), "must not label {text}");
    }
    let text = "local control = 38\nIsControlJustPressed(0, control)";
    client.change(CLIENT, 100, text);
    let (line, character) = pos(text, ", control", 2);
    let hover = client.hover_text(CLIENT, line, character);
    assert!(hover.contains("local control: integer"), "ordinary variable hover is preserved: {hover}");
    assert!(!hover.contains("INPUT_PICKUP"), "do not evaluate dynamic arguments: {hover}");
}

#[test]
fn native_argument_hovers_ignore_shadowed_and_redefined_natives() {
    let mut client = Client::start(fixture_root());
    let cases = [
        "local IsControlJustPressed = function(...) end\nIsControlJustPressed(0, 38)",
        "local function IsControlJustPressed(...) end\nIsControlJustPressed(0, 38)",
        "local IsControlJustPressed = IsControlJustPressed\nIsControlJustPressed(0, 38)",
        "function check(IsControlJustPressed)\nIsControlJustPressed(0, 38)\nend",
        "IsControlJustPressed = unknown\nIsControlJustPressed(0, 38)",
        "function IsControlJustPressed(...) end\nIsControlJustPressed(0, 38)",
        "_G.IsControlJustPressed = function(...) end\nIsControlJustPressed(0, 38)",
        "_ENV['IsControlJustPressed'] = function(...) end\nIsControlJustPressed(0, 38)",
        "local _ENV = {}\nIsControlJustPressed(0, 38)",
        "_ENV = {}\nIsControlJustPressed(0, 38)",
        "local N_0x580417101DDB492F = function(...) end\nN_0x580417101DDB492F(0, 38)",
        "local SetPedConfigFlag = function(...) end\nSetPedConfigFlag(ped, 38, true)",
    ];
    client.open_with(CLIENT, cases[0]);
    for (version, text) in cases.into_iter().enumerate() {
        client.change(CLIENT, version as i32 + 2, text);
        let (line, character) = pos(text, "38", 0);
        assert!(client.hover_text(CLIENT, line, character).is_empty(), "must not label shadowed call: {text}");
    }
    // A local declaration in a different scope must not hide the real native here.
    let text = "do local IsControlJustPressed = function(...) end end\nIsControlJustPressed(0, 38)";
    client.change(CLIENT, 100, text);
    let (line, character) = pos(text, "38", 0);
    assert!(client.hover_text(CLIENT, line, character).contains("INPUT_PICKUP"));

    // A definition in a visible resource file must suppress the native annotation too.
    let other = "myresource/shared/config.lua";
    client.open_with(other, "IsControlJustPressed = function(...) end");
    assert!(client.hover_text(CLIENT, line, character).is_empty());
    client.change(other, 2, "");
    assert!(
        client.hover_text(CLIENT, line, character).contains("INPUT_PICKUP"),
        "removing an override restores native hover"
    );
}

#[test]
fn hover_keeps_literal_types_of_inline_loop_tables() {
    let mut client = Client::start(fixture_root());
    let text = "\
for _, sex in pairs({ 'male', 'female' }) do end
for i, n in ipairs({ 1, 2, extra = 'x' }) do end
for key, value in pairs({ a = true, [3] = 'c' }) do end
";
    client.open_with(CLIENT, text);
    let cases = [
        ("sex", "sex: \"male\"|\"female\""),
        ("i,", "i: integer"),
        ("n in", "n: 1|2"),
        ("key", "key: \"a\"|3"),
        ("value", "value: true|\"c\""),
    ];
    for (needle, expected) in cases {
        let (l, c) = pos(text, needle, 0);
        let hover = client.hover_text(CLIENT, l, c);
        assert!(hover.contains(expected), "{needle}: expected {expected:?} in {hover}");
    }
}

#[test]
fn hover_indexes_fields_with_literal_typed_keys() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@type ['male', 'female']
local sexes = { 'male', 'female' }

---@param metadata { male: table, female: table, age: integer }
---@param field 'male'|'age'
---@param kind GarageKind
local function send(metadata, field, kind)
    for _, sex in pairs({ 'male', 'female' }) do
        local sexData = metadata[sex]
    end
    for _, listed in pairs(sexes) do
        local listedData = metadata[listed]
    end
    local either = metadata[field]
    local missing = metadata[kind]
end
";
    client.open_with(CLIENT, text);
    let cases = [
        ("sexData", "sexData: table"),
        ("listed in", "listed: \"male\"|\"female\""),
        ("listedData", "listedData: table"),
        ("either", "either: table|integer"),
        ("missing", "missing: unknown"),
    ];
    for (needle, expected) in cases {
        let (l, c) = pos(text, needle, 0);
        let hover = client.hover_text(CLIENT, l, c);
        assert!(hover.contains(expected), "{needle}: expected {expected:?} in {hover}");
    }
}

#[test]
fn hover_infers_loop_variables_of_top_level_tables() {
    let mut client = Client::start(fixture_root());
    let text = "\
local list = { 'male', 'female' }
local mixed = { 'a', count = 1, [10] = true }
local garages = { legion = { label = 'Legion' }, pillbox = { label = 'Pillbox' } }
local first = list[1]
for _, item in pairs(list) do end
for k, v in pairs(mixed) do end
for i, element in ipairs(mixed) do end
for name, garage in pairs(garages) do end
local lookup = { [1] = 'one', [2] = 'two' }
local fromLookup = lookup[1]
for _, looked in ipairs(lookup) do end
local flagged = { 'a', [true] = 5 }
local sparse = { 'a', [10] = true }
";
    client.open_with(CLIENT, text);
    let cases = [
        ("list", "local list: string[]"),
        ("first", "first: string"),
        ("item", "item: string"),
        ("k, v", "k: string|integer"),
        ("v in", "v: integer|string|boolean"),
        // `ipairs` stops before `[10]`.
        ("element", "element: string\n"),
        ("name,", "name: string"),
        ("garage in", "label: string"),
        ("lookup =", "local lookup: string[]"),
        ("fromLookup", "fromLookup: string"),
        ("looked", "looked: string"),
        ("flagged", "local flagged: table"),
        ("sparse", "local sparse: (string|boolean)[]"),
    ];
    for (needle, expected) in cases {
        let (l, c) = pos(text, needle, 0);
        let hover = client.hover_text(CLIENT, l, c);
        assert!(hover.contains(expected), "{needle}: expected {expected:?} in {hover}");
    }
}

#[test]
fn hover_infers_loops_over_classes_unions_next_and_mixed_tables() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@param payload Garage
---@param both string[]|table<string, integer>
---@param t table<string, boolean>
local function f(payload, both, t)
    for k1, v1 in pairs(payload) do end
    for k2, v2 in pairs(both) do end
    for k3, v3 in next, t do end
    local mixed = { 'a', x = 1 }
    for k4, v4 in pairs(mixed) do end
    local keyed = { [1] = 'a', [2] = 5 }
    local fromKeyed = keyed[1]
    local flags = { 'a', [true] = 5 }
    for _, flagged in ipairs(flags) do end
    for flag in pairs(flags) do end
end
";
    client.open_with(CLIENT, text);
    let cases: &[(&str, &[&str])] = &[
        ("flags =", &["flags: { [integer]: string, [boolean]: integer }"]),
        ("flagged", &["flagged: string\n"]),
        ("flag in", &["flag: integer|boolean"]),
        ("k1", &["k1: string"]),
        ("v1", &["coords: vector3", "type GarageKind ="]),
        ("k2", &["k2: integer|string"]),
        ("v2", &["v2: string|integer"]),
        ("k3", &["k3: string"]),
        ("v3", &["v3: boolean"]),
        ("k4", &["k4: string|integer"]),
        ("v4", &["v4: integer|string"]),
        ("fromKeyed", &["fromKeyed: string|integer"]),
    ];
    for &(needle, expected) in cases {
        let (l, c) = pos(text, needle, 0);
        let hover = client.hover_text(CLIENT, l, c);
        for part in expected {
            assert!(hover.contains(part), "{needle}: expected {part:?} in {hover}");
        }
    }
}

#[test]
fn hover_indexes_global_arrays_and_unions() {
    let mut client = Client::start(fixture_root());
    let text = "\
TestShop = {}
TestShop.Items = { 'bread', 'water' }
TestShop.Lookup = { [1] = 'one', [2] = 2 }
local firstItem = TestShop.Items[1]
for _, item in ipairs(TestShop.Items) do end
for k, v in pairs(TestShop.Lookup) do end

---@param either string[]|integer[]
local function pick(either)
    local picked = either[1]
end

local shelf = { 'bread', 'water' }

---@param slot string
local function restock(index, slot)
    local byIndex = shelf[index]
    local byNumber = TestShop.Items[tonumber(slot)]
    local bySlot = shelf[slot]
end
";
    client.open_with(CLIENT, text);
    let cases = [
        ("Items[1]", "TestShop.Items: string[]"),
        ("firstItem", "firstItem: string"),
        ("item in", "item: string"),
        ("k, v", "k: integer"),
        ("v in", "v: string|integer"),
        ("picked", "picked: string|integer"),
        ("byIndex", "byIndex: string"),
        ("byNumber", "byNumber: string"),
        ("bySlot", "bySlot: unknown"),
    ];
    for (needle, expected) in cases {
        let (l, c) = pos(text, needle, 0);
        let hover = client.hover_text(CLIENT, l, c);
        assert!(hover.contains(expected), "{needle}: expected {expected:?} in {hover}");
    }
}

#[test]
fn hover_picks_the_overload_a_call_fits() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@overload fun(name: string): string
---@param id integer
---@return integer
local function find(id) end

---@overload fun(): boolean
---@param x string
---@return string
local function arity(x) end

---@overload fun(name: string, cb: fun(found: string))
---@param id integer
---@param cb fun(found: integer)
local function lookup(id, cb) end

---@overload fun(filter: table?, cb: fun(found: string))
---@param id? integer
---@param cb fun(found: integer)
local function search(id, cb) end

---@class OverloadedShop
local OverloadedShop = {}

---@overload fun(self: OverloadedShop, label: string): string
---@param id integer
---@return integer
function OverloadedShop:price(id) end

---@overload fun(label: string): string
---@param id integer
---@return integer
function OverloadedShop:stock(id) end

local byId = find(1)
local byName = find('x')
local none = arity()
lookup('x', function(named) end)
lookup(1, function(numbered) end)
search(nil, function(byNil) end)
local priceByLabel = OverloadedShop:price('bread')
local priceById = OverloadedShop:price(1)
local stockByLabel = OverloadedShop:stock('bread')
local stockViaDot = OverloadedShop.stock(OverloadedShop, 'bread')
local priceViaDot = OverloadedShop.price(OverloadedShop, 'bread')
local flat = vec(1, 2)
local deep = vec(1, 2, 3)
";
    client.open_with(CLIENT, text);
    let cases = [
        ("byId", "byId: integer"),
        ("byName", "byName: string"),
        ("none", "none: boolean"),
        ("named", "named: string"),
        ("numbered", "numbered: integer"),
        // `nil` fills the optional `id`, so the declared signature still fits.
        ("byNil", "byNil: integer"),
        ("priceByLabel", "priceByLabel: string"),
        ("priceById", "priceById: integer"),
        ("stockByLabel", "stockByLabel: string"),
        ("stockViaDot", "stockViaDot: string"),
        ("priceViaDot", "priceViaDot: string"),
        // `vec(...)` takes any number of values, but its overloads name the exact ones.
        ("flat", "flat: vector2"),
        ("deep", "deep: vector3"),
    ];
    for (needle, expected) in cases {
        let (l, c) = pos(text, needle, 0);
        let hover = client.hover_text(CLIENT, l, c);
        assert!(hover.contains(expected), "{needle}: expected {expected:?} in {hover}");
    }
}

#[test]
fn hover_binds_generics_from_arguments_and_callbacks() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@generic K, V, RK, RV
---@param tbl table<K, V>
---@param fun fun(value: V, key: K): RV, RK
---@return table<RK, RV>
function table.mapEntries(tbl, fun)
    local result = {}
    for key, value in pairs(tbl) do
        local newValue, newKey = fun(value, key)
        result[newKey or key] = newValue
    end
    return result
end

local function normalize(step)
    return step / 10
end

---@generic T
---@param value? T
---@return T|string
local function orName(value) end

---@generic T
---@param value T
---@param onBox? fun(box: { value: T })
---@return { value: T }
local function box(value, onBox) end

---@param steps { [number]: number }
local function send(steps)
    local mapped = table.mapEntries(steps, function(step, featureId)
        return normalize(step), tostring(featureId)
    end)
    local fromList = table.mapEntries({ 'a', 'b' }, function(letter, position)
        return position, letter
    end)
    local unbound = table.mapEntries(steps, function() end)
    local named = orName()
    local boxed = box(1, function(opened) end)
end
";
    client.open_with(CLIENT, text);
    let cases = [
        ("mapped", "mapped: table<string, number>"),
        ("step, f", "step: number"),
        ("featureId", "featureId: number"),
        ("fromList", "fromList: table<string, integer>"),
        ("letter,", "letter: string"),
        ("position)", "position: integer"),
        ("unbound", "unbound: table<unknown, unknown>"),
        ("named =", "named: string"),
        ("boxed", "value: integer"),
        ("opened", "value: integer"),
        // Inside the generic function its parameters stay generic.
        ("key, value", "key: K"),
    ];
    for (needle, expected) in cases {
        let (l, c) = pos(text, needle, 0);
        let hover = client.hover_text(CLIENT, l, c);
        assert!(hover.contains(expected), "{needle}: expected {expected:?} in {hover}");
    }
}

#[test]
fn hover_expands_aliases_and_lists_members_only_for_tables() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@alias Test.Name string
---@param kind GarageKind
---@param name Test.Name
---@param garage Garage|string
---@param kinds GarageKind[]
---@param owned Garage
local function describe(kind, name, garage, kinds, owned)
    for _, each in ipairs(kinds) do end
    print(owned.kind)
end
";
    client.open_with(CLIENT, text);
    let garage_kind = "type GarageKind = \"public\"|\"job\"|\"gang\"";
    let cases: &[(&str, u32, &[&str])] = &[
        ("(kind", 1, &["kind: GarageKind", garage_kind]),
        ("name, garage", 0, &["name: Test.Name", "type Test.Name = string"]),
        ("garage, kinds", 0, &["point: GaragePoint"]),
        ("each", 0, &["each: GarageKind", garage_kind]),
        ("owned.kind", 6, &["Garage.kind: GarageKind", garage_kind]),
    ];
    for &(needle, delta, expected) in cases {
        let (l, c) = pos(text, needle, delta);
        let hover = client.hover_text(CLIENT, l, c);
        for part in expected {
            assert!(hover.contains(part), "{needle}: expected {part:?} in {hover}");
        }
        assert!(!hover.contains("byte"), "{needle}: lists the string library in {hover}");
    }
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
fn excluded_files_stay_out_and_ignored_files_stay_quiet() {
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
        "qbx-ignore-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
    )));
    let write = |relative: &str, text: &str| {
        let path = fixture.0.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    };
    write("qbxlint.toml", "exclude = ['skip/**']\nignore_diagnostics = ['vendor/']\n");
    write(
        "fxmanifest.lua",
        "fx_version 'cerulean'\ngame 'gta5'\nclient_scripts { 'vendor/*.lua', 'skip/*.lua', 'main.lua' }\n",
    );
    write("vendor/lib.lua", "VendorApi = {}\nCitizen.Wait(0)\n");
    write("skip/old.lua", "SkippedApi = {}\nCitizen.Wait(0)\n");
    write("main.lua", "print(VendorApi, SkippedApi)\n");
    let mut client = Client::start(fixture.0.clone());
    let undefined = |client: &mut Client| {
        client.diagnostics_for("main.lua");
        let list = client.diagnostics[&client.uri("main.lua").to_string()].as_array().unwrap().clone();
        list.iter().map(|d| d["message"].as_str().unwrap().to_string()).collect::<Vec<_>>()
    };

    let messages = undefined(&mut client);
    assert!(messages.len() == 1 && messages[0].contains("SkippedApi"), "{messages:?}");
    assert_eq!(client.diagnostics_for("vendor/lib.lua"), []);
    assert_eq!(client.diagnostics_for("skip/old.lua"), []);
    let symbols = client.request("workspace/symbol", json!({ "query": "VendorApi" }));
    assert!(!symbols.as_array().unwrap().is_empty(), "ignored files are still indexed: {symbols}");

    client.open("vendor/lib.lua");
    assert_eq!(client.diagnostics_for("vendor/lib.lua"), []);
    client.open("skip/old.lua");
    assert_eq!(client.diagnostics_for("skip/old.lua"), []);
    assert_eq!(undefined(&mut client).len(), 1, "an open excluded file is not indexed");
    client.notify("textDocument/didClose", json!({ "textDocument": { "uri": client.uri("skip/old.lua") } }));
    assert_eq!(client.diagnostics_for("skip/old.lua"), [], "a closed excluded file stays out of the Problems panel");
    assert_eq!(undefined(&mut client).len(), 1);

    write("skip/new.lua", "NewApi = {}\nCitizen.Wait(0)\n");
    client.notify(
        "workspace/didChangeWatchedFiles",
        json!({ "changes": [{ "uri": client.uri("skip/new.lua"), "type": 1 }] }),
    );
    assert_eq!(client.diagnostics_for("skip/new.lua"), []);
    let symbols = client.request("workspace/symbol", json!({ "query": "NewApi" }));
    assert!(symbols.as_array().unwrap().is_empty(), "{symbols}");
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

fn framework_fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/framework_callbacks")
}

const FRAMEWORK_CLIENT: &str = "adapters/client.lua";
const FRAMEWORK_IMPORTS: &str =
    "local Core = exports['qb-core']:GetCoreObject()\nlocal Framework = exports.es_extended:getSharedObject()\nlocal QB = Core\nlocal ESX = Framework\n";

fn framework_definitions(client: &mut Client, relative: &str, text: &str, needle: &str) -> Value {
    let (line, column) = pos(text, needle, 2);
    client.request("textDocument/definition", client.position_params(relative, line, column))
}

fn framework_hints(client: &mut Client, relative: &str, line: u32) -> Vec<String> {
    let hints = client.request(
        "textDocument/inlayHint",
        json!({ "textDocument": { "uri": client.uri(relative) }, "range": {
            "start": { "line": line, "character": 0 }, "end": { "line": line + 1, "character": 0 }
        } }),
    );
    hints.as_array().unwrap().iter().filter_map(|hint| hint["label"].as_str().map(str::to_owned)).collect()
}

#[test]
fn framework_callbacks_keep_completion_and_navigation_in_their_own_family() {
    let mut client = Client::start(framework_fixture_root());
    client.open_with(FRAMEWORK_CLIENT, "");
    for (version, (call, expected, definition)) in (2..).zip([
        ("QB.Functions.TriggerCallback", vec!["qb:guarded", "qb:only", "shared:call"], "adapters/server/qb.lua"),
        ("ESX.TriggerServerCallback", vec!["esx:imported", "esx:only", "shared:call"], "adapters/server/esx.lua"),
        ("lib.callback.await", vec!["ox:only", "shared:call"], "adapters/server/other.lua"),
        ("TriggerServerEvent", vec!["native:only", "shared:call"], "adapters/server/other.lua"),
    ]) {
        let text = format!("{FRAMEWORK_IMPORTS}{call}('')\n{call}('shared:call', function() end, 1, 2)\n");
        client.change(FRAMEWORK_CLIENT, version, &text);
        let (line, column) = pos(&text, "('')", 2);
        let mut labels = client.completion_labels(FRAMEWORK_CLIENT, line, column);
        labels.sort();
        assert_eq!(labels, expected, "{call}");

        let found = framework_definitions(&mut client, FRAMEWORK_CLIENT, &text, "shared:call");
        let locations = found.as_array().unwrap();
        assert_eq!(locations.len(), 1, "{call}: {found}");
        assert_eq!(locations[0]["uri"], client.uri(definition).as_str(), "{call}: {found}");
        let source = std::fs::read_to_string(client.root.join(definition)).unwrap();
        let registration =
            if call == "TriggerServerEvent" { "RegisterNetEvent('shared:call'" } else { "'shared:call'" };
        let (registration_line, _) = pos(&source, registration, 0);
        assert_eq!(locations[0]["range"]["start"]["line"], registration_line, "{call}: {found}");
    }
}

#[test]
fn framework_callbacks_show_typed_payloads_without_source_response_or_async_returns() {
    let mut client = Client::start(framework_fixture_root());
    let text = client.open(FRAMEWORK_CLIENT);
    for (call, value, family, payload, other_payload, expected_label, handler_file) in [
        (
            "QB.Functions.TriggerCallback",
            "'water'",
            "QB-Core callback",
            "qbItem",
            "esxVehicle",
            "QB.Functions.TriggerCallback(name: string, cb: function, qbItem: string, qbAmount: integer)",
            "qb.lua",
        ),
        (
            "ESX.TriggerServerCallback",
            "42",
            "ESX callback",
            "esxVehicle",
            "qbItem",
            "ESX.TriggerServerCallback(name: string, cb: function, esxVehicle: number, esxDepot: string)",
            "esx.lua",
        ),
    ] {
        let call_start = text.find(call).unwrap();
        let call_text =
            &text[call_start..text[call_start..].find('\n').map(|end| call_start + end).unwrap_or(text.len())];
        let (line, column) = pos(&text, call_text, call_text.find(value).unwrap() as u32 + 1);
        let result =
            client.request("textDocument/signatureHelp", client.position_params(FRAMEWORK_CLIENT, line, column));
        assert_eq!(result["signatures"][0]["label"], expected_label, "{result}");
        assert_eq!(result["activeParameter"], 2, "{result}");
        let note = result["signatures"][0]["documentation"]["value"].as_str().unwrap_or_default();
        assert!(note.contains(handler_file), "{note}");
        let name_column = call_text.find("shared:call").unwrap() as u32 + 2;
        let hover = client.hover_text(FRAMEWORK_CLIENT, line, name_column);
        assert!(hover.contains(family) && hover.contains(payload) && hover.contains(handler_file), "{hover}");
        assert!(!hover.contains(other_payload) && !hover.contains("source: integer"), "{hover}");
        assert!(hover.to_lowercase().contains("asynchronous"), "{hover}");
        let hints = framework_hints(&mut client, FRAMEWORK_CLIENT, line);
        assert!(hints.contains(&format!("{payload}:")), "{hints:?}");
        assert!(!hints.contains(&"source:".to_string()) && !hints.contains(&format!("{other_payload}:")), "{hints:?}");
    }
    for (call, expected, forbidden) in
        [("lib.callback.await", "oxPayload:", "nativePayload:"), ("TriggerServerEvent", "nativePayload:", "oxPayload:")]
    {
        let (line, _) = pos(&text, call, 0);
        let hints = framework_hints(&mut client, FRAMEWORK_CLIENT, line);
        assert!(hints.contains(&expected.to_string()) && !hints.contains(&forbidden.to_string()), "{hints:?}");
        assert!(!hints.iter().any(|hint| hint.starts_with("qb") || hint.starts_with("esx")), "{hints:?}");
    }
}

#[test]
fn framework_callbacks_complete_whole_strings_for_minimal_and_snippet_clients() {
    for snippets in [false, true] {
        let mut client = Client::start_with_capabilities(
            framework_fixture_root(),
            json!({ "textDocument": { "completion": { "completionItem": { "snippetSupport": snippets } } } }),
        );
        client.open_with(FRAMEWORK_CLIENT, "");
        for (version, (call, prefix, expected)) in (2..).zip([
            ("QB.Functions.TriggerCallback", "qb:", "qb:only"),
            ("ESX.TriggerServerCallback", "esx:", "esx:only"),
        ]) {
            let head = format!("local emoji = '🚗'; {call}('");
            let text = format!("{FRAMEWORK_IMPORTS}{head}{prefix}stale', function() end)");
            let line = FRAMEWORK_IMPORTS.lines().count() as u32;
            let start = head.encode_utf16().count() as u32;
            client.change(FRAMEWORK_CLIENT, version, &text);
            let result = client.request(
                "textDocument/completion",
                client.position_params(FRAMEWORK_CLIENT, line, start + prefix.len() as u32),
            );
            let item = result["items"].as_array().unwrap().iter().find(|item| item["label"] == expected).unwrap();
            assert_eq!(
                item["textEdit"],
                json!({ "range": {
                "start": { "line": line, "character": start },
                "end": { "line": line, "character": start + prefix.len() as u32 + 5 }
            }, "newText": expected }),
                "{result}"
            );
            assert!(item["insertTextFormat"].is_null(), "literal event names are not snippets: {item}");
        }
    }
}

#[test]
fn framework_callbacks_require_proven_unmodified_roots_and_the_name_argument() {
    let mut client = Client::start(framework_fixture_root());
    client.open_with(FRAMEWORK_CLIENT, "");
    let cases = [
        "local QB = {}\nQB.Functions.TriggerCallback('qb:only', function() end, 7)",
        "local function demo(QB)\nQB.Functions.TriggerCallback('qb:only', function() end, 7)\nend",
        "local QB = exports['qb-core']:GetCoreObject()\nQB = {}\nQB.Functions.TriggerCallback('qb:only', function() end, 7)",
        "local QB = exports['qb-core']:GetCoreObject()\nQB.Functions.TriggerCallback = function() end\nQB.Functions.TriggerCallback('qb:only', function() end, 7)",
        "local exports = {}\nlocal QB = exports['qb-core']:GetCoreObject()\nQB.Functions.TriggerCallback('qb:only', function() end, 7)",
        "_G.exports = {}\nlocal QB = exports['qb-core']:GetCoreObject()\nQB.Functions.TriggerCallback('qb:only', function() end, 7)",
        "_ENV['exports'] = {}\nlocal QB = exports['qb-core']:GetCoreObject()\nQB.Functions.TriggerCallback('qb:only', function() end, 7)",
        "_G.exports['qb-core'].GetCoreObject = function() return {} end\nlocal QB = exports['qb-core']:GetCoreObject()\nQB.Functions.TriggerCallback('qb:only', function() end, 7)",
        "local _ENV = {}\nlocal QB = exports['qb-core']:GetCoreObject()\nQB.Functions.TriggerCallback('qb:only', function() end, 7)",
        "local QB = exports['qb-core']:GetCoreObject('subset')\nQB.Functions.TriggerCallback('qb:only', function() end, 7)",
        "local QB = exports['qb-core']:GetCoreObject()\nQB.Functions:TriggerCallback('qb:only', function() end, 7)",
        "local QB = exports['qb-core']:GetCoreObject()\nQB.Functions.TriggerCallback('missing', function() end, 'qb:only')",
        "local QB = exports['qb-core']:GetCoreObject()\nlocal name = 'qb:only'\nQB.Functions.TriggerCallback(name, function() end, 7)",
        "local ESX = {}\nESX.TriggerServerCallback('esx:only', function() end, 7)",
        "local ESX = exports.es_extended:getSharedObject()\nESX.TriggerServerCallback = function() end\nESX.TriggerServerCallback('esx:only', function() end, 7)",
        "local Core = exports.es_extended:getSharedObject()\nlocal ESX = Core\nCore.TriggerServerCallback = function() end\nESX.TriggerServerCallback('esx:only', function() end, 7)",
        "local ESX = exports.es_extended:getSharedObject()\nESX:TriggerServerCallback('esx:only', function() end, 7)",
        "print('qb:only')",
    ];
    for (version, text) in (2..).zip(cases) {
        client.change(FRAMEWORK_CLIENT, version, text);
        let needle = if text.contains("qb:only") { "qb:only" } else { "esx:only" };
        let (line, column) = pos(text, needle, 2);
        let labels = client.completion_labels(FRAMEWORK_CLIENT, line, column);
        assert!(!labels.contains(&needle.to_string()), "{text}: {labels:?}");
        let found = framework_definitions(&mut client, FRAMEWORK_CLIENT, text, needle);
        assert!(found.is_null() || found.as_array().is_some_and(Vec::is_empty), "{text}: {found}");
        let hover = client.hover_text(FRAMEWORK_CLIENT, line, column);
        assert!(!hover.contains("QB-Core callback") && !hover.contains("ESX callback"), "{text}: {hover}");
        let signature =
            client.request("textDocument/signatureHelp", client.position_params(FRAMEWORK_CLIENT, line, column));
        let rendered = signature.to_string();
        assert!(!rendered.contains("qbUnique") && !rendered.contains("esxUnique"), "{text}: {signature}");
    }

    client.open_with("imported_esx/client.lua", "");
    for (version, mutation) in (2..).zip([
        "_G.ESX = {}",
        "_ENV['ESX'] = {}",
        "_G.ESX.TriggerServerCallback = function() end",
        "_ENV['ESX']['TriggerServerCallback'] = function() end",
    ]) {
        let text = format!("{mutation}\nESX.TriggerServerCallback('esx:imported', function() end, 7)");
        client.change("imported_esx/client.lua", version, &text);
        let (line, column) = pos(&text, "esx:imported", 2);
        let labels = client.completion_labels("imported_esx/client.lua", line, column);
        assert!(!labels.contains(&"esx:imported".to_string()), "{text}: {labels:?}");
        let found = framework_definitions(&mut client, "imported_esx/client.lua", &text, "esx:imported");
        assert!(found.is_null() || found.as_array().is_some_and(Vec::is_empty), "{text}: {found}");
    }
}

#[test]
fn framework_callbacks_honor_server_registration_client_trigger_and_shared_guards() {
    let mut client = Client::start(framework_fixture_root());
    let shared = client.open("adapters/shared.lua");
    let (line, column) = pos(&shared, "'guarded'", 2);
    let signature =
        client.request("textDocument/signatureHelp", client.position_params("adapters/shared.lua", line, column));
    assert!(signature["signatures"][0]["label"].as_str().unwrap_or_default().contains("guardedPayload"), "{signature}");

    let imported = client.open("imported_esx/client.lua");
    let definitions = framework_definitions(&mut client, "imported_esx/client.lua", &imported, "esx:imported");
    assert_eq!(definitions[0]["uri"], client.uri("imported_esx/server.lua").as_str(), "{definitions}");
    let (line, column) = pos(&imported, "'imported'", 2);
    let signature =
        client.request("textDocument/signatureHelp", client.position_params("imported_esx/client.lua", line, column));
    assert!(
        signature["signatures"][0]["label"].as_str().unwrap_or_default().contains("importedPayload"),
        "{signature}"
    );

    client.open_with(FRAMEWORK_CLIENT, "");
    let wrong_registration = format!("{FRAMEWORK_IMPORTS}QB.Functions.CreateCallback('qb:client-invalid', function(source, cb, badPayload) end)\nESX.RegisterServerCallback('esx:client-invalid', function(source, cb, badPayload) end)\nQB.Functions.TriggerCallback('')\nESX.TriggerServerCallback('')");
    client.change(FRAMEWORK_CLIENT, 2, &wrong_registration);
    for call in ["QB.Functions.TriggerCallback('')", "ESX.TriggerServerCallback('')"] {
        let (line, column) = pos(&wrong_registration, call, call.find("''").unwrap() as u32 + 1);
        let labels = client.completion_labels(FRAMEWORK_CLIENT, line, column);
        assert!(!labels.iter().any(|label| label.ends_with("client-invalid")), "{labels:?}");
    }
    for (relative, text) in [
        ("adapters/server/wrongside.lua", format!("{FRAMEWORK_IMPORTS}QB.Functions.TriggerCallback('qb:only', function() end, 7)")),
        ("adapters/shared.lua", format!("{FRAMEWORK_IMPORTS}QB.Functions.CreateCallback('qb:unguarded', function(source, cb, badPayload) end)\nQB.Functions.TriggerCallback('qb:only', function() end, 7)")),
    ] {
        if relative == "adapters/shared.lua" { client.change(relative, 2, &text); } else { client.open_with(relative, &text); }
        let (line, column) = pos(&text, "qb:only", 2);
        let labels = client.completion_labels(relative, line, column);
        assert!(!labels.contains(&"qb:only".to_string()), "{relative}: {labels:?}");
        let found = framework_definitions(&mut client, relative, &text, "qb:only");
        assert!(found.is_null() || found.as_array().is_some_and(Vec::is_empty), "{relative}: {found}");
        let signature = client.request("textDocument/signatureHelp", client.position_params(relative, line, column));
        assert!(!signature.to_string().contains("qbUnique"), "{relative}: {signature}");
    }
}

#[test]
fn framework_callbacks_refresh_unsaved_registration_names_and_payloads() {
    let mut client = Client::start(framework_fixture_root());
    let original = client.open("adapters/server/qb.lua");
    let client_text = format!("{FRAMEWORK_IMPORTS}QB.Functions.TriggerCallback('qb:renamed', function() end, 'item', 3)\nQB.Functions.TriggerCallback('')");
    client.open_with(FRAMEWORK_CLIENT, &client_text);
    let (line, column) = pos(&client_text, "('')", 2);
    assert!(client.completion_labels(FRAMEWORK_CLIENT, line, column).contains(&"qb:only".to_string()));
    let changed = original.replace("qb:only", "qb:renamed").replace("qbUnique", "freshPayload");
    client.change("adapters/server/qb.lua", 2, &changed);
    let labels = client.completion_labels(FRAMEWORK_CLIENT, line, column);
    assert!(labels.contains(&"qb:renamed".to_string()) && !labels.contains(&"qb:only".to_string()), "{labels:?}");
    let (call_line, call_column) = pos(&client_text, "'item'", 2);
    let signature =
        client.request("textDocument/signatureHelp", client.position_params(FRAMEWORK_CLIENT, call_line, call_column));
    assert!(signature["signatures"][0]["label"].as_str().unwrap_or_default().contains("freshPayload"), "{signature}");
    let definitions = framework_definitions(&mut client, FRAMEWORK_CLIENT, &client_text, "qb:renamed");
    assert_eq!(definitions[0]["uri"], client.uri("adapters/server/qb.lua").as_str(), "{definitions}");

    client.change("adapters/server/qb.lua", 3, "local QB = exports['qb-core']:GetCoreObject()\nlocal name = 'qb:renamed'\nQB.Functions.CreateCallback(name, function(source, cb, shouldNotInfer) end)\n");
    let labels = client.completion_labels(FRAMEWORK_CLIENT, line, column);
    assert!(!labels.contains(&"qb:renamed".to_string()) && !labels.contains(&"qb:only".to_string()), "{labels:?}");
    let signature =
        client.request("textDocument/signatureHelp", client.position_params(FRAMEWORK_CLIENT, call_line, call_column));
    assert!(
        !signature.to_string().contains("freshPayload") && !signature.to_string().contains("shouldNotInfer"),
        "{signature}"
    );
}

#[test]
fn framework_callbacks_do_not_choose_between_conflicting_handler_payloads() {
    let mut client = Client::start(framework_fixture_root());
    client.open_with("adapters/server/conflict.lua", "local QB = exports['qb-core']:GetCoreObject()\nQB.Functions.CreateCallback('shared:call', function(source, cb, conflictingPayload) end)\n");
    let text = client.open(FRAMEWORK_CLIENT);
    let definitions = framework_definitions(&mut client, FRAMEWORK_CLIENT, &text, "shared:call");
    assert_eq!(definitions.as_array().unwrap().len(), 2, "{definitions}");
    let (line, column) = pos(&text, "'water'", 2);
    let signature =
        client.request("textDocument/signatureHelp", client.position_params(FRAMEWORK_CLIENT, line, column));
    assert!(
        !signature.to_string().contains("qbItem") && !signature.to_string().contains("conflictingPayload"),
        "{signature}"
    );
    let hints = framework_hints(&mut client, FRAMEWORK_CLIENT, line);
    assert!(
        !hints.contains(&"qbItem:".to_string()) && !hints.contains(&"conflictingPayload:".to_string()),
        "{hints:?}"
    );

    client.change(
        "adapters/server/conflict.lua",
        2,
        "local QB = exports['qb-core']:GetCoreObject()\nQB.Functions.CreateCallback('shared:call', unknownHandler)\n",
    );
    let definitions = framework_definitions(&mut client, FRAMEWORK_CLIENT, &text, "shared:call");
    assert_eq!(
        definitions.as_array().unwrap().len(),
        2,
        "an unresolved handler still has a registration: {definitions}"
    );
    let signature =
        client.request("textDocument/signatureHelp", client.position_params(FRAMEWORK_CLIENT, line, column));
    assert!(
        !signature.to_string().contains("qbItem"),
        "an unresolved duplicate must not select the known handler: {signature}"
    );
    let hints = framework_hints(&mut client, FRAMEWORK_CLIENT, line);
    assert!(!hints.contains(&"qbItem:".to_string()), "{hints:?}");

    let (name_line, name_column) = pos(&text, "shared:call", 2);
    let completion =
        client.request("textDocument/completion", client.position_params(FRAMEWORK_CLIENT, name_line, name_column));
    let item = completion["items"].as_array().unwrap().iter().find(|item| item["label"] == "shared:call").unwrap();
    let detail = item["detail"].as_str().unwrap_or_default();
    assert!(!detail.contains("qbItem") && detail.contains("Multiple handlers"), "{item}");
}
