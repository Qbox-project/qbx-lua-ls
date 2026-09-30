# qbx-lua-ls has merged into qbx-lint

**The FiveM Lua language server is now part of
[Qbox-project/qbx-lint](https://github.com/Qbox-project/qbx-lint).** Its source, tests, scripts
and documentation live in
[`crates/qbx_lua_ls`](https://github.com/Qbox-project/qbx-lint/tree/main/crates/qbx_lua_ls),
alongside the command-line linter, formatter and shared analysis crates.

This repository is archived. Use the combined repository for new language-server issues,
pull requests, builds and downloads. Shared analysis and server changes now fit in one
checkout and one pull request.

## Where to go

| Task | Current location |
| --- | --- |
| Learn about both tools | [Qbox Lua tooling README](https://github.com/Qbox-project/qbx-lint#readme) |
| Read language-server features and configuration | [qbx-lua-ls guide](https://github.com/Qbox-project/qbx-lint/blob/main/crates/qbx_lua_ls/README.md) |
| Download the server or CLI | [Shared tooling releases](https://github.com/Qbox-project/qbx-lint/releases) |
| Report a server bug or request a feature | [qbx-lint issues](https://github.com/Qbox-project/qbx-lint/issues) |
| Contribute to the server or shared Rust crates | [Workspace contribution guide](https://github.com/Qbox-project/qbx-lint/blob/main/CONTRIBUTING.md) |
| Set up an editor | [qbx-editor setup guide](https://github.com/Qbox-project/qbx-editor/blob/main/docs/editors.md) |
| Read LSP settings and custom requests | [Protocol reference](https://github.com/Qbox-project/qbx-lint/blob/main/crates/qbx_lua_ls/docs/protocol.md) |

## Install or build the current server

From **v1.0.5**, [qbx-lint releases](https://github.com/Qbox-project/qbx-lint/releases) contain
both `qbx-lint-<target>` and `qbx-lua-ls-<target>` archives. Choose `qbx-lua-ls-<target>` for
your editor's language server. The executable is still named `qbx-lua-ls` (`qbx-lua-ls.exe`
on Windows) and still uses LSP over standard input and output.

For VS Code, the [Qbox Lua extension](https://marketplace.visualstudio.com/items?itemName=Qbox.qbx-lua)
includes the server. Other editors can use a downloaded binary or a source build.

To build with stable Rust, clone the combined workspace:

```sh
git clone https://github.com/Qbox-project/qbx-lint.git
cd qbx-lint
cargo build --release --locked -p qbx_lua_ls
```

The executable is `target/release/qbx-lua-ls` (add `.exe` on Windows). To install it with Cargo,
run `cargo install --path crates/qbx_lua_ls --locked` from the same checkout. A separate
`qbx-lua-ls` checkout is no longer needed.

## Preserved history and older releases

The server's original commit graph was imported without squashing or rewriting commits.
Original commit hashes, authors, timestamps and messages are preserved in the combined
repository. Historical server tags are available there as `lua-ls/v1.0.0` through
`lua-ls/v1.0.4`, alongside the linter's existing tags.

This archived repository retains its original history and
[standalone releases through v1.0.4](https://github.com/Qbox-project/qbx-lua-ls/releases)
for older installations and download links. See the
[migration notes](https://github.com/Qbox-project/qbx-lint/blob/main/docs/repository-migration.md)
for the completed migration and compatibility details.

## License

[GPL-3.0-or-later](LICENSE).
