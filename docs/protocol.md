# LSP client configuration

Start `qbx-lua-ls` without arguments and communicate over stdio. Give the server file-based
workspace folders through `workspaceFolders` or `rootUri`. It uses UTF-16 positions and accepts
incremental document changes.

## Settings

`initializationOptions` must contain the settings object directly, without a `qbxLua` wrapper.
For example:

```json
{
  "library": ["/srv/fivem/resources"],
  "diagnostics": {
    "enable": true,
    "workspace": true,
    "rules": { "unused-argument": "off" }
  },
  "inlayHints": { "enable": true },
  "semanticTokens": { "enable": true }
}
```

Use absolute paths in `library`; Windows paths such as `C:/server/resources` work too.

| Setting | Default | Effect |
| --- | --- | --- |
| `library` | `[]` | Extra folders to index for types, definitions and resource imports. Restart after changing it. |
| `diagnostics.enable` | `true` | Publish diagnostics. |
| `diagnostics.workspace` | `true` | Also report diagnostics for closed files in the workspace. |
| `diagnostics.rules` | `{}` | Override rule levels with `off`, `hint`, `info`, `warning` or `error`. |
| `inlayHints.enable` | `true` | Return parameter hints. |
| `semanticTokens.enable` | `true` | Return semantic highlighting tokens. |

Send updates using `workspace/didChangeConfiguration`. Its parameters may use either form:

```json
{ "settings": { "qbxLua": { "diagnostics": { "enable": false } } } }
```

```json
{ "settings": { "diagnostics": { "enable": false } } }
```

Updates replace the settings object rather than merging individual keys. Send the full settings
object when preserving other overrides. The server does not request `workspace/configuration`;
the client must provide settings during initialization or send the notification.

The server discovers `qbxlint.toml` from the first workspace root and its ancestors. Its
`[format]` section controls formatting. Without a discovered config file, the editor's formatting
request supplies indentation width and tabs/spaces. Diagnostic rule overrides from the client
take precedence over the config file.

## Client capabilities and file changes

Snippet completions are sent only when
`textDocument.completion.completionItem.snippetSupport` is `true`. Other clients receive ordinary
symbol, annotation and manifest completions without snippet placeholders.

The server requests file watches only when
`workspace.didChangeWatchedFiles.dynamicRegistration` is `true`. It watches Lua, lint config,
locale JSON and server config files through the client. It has no internal filesystem watcher
or polling loop.

If a client cannot provide file-watch notifications, open-document changes still work. After
external changes to files, manifests or dependencies, send `qbx/reindex` with `null` parameters,
or restart the server. Restart after changing workspace folders or library locations.

## Custom requests

These requests are optional conveniences for editor integrations; normal language features use
standard LSP requests.

| Request | Parameters | Result |
| --- | --- | --- |
| `qbx/status` | `null` | Object with `files`, `resources`, `openDocuments` and `natives` counts. |
| `qbx/reindex` | `null` | Rebuilds the index from disk while preserving open-document text; returns `files`, `resources` and `millis`. |
| `qbx/fileInfo` | `{ "uri": "file:///path/to/script.lua" }` | Object with `side` and `resource` (a resource name or `null`). |
| `qbx/snippets` | `{ "uri": "file:///path/to/script.lua" }` or `null` | Array of snippets with `label`, `description`, `body` and `preview`. |

`qbx/fileInfo.side` is one of `client`, `server`, `shared`, `module`, `manifest` or `standalone`.
It describes manifest placement; a guard inside the file can narrow the side of an individual
call. `qbx/snippets` returns snippet syntax in `body` even for a client that has not enabled
completion snippets, so a custom snippet picker must handle that syntax itself.
