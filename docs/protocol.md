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

Files matching the config's `exclude` patterns are never indexed or diagnosed, even while they are
open. Files matching `ignore_diagnostics` are indexed like any other file, so definitions, hover
and completion still reach their symbols, but the server publishes no diagnostics for them.
`ignore_diagnostics` changes apply once the client reports the saved config file. Files added to
or removed from `exclude` leave or enter the index on the next `qbx/reindex` or restart.

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
| `qbx/referenceSearch` | Search object below, or `null` for defaults. | A bounded page of native, control or ped flag summaries. |
| `qbx/referenceDetail` | `{ "id": "native:GetEntityCoords" }` | Reference detail object below, or `null` for an unknown ID. |
| `qbx/resourceDetails` | `{ "uri": "file:///path/to/resource" }` | Resource snapshot below; accepts an indexed resource folder or its selected manifest. |
| `qbx/workspaceHealth` | `null` or `{}` | Bounded workspace dependency health snapshot below. |
| `qbx/nuiResource` | `{ "uri": "file:///path/to/resource" }` | Saved NUI page metadata and indexed Lua callback registrations below. |
| `qbx/resourceAssets` | `{ "uri": "file:///path/to/resource" }` | Literal manifest asset declarations and supported native asset arguments with source locations. |
| `qbx/resources` | `{ "query"?: string, "offset"?: number, "limit"?: number }` | A page of indexed resource identities, retaining duplicate names. |
| `qbx/diagnostics` | `{ "uri"?: string, "offset"?: number, "limit"?: number }` | A current, configured diagnostic snapshot for one indexed Lua file/manifest or the workspace. |
| `qbx/symbolReferences` | `{ "uri": string, "line": number, "character": number, "includeDeclaration"?: boolean, "offset"?: number, "limit"?: number }` | A page of symbol locations, including support for closed indexed Lua files. |

`qbx/fileInfo.side` is one of `client`, `server`, `shared`, `module`, `manifest` or `standalone`.
It describes manifest placement; a guard inside the file can narrow the side of an individual
call. `qbx/snippets` returns snippet syntax in `body` even for a client that has not enabled
completion snippets, so a custom snippet picker must handle that syntax itself.

### Bundled reference search

Both reference requests work without an open document. They read the data already bundled
with the server, fetch nothing from the network, and do not write files or execute code.
The search catalog is created only when first requested. Native documentation is returned
only by the detail request, keeping search responses small.

```ts
type ReferenceKind = 'native' | 'control' | 'pedFlag';
type ReferenceSide = 'client' | 'server' | 'shared';

interface ReferenceSearchParams {
    query?: string;
    kind?: 'all' | ReferenceKind;
    side?: 'all' | ReferenceSide;
    namespace?: string;
    offset?: number;
    limit?: number;
}

interface ReferenceItem {
    id: string;
    kind: ReferenceKind;
    name: string;
    side: ReferenceSide;
    namespace?: string;
    hash?: string;
    numericId?: number;
}

interface ReferenceSearchResult {
    items: ReferenceItem[];
    total: number;
    offset: number;
    limit: number;
    namespaces: string[];
}

interface ReferenceDetail extends ReferenceItem {
    signature?: string;
    parameters?: { name: string; type: string }[];
    returns?: string[];
    documentation: string;
    sourceUrl: string;
    copyText: string;
    insertText: string;
    insertSnippet?: string;
}
```

Search defaults to an empty query, `kind: "all"`, `side: "all"`, no namespace,
`offset: 0` and `limit: 50`. Limits are clamped to 1–100 and offsets beyond the end
are clamped to `total`, returning an empty page. Offsets and limits must be nonnegative
integers. Queries may contain at most 256 Unicode characters; namespaces at most 64.
Malformed parameters return the LSP `InvalidParams` error (`-32602`).

