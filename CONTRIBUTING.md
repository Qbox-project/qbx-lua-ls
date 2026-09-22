# Contributing

## Set up

Install stable Rust. Keep this checkout beside
[qbx-lint](https://github.com/Qbox-project/qbx-lint), which provides the parser, formatter,
analysis and FiveM data crates through local path dependencies:

```text
work/
  qbx-lint/
  qbx-lua-ls/
```

From `qbx-lua-ls`, build with `cargo build --locked`. If a change needs shared parser or lint
behavior, make the corresponding change in `qbx-lint` and mention both changes in the pull
request. Keep the path dependencies intact.

## Check changes

Run these commands from this repository before submitting a pull request:

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

Use `cargo fmt` to apply formatting. Add a regression test when fixing a bug, using a small Lua
example and the manifest needed to reproduce it. The tests in `tests/lsp.rs` run the server
through an in-memory LSP connection. `tests/workspace_refresh.rs` covers event-side behavior
and index refreshes with temporary files.

Check both minimal LSP clients and clients that advertise the relevant capability when changing
protocol behavior. Editor integration code belongs in
[qbx-editor](https://github.com/Qbox-project/qbx-editor).

For manual inspection, build the executable and use the probe script:

```sh
cargo build --release --locked
node scripts/probe.mjs <workspace> <virtual-file> <server-executable> < snippet.lua
```

`probe.mjs` requires Node.js. It opens the supplied text as a document and prints completions
for lines marked with `--^` and hovers for `--?<column>` markers. Use the `.exe` executable on
Windows. Run the command in a shell that supports input redirection.

## Code layout

| Location | Purpose |
| --- | --- |
| `src/types.rs`, `src/luacats.rs` | Type representation and LuaCATS annotations. |
| `src/indexer.rs`, `src/index.rs` | File summaries, symbols and visibility between resources. |
| `src/infer.rs` | Type inference used by editor features. |
| `src/workspace.rs` | Resource discovery, manifests, dependencies and index refreshes. |
| `src/features/` | LSP feature implementations. |
| `src/server.rs` | Protocol dispatch, settings and diagnostics publication. |

Closed files retain summaries rather than full syntax trees. Reference searches may parse them
again. Keep that distinction in mind when adding data to the index or handling unsaved text.

## Issues and pull requests

For a bug report, include the server version, operating system, editor/LSP client, relevant
settings, expected behavior and a small Lua/manifest example. For crashes, include server logs
and the last action that triggered the problem.

Describe the behavior changed by a pull request and list the checks you ran. State any editor
or platform behavior you could not test. Update the docs and changelog when the user-facing
behavior changes.
