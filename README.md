# qbx-lua-ls

A language server for FiveM Lua. It reads `fxmanifest.lua` to resolve resource imports and
client/server scripts, uses LuaCATS annotations for editor help, and provides diagnostics from
[qbx-lint](https://github.com/Qbox-project/qbx-lint).

The server communicates over standard input and output using the Language Server Protocol
(LSP). Editor integrations and setup instructions live in
[qbx-editor](https://github.com/Qbox-project/qbx-editor/blob/main/docs/editors.md).
Available features depend on the editor's LSP client.

## Features

- Completion and hover for Lua symbols, FiveM natives, exports, events and callbacks.
- Reference hovers for literal control IDs in PAD natives and ped configuration flags
  in `SetPedConfigFlag` / `GetPedConfigFlag`, using bundled Cfx documentation.
- Optional read-only reference search/detail requests for native, control and ped flag browsers,
  with filters, pagination, source links and Lua insertion templates.
- Definitions, references and rename for locals, globals and fields, including static string
  keys such as `Config['name']` and supported `---@field` declarations.
- Hover and definitions for the classes, aliases and enums named in LuaCATS annotations.
- Diagnostics and quick fixes with resource and client/server context.
- Signature help, parameter hints, semantic tokens, folding and document/workspace symbols.
- QB-Core and ESX server callback completion, navigation and payload hints from local handlers.
- Whole-document formatting, configured through `qbxlint.toml`.
- Completion for manifest paths, locale keys, convars, state bag keys and LuaCATS annotations.
- Read-only resource, dependency-health, NUI callback and asset-reference requests
  for editor tabs, plus paginated diagnostic and symbol-reference queries for assistants.

Custom requests are documented in the [protocol reference](docs/protocol.md).
The VS Code asset/utility interfaces and assistant MCP adapter live in `qbx-editor`.

Opening a resource also indexes dependencies and imported scripts found in sibling resource
folders. Add other locations through the `library` setting.

Hovering `38` in `IsControlJustPressed(0, 38)` shows `INPUT_PICKUP` and its default
QWERTY/Xbox bindings. Ped configuration flag hovers show the documented symbol;
undocumented behavior is identified as such. References work offline and include
source links. ID hovers apply only to recognized native arguments, not variables,
calculated expressions or functions that shadow the native.

## Framework callbacks

The server recognizes QB-Core `Functions.CreateCallback` / `Functions.TriggerCallback`
and ESX `RegisterServerCallback` / `TriggerServerCallback`. Receiver provenance comes
from standard framework export initialization (including local aliases), supported
framework imports or the framework's own resource. Arbitrary same-named tables are
not framework receivers. Manifest sides and `IsDuplicityVersion()` guards restrict
registrations to server code and triggers to client code.

Literal callback names complete and navigate to locally indexed registrations.
Payload signature help and inlay hints omit the handler's leading `source` and `cb`;
LuaCATS annotations on local functions supply types. QB-Core, ESX, ox_lib and native
events have separate name spaces. Conflicting payload definitions suppress derived
hints, while navigation still lists the matching registrations. Handler return
values are not treated as asynchronous callback responses.

No framework API documentation is downloaded or bundled. Custom wrappers,
client-callback/Await variants and response-type inference are not included.
See the [convention and maintenance notes](docs/framework-callbacks.md) for source revisions and recognition limits.

## Build and run

Download the archive for your platform from [Releases](https://github.com/Qbox-project/qbx-lua-ls/releases).
Archives cover Windows x64, Linux x64/ARM64 (musl), and macOS x64/ARM64. You can also build
from source using the steps below.

Install stable Rust and keep these repositories next to each other. The server currently uses
local path dependencies from `qbx-lint`.

```sh
git clone https://github.com/Qbox-project/qbx-lint.git
git clone https://github.com/Qbox-project/qbx-lua-ls.git
cd qbx-lua-ls
cargo build --release --locked
```

The executable is `target/release/qbx-lua-ls`, or `target/release/qbx-lua-ls.exe` on Windows.
Put it on `PATH`, or configure its absolute path in your editor. Start it without arguments for
LSP over stdio; `--version` prints the version. It does not need a running FiveM server.

Use a resource folder or the server's `resources` folder as the editor workspace. Follow the
[editor setup instructions](https://github.com/Qbox-project/qbx-editor/blob/main/docs/editors.md)
to start the server from your editor.

## Configuration

Send this object directly as LSP `initializationOptions`:

```json
{
  "library": [],
  "diagnostics": {
    "enable": true,
    "workspace": true,
    "rules": {}
  },
  "inlayHints": { "enable": true },
  "semanticTokens": { "enable": true }
}
```

For `workspace/didChangeConfiguration`, put the same object in `settings.qbxLua` or directly in
`settings`. Restart the server after changing `library`. A discovered `qbxlint.toml` supplies
lint and formatting settings; editor rule overrides take precedence for diagnostics.

The server relies on the editor for file-watch notifications. If the client does not send them,
send a `qbx/reindex` request with `null` parameters or restart after external file or manifest
changes. Open-document edits continue to update normally.

See the [client configuration and request reference](docs/protocol.md) for the full settings
and custom requests.

## Limits

- Types support completion, hover and navigation. The server does not check assignment types
  or provide full control-flow narrowing or generic inference.
- Field references depend on the inferred owner type. Computed keys and fields reached through
  unknown types may not be found; known declarations that cannot be edited safely prevent rename.
- Formatting applies to whole documents. Range formatting is not implemented.
- Encrypted scripts cannot be analyzed. Security diagnostics are heuristic checks, not proof
  that an event handler or resource is secure.

See [CONTRIBUTING.md](CONTRIBUTING.md) for development checks and the
[releases page](https://github.com/Qbox-project/qbx-lua-ls/releases) for release notes.

## License

[GPL-3.0-or-later](LICENSE).