Search is case-insensitive and matches names, aliases, native hashes with or without
`0x`, numeric IDs and default control bindings. Lua native names and underscore-separated
spellings both work. Whitespace/underscore-separated tokens must all match. Exact matches
rank before prefixes, which rank before other substrings. Ties retain native name order,
followed by controls and flags in numeric ID order. Native aliases resolve to one canonical
listing rather than duplicate rows. An empty query lists the complete selected catalog.

The `client` and `server` side filters include shared natives; `shared` includes only
shared natives. Numeric control/flag catalogs have `side: "client"`. Namespace filters
apply only to natives, including under `kind: "all"`. The returned `namespaces` array
always lists all bundled native namespaces in sorted order.

Stable IDs have the form `native:GetEntityCoords`, `control:38` or `pedFlag:48`.
Detail accepts native aliases and known `N_0x...` spellings and returns the canonical ID.
Numeric IDs require canonical decimal spelling. IDs longer than 512 Unicode characters
are rejected; other unknown IDs return `null`.

Native details include their signature, parameters, return types and available source
documentation. `copyText` is the native name; `insertText` is a Lua call with parameter
names; `insertSnippet` adds escaped numbered placeholders and a final `$0` tab stop.
For controls and flags, both copy and insert text are the numeric ID. Snippet syntax is
returned regardless of normal completion capabilities; a custom client must explicitly
use its snippet insertion API or use `insertText` instead.

Control documentation identifies default bindings, including missing/unbound values and
the possibility of remapping. Flag documentation preserves official symbols and explicitly
identifies undocumented behavior and uncertain names. Reference details and numeric hovers
share the same text. `documentation` is Markdown from bundled sources: web clients must
render it as untrusted content, disable raw HTML, and validate links before opening them.
Insertion and clipboard actions require an explicit client/user action; these server
requests themselves only return data.

### Resource details

`qbx/resourceDetails` reads the existing local index after the normal dirty-document flush.
It does not rescan directories, contact a running server, write files, or execute commands.
The URI must identify the exact folder or selected manifest of an indexed resource. Script
files, unknown folders, non-file URIs, queries, fragments, and malformed parameters return
`InvalidParams` (`-32602`) with a human-readable explanation. URIs are limited to 16,384 bytes.
Path matching normalizes lexical components and Windows case; it does not resolve filesystem
symlink aliases or select a resource by its name alone.

```ts
interface ResourceIdentity {
    name: string;
    uri: string;
    manifestUri: string;
}

interface ResourceSymbol {
    name: string;
    kind: string;
    side: 'client' | 'server' | 'shared' | 'unknown';
    location: {
        uri: string;
        range: {
            start: { line: number; character: number };
            end: { line: number; character: number };
        };
    };
    signature?: string;
}

interface ResourceRelation {
    name: string;
    kinds: ('dependency' | 'import')[];
    status: 'resolved' | 'missing' | 'ambiguous';
    targets: ResourceIdentity[];
    targetCount: number;
}

interface ResourceDetails {
    resource: ResourceIdentity;
    files: { total: number; client: number; server: number; shared: number; module: number };
    counts: { events: number; exports: number };
    events: ResourceSymbol[];
    exports: ResourceSymbol[];
    dependencies: ResourceRelation[];
    dependents: ResourceRelation[];
    constraints: string[];
    notes: string[];
    truncated: { events: number; exports: number; dependencies: number; dependents: number };
}
```

File counts cover owned indexed Lua sources, including modules not directly listed as scripts.
They exclude the manifest, built-in stubs, and imported files owned by other resources. The
side buckets are disjoint. Symbol counts are registration rows, not unique names: a network
registration and separate handler of the same name remain separate rows. Events include native
registrations/handlers and recognized ox_lib, QB-Core and ESX callbacks, with their effective
side. Trigger calls are excluded. Exports are literal Lua export registrations; manifest export
declarations and dynamically computed registrations may be absent. Locations use standard
zero-based UTF-16 LSP ranges and always refer to the original declarations.

