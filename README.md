# qbx-lua-ls

A language server for FiveM Lua that stays small. It understands CfxLua 5.4 syntax, reads
`fxmanifest.lua` to know which globals and natives exist on which side, takes its types from
LuaCATS annotations, and shares its diagnostics with [`qbx-lint`](../qbx-lint).

> Status: proof of concept. It is an alternative to running `lua-language-server` with the cfxlua
> add-on, not a drop-in clone of it: the type checker is deliberately much simpler.

## Numbers

Same machine (Windows 11), same workspace: qbx_core, qbx_police, qbx_vehicleshop, ox_lib and
ox_inventory, 224 Lua files.

| | Workspace loaded | Resident memory |
| --- | --- | --- |
| lua-language-server 3.19.1 | 4.7 s | 309 MB |
| lua-language-server 3.19.1 + cfxlua natives library | 6.5 s | 458 MB |
| qbx-lua-ls | 0.21 s | 12 MB |

Requests against a 2,600 line file (`ox_inventory/modules/inventory/server.lua`): hover,
completion and definition answer in well under a millisecond, semantic tokens in ~2 ms, document
symbols in ~7 ms.

Reproduce with:

```bash
cargo build --release
node scripts/bench.mjs <workspace> [file-to-open]
node scripts/bench-luals.mjs <workspace> <path-to-lua-language-server> [library-dir ...]
```

### Why it is small

- **Closed files keep no syntax tree.** Each file is parsed once and reduced to a summary: global
  symbols, table members, classes and aliases, exports, events and the module return type. Only
  open documents keep their AST, and references in closed files are found by re-parsing on demand
  (the parser handles ~30 MB/s, so that is cheap).
- **Natives are not Lua files.** The ~9,900 native signatures and their documentation are a
  sorted table embedded in the binary and binary-searched in place; nothing is parsed or copied
  at startup.
- **Types are resolved lazily.** Symbols store references such as "class `Player`", "table
  `lib.callback`" or "module `config.shared`", which are looked up when a request needs them.
- **Dependency driven indexing.** Opening one resource indexes only that resource plus the
  resources its manifest refers to (`@ox_lib/init.lua`, `dependencies { ... }`), found by walking
  up to the surrounding `resources` folder.

## Features

| | |
| --- | --- |
| Completion | locals, side-aware globals and natives, members through classes/tables/modules, `exports.resource:Fn`, event and callback names, `require` paths, expected table fields, LuaCATS tags and types, manifest directives and paths, FiveM snippets |
| Hover | signatures, LuaCATS docs, native docs with examples and side, event handler locations |
| Navigation | definition (incl. `require` targets, event registrations, exports), references, rename, document highlight, document and workspace symbols |
| Editing help | signature help, inlay parameter hints, folding, semantic tokens |
| Diagnostics | every `qbx-lint` rule with manifest context, for the whole workspace (closed files are linted one at a time and dropped again; a save only re-lints the affected resource), quick fixes, "disable for this line" actions |
| Side awareness | natives and globals filtered by client/server, wrong-side errors, event name completion that follows the call direction (`TriggerServerEvent` only offers events handled on the server), `qbx/fileInfo` for editors |
| ox_lib | `onCache` snippet and `lib.onCache('…')` completion whose key list is read from the indexed ox_lib source |

Type sources: `---@class/@field/@alias/@enum/@type/@param/@return/@generic/@overload`, table
constructors (also behind `setmetatable`), `function Table.name()` / `Table.name = ...` anywhere
in the resource, `_ENV.name = value`, module returns through `require`/`lib.load`, exports
registered with `exports('Name', fn)`, native signatures, and callback parameter types taken from
the function being called.

### Known limits

- No type *checking* (no "cannot assign string to number"); types drive completion, hover and
  navigation only.
- No control-flow narrowing, no full generics (only simple `T` substitution), no operator
  metamethod lookup apart from vectors.
- References and rename cover locals and globals, not table fields.
- No formatter.

## Configuration

Sent as `initializationOptions` and through `workspace/didChangeConfiguration` under `qbxLua`:

```jsonc
{
  "library": ["C:/server/resources"],           // extra folders to index
  "diagnostics": { "enable": true, "rules": { "unused-argument": "off" } },
  "inlayHints": { "enable": true },
  "semanticTokens": { "enable": true }
}
```

A `qbxlint.toml` in the workspace root is honoured for diagnostics, so the editor shows what CI
will report.

`diagnostics.workspace` (default `true`) controls whether files that are not open are reported.

Custom requests: `qbx/status` (index statistics), `qbx/reindex` and `qbx/fileInfo`
(`{ uri }` → `{ side: client|server|shared|module|manifest|standalone, resource }`).

## Editors

- VS Code: [`qbx-vscode`](../qbx-vscode).
- Anything that speaks LSP over stdio, for example Neovim:

  ```lua
  vim.lsp.start({ name = 'qbx-lua-ls', cmd = { 'qbx-lua-ls' }, root_dir = vim.fs.root(0, { 'fxmanifest.lua', '.git' }) })
  ```

## Layout

| Module | Purpose |
| --- | --- |
| `types`, `luacats` | type model and LuaCATS annotation parser |
| `indexer`, `index` | per-file summaries and the workspace index with visibility rules |
| `infer` | on-demand expression typing for open documents and for the indexer |
| `workspace` | resource discovery, sides, imports, dependency indexing, lint environment |
| `features/*` | one module per LSP feature |
| `server` | stdio main loop; diagnostics are published when the message queue is idle |

The parser, scope resolver, manifest model, natives data and lint rules come from the `qbx-lint`
workspace through path dependencies; switch them to git dependencies once both are published.

## Development

```bash
cargo test            # unit tests plus end-to-end LSP tests over an in-memory connection
node scripts/probe.mjs <workspace> <virtual-file> target/release/qbx-lua-ls < snippet.lua
```

`probe.mjs` opens a virtual document and prints completions (`--^` at the end of a line) and
hovers (`--?<column>`), which is handy for checking behaviour against real resources.

## License

GPL-3.0-or-later
