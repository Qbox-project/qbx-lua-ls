# Changelog

## 1.0.2

- Keep event and callback completions anchored to the whole name when typing `:`.
- Replace the existing event name when accepting a suggestion, preserving its quotes and arguments.
- Update the shared parser and analysis crates to version 1.0.2.

## 1.0.1

- Update the shared parser and analysis crates to version 1.0.1.

## 1.0.0

- Add FiveM Lua language support over stdio LSP: completion, hover, definitions, references,
  rename, diagnostics, quick fixes, formatting, signature help, inlay hints, semantic tokens,
  folding and symbols.
- Resolve resource imports and client/server scripts from `fxmanifest.lua`, with LuaCATS types,
  native signatures and event/callback handler parameters available to editor features.
- Include static string keys and supported annotation fields in member rename. Refuse edits
  when a known declaration or an unreadable file prevents a safe rename.
- Respect client/server guards when selecting event diagnostics, completions and signatures.
- Rebuild files, manifests and dependencies during manual reindex, preserving unsaved documents
  and removing stale disk entries.
- Limit long expression chains and avoid offering a hash replacement that would create an
  invalid Lua statement.
- Honor client capabilities for snippet completions and dynamic file-watch registration.
- Configure standalone release archives for Windows x64, Linux x64/ARM64 and macOS x64/ARM64.
  Editor setup is documented in [qbx-editor](https://github.com/Qbox-project/qbx-editor).