Direct dependencies combine literal manifest `dependency` entries and `@resource/file` script
imports. They are grouped by ASCII-case-insensitive resource name, matching the existing local
resource locator; `kinds` records both origins where applicable. Resolution searches indexed
resource identities: zero candidates is `missing`, one is `resolved`, and multiple distinct
folders with that name is `ambiguous`. An import's resolved resource does not establish that
the individual imported file exists or was indexed. Dependencies beginning with `/`, such as
`/server:7290` or `/onesync`, appear separately under `constraints`.

Inverse `dependents` retain one row per dependent resource identity, even when several folders
share a name. Each row targets that dependent's folder. If several possible providers share
the selected resource's name, inverse rows have `status: "ambiguous"` and `targetCount: 1`:
the ambiguity concerns which provider the dependent uses. These are potential dependents,
not a claim about actual runtime resolution. The current manifest model does not retain
`provide` aliases; a note explains this limitation when an indexed manifest uses them.

Results are deterministic. Symbols sort by displayed name, kind, side and source location;
dependencies by normalized name; dependents by name and folder URI; target candidates by name
and URI. Each symbol list is capped at 500 rows, each relation list at 200 rows, candidates at
20 per relation, and constraints at 200. `counts` and `targetCount` retain full indexed totals.
The four `truncated` numbers are **omitted row counts**, not flags. Notes explain candidate and
constraint omissions. Displayed symbol/relation names and constraints are capped at 2,048
Unicode characters and signatures at 8,192, including a final ellipsis when shortened.
Dependency lookup uses the full name before shortening; source links remain unchanged.

Unsaved Lua changes appear after the normal index update. Save manifest changes before
refreshing details; manifest metadata and script sides use the saved manifest. External changes
need ordinary watched-file notifications, `qbx/reindex`, or a server restart. Refreshing details
alone does not discover new files. Notes also identify incomplete escrowed, unreadable,
excluded, oversized, non-Lua, or dynamically registered content. This endpoint reports local
source information, not whether a resource is installed, started, or healthy on a live server.

### Workspace health

`qbx/workspaceHealth` accepts `null` or an empty object. Other parameters return
`InvalidParams` (`-32602`). It reads the existing index after the normal dirty-document flush,
without scanning folders, resolving new files, contacting a running server, or changing files.

```ts
interface WorkspaceHealth {
    files: number;
    resources: number;
    counts: { duplicates: number; missing: number; ambiguous: number };
    issues: {
        kind: 'duplicate' | 'missing' | 'ambiguous';
        name: string;
        resource?: ResourceIdentity;
        targets: ResourceIdentity[];
        targetCount: number;
        kinds: ('dependency' | 'import')[];
    }[];
    truncated: number;
    notes: string[];
}
```

`ResourceIdentity` has the same shape as Resource Details. `resources` counts distinct
normalized resource roots. `files` counts indexed Lua sources, including standalone modules
and configured library files, excluding manifests and built-in stubs. No resource's full
symbol details are constructed: resource-name candidates are grouped once and each distinct
resource's manifest references are checked once.

`duplicates` counts names shared by multiple distinct resource folders. Each duplicate issue
has no `resource`, an empty `kinds` list, and the candidate provider folders as `targets`.
`missing` and `ambiguous` count referenced names per dependent resource, merging dependency
and import declarations into one row. Their `resource` is that dependent resource. A missing
row has no targets; an ambiguous row has multiple candidate providers. Resource names match
ASCII-case-insensitively, using the same direct dependency/import rules as Resource Details.
Runtime constraints beginning with `/` are excluded. A resolved import's resource name does
not establish whether its individual file exists. `provide` aliases are not retained by the
manifest index; a note explains this limitation when any indexed manifest uses them.

Duplicate issues are listed first in normalized-name order, followed by reference issues in
normalized dependent-folder and referenced-name order. Candidate folders sort by resource
name and path. The output is capped at 500 issue rows and 20 targets per row. `counts` and
`targetCount` retain full totals; `truncated` is the number of omitted issue rows. Notes explain
omitted candidate folders. Displayed issue and resource names are capped at 2,048 Unicode
characters including an ellipsis; matching always uses the full name and folder/manifest
URIs remain unchanged.

Save manifest changes before refreshing this snapshot. External file changes need ordinary
watched-file notifications, `qbx/reindex`, or a server restart. The report cannot assess live
server state or dependencies supplied outside the indexed workspace. Computed names and
excluded, unreadable, oversized or non-Lua source files may be absent.

### NUI resource metadata

`qbx/nuiResource` requires an object containing a `uri` for an exact indexed resource folder
or its selected manifest. It uses the same URI validation and lexical path matching as
Resource Details; invalid, unknown or script-file URIs return `InvalidParams` (`-32602`).
The request reads the existing local index after the normal dirty-document flush. It does not
parse closed Lua files, scan directories, execute Lua, start a preview, or send any network
request.

```ts
interface NuiResource {
    resource: ResourceIdentity;
    uiPage: string | null;
    callbacks: {
        name: string;
        location: {
            uri: string;
            range: {
                start: { line: number; character: number };
                end: { line: number; character: number };
            };
        };
    }[];
    truncated: number;
    notes: string[];
}
```

`uiPage` is the literal `ui_page` value retained by the saved manifest model, or `null` when
none is recorded. Local paths and remote URLs are returned unchanged; the client must decide
which targets it supports and validate them before previewing anything. A literal exceeding
16,384 bytes is omitted with a note instead of being shortened into a different path. Computed
manifest expressions are not evaluated. Save manifest changes before refreshing the page
metadata.

Callbacks are recorded during ordinary Lua indexing, separately from network events. Only
direct calls to the global `RegisterNUICallback` or `RegisterNuiCallback` with a nonempty literal
string first argument and a second argument are considered. Local or `_ENV` shadows,
same-file reassignments and direct `_G`/`_ENV` replacements are suppressed. Current visible
global replacements in other indexed files are checked at request time, so changing a global
in an unsaved document can suppress or restore results without reparsing every callback file.
This is conservative static recognition, not proof that a registration runs at runtime.
Dynamic calls, aliases and arbitrary table/environment mutation cannot be fully resolved.

Results cover owned indexed Lua code with an effective client/shared side. Recognized side
guards exclude server-only branches; server-script files are always excluded. Unclassified
modules without a client-side guard, imported external files, non-Lua and unreadable scripts
may be absent. Unsaved Lua updates use the normal index refresh. Each location points to the
complete original callback-name string literal using zero-based UTF-16 LSP coordinates.
Duplicate registrations remain separate source rows, sorted by name and source location.

The response contains at most 500 callback rows. Names over 2,048 Unicode characters are
omitted rather than shortened, preserving exact callback keys for clients that offer mock
responses. `truncated` counts all omitted rows, including oversized names, and notes explain
the omissions. This metadata does not run Lua callbacks or establish live game/server state.
The conventions follow the official [Cfx NUI callback documentation](https://docs.fivem.net/docs/scripting-manual/nui-development/nui-callbacks/)
and [fullscreen NUI documentation](https://docs.fivem.net/docs/scripting-manual/nui-development/full-screen-nui/).

### Resource asset metadata

`qbx/resourceAssets` requires `{ "uri": "file:///.../resource" }` using an exact
indexed resource folder or its manifest, as with Resource Details. It returns
source metadata; it does not read or decode asset files. The editor combines this
response with its bounded local asset inventory.

```ts
interface ResourceAssets {
    resource: ResourceIdentity;
    declarations: {
        kind: 'file' | 'client_script' | 'server_script' | 'shared_script'
            | 'ui_page' | 'loadscreen' | 'data_file' | 'map';
        value: string;
        dataType?: string;
        location: Location;
    }[];
    references: {
        kind: 'model' | 'textureDictionary' | 'texture' | 'particleAsset' | 'audioBank';
        value?: string;
        hash?: number;
        dictionary?: string;
        location: Location;
    }[];
    truncated: { declarations: number; references: number };
    notes: string[];
}
```

`Location` is an LSP file URI and zero-based UTF-16 range. Declaration values
preserve literal manifest paths, including globs and `@resource` imports.
`dataType` identifies a literal `data_file` type. Only supported top-level
manifest calls and literal arguments are collected; Lua expressions are not run.
The rules follow the [Cfx resource manifest reference](https://docs.fivem.net/docs/scripting-reference/resource-manifest/).

Lua references recognize direct native calls for models, streamed texture
dictionaries, `DrawSprite` texture names, named particle assets and audio banks.
Model arguments can be literal strings, Cfx backtick hashes, integer hashes or
literal strings passed to `GetHashKey`/`joaat`. Calculated arguments, aliases and
shadowed/redefined natives are omitted. `hash` is an unsigned 32-bit value;
string hashing is limited to ASCII. Texture names can include a `dictionary`
literal from the same call. A source reference alone does not prove that an asset
exists locally or is missing from the game.

The request uses unsaved manifest/Lua buffers where available and reads only
owned indexed Lua sources. It excludes scripts imported from other resources.
Closed sources are limited to 2 MiB per file, 32 MiB total and 2,000 files. The
response contains at most 1,000 declarations and 2,000 references; each literal
is limited to 4 KiB. `truncated` counts omitted entries, and notes describe partial
coverage. Index membership still requires normal file notifications or reindexing.

### Paginated assistant queries

These read-only requests require object parameters and reject unknown fields
with `InvalidParams` (`-32602`). They do not execute Lua or server commands.

| Request | Parameters | Items |
| --- | --- | --- |
| `qbx/resources` | Optional `query`, `offset`, `limit` | `ResourceIdentity` |
| `qbx/diagnostics` | Optional indexed Lua/manifest `uri`, `offset`, `limit` | `{ uri, range, severity, code?, message }` |
| `qbx/symbolReferences` | Indexed Lua `uri`, zero-based `line` and UTF-16 `character`; optional `includeDeclaration`, `offset`, `limit` | LSP `Location` |

Each returns `{ items, total, offset, limit, notes }`. Offsets default to zero and
cannot exceed 1,000,000. Page sizes default to 50, with a maximum of 100 for
resources and 200 for diagnostics/references. The editor assistant adapter
exposes a maximum of 100 for every tool and also checks workspace trust, file
scope and response size. Its portable MCP entry point is part of `qbx-editor`.

Resource queries match name or folder path using literal case-insensitive text
of up to 256 characters. Results sort by name and URI, preserving duplicate
resource names as separate identities. They use the current index; the adapter's
optional `refresh: true` issues `qbx/reindex` before requesting this list.

Diagnostics reuse configured Lua/manifest checks and rule overrides. Disabling
diagnostics returns an empty result with a note. The workspace-publishing switch
does not disable this explicit query. Unsaved buffers take precedence over disk;
whole-workspace queries include configured locale checks when coverage permits.
Messages are capped at 4,096 characters and omit code-action payloads. Read the
notes before interpreting a zero count or comparing separate pages.

Symbol references preserve normal resource visibility and default to including
the declaration. Invalid UTF-16 positions, built-in stubs and unindexed files
are rejected. Results sort by URI/range and remove duplicates. Diagnostic and
reference queries have additional inspection budgets and report incomplete
coverage in `notes`; totals refer to the inspected snapshot, not a live server.
They share limits of 2,000 attempted files, 2 MiB per file, 32 MiB of source and
supporting data, and 20,000 results. Invalid or binary inputs consume the read
budget too. Diagnostic support discovery shares a 20,000-directory-entry budget
for locale selection, manifest file inventory and server startup configuration.
When supporting data is incomplete, notes identify omitted checks rather than
reporting missing resources/files from an incomplete inventory. Locale JSON must
also pass syntax and nesting validation before locale analysis.
Portable MCP reads saved files. Its resource index requires explicit refresh
after edits, even though diagnostic/reference source reads can see newer disk
contents.
