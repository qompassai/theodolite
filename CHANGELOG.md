# Changelog

## 1.0.3 - 2026-11-26

- Patch: add the MCP Registry ownership marker (`mcp-name: ai.qompass/theodolite`) to the README as shipped in the crate package; the 1.0.2 crates.io README lacked it and the Registry rejected the publish. No functional changes.


## 1.0.2 - 2026-11-26

- **GDShader route: legacy `rootPath` opt-in.** gdshader-lsp
  0.1.0 reads its project root only from the deprecated
  `rootPath` initialize param (its `main.rs`) and loads its
  builtin functions only when a root exists; the bridge sent
  the modern shape (`rootUri` / `workspaceFolders`) only, so
  through the bridge every builtin call false-positived
  ("Function 'vec3' does not exist") and the route could not
  be wired (SHADER-ADDENDUM item 4). New per-server stanza
  opt-in `send_root_path` (default off): for an opted-in
  server only, `initialize` also carries `rootPath`, the
  filesystem path behind `rootUri`. Stanzas without the
  opt-in are unchanged — no deprecated field is sent to a
  server that did not ask for it. Gates: config parse
  (opt-in loads, defaults off, non-bool refused), fake-lsp
  `rootpath` mode echoing the received field through
  documentSymbol (opt-in lands verbatim, default absent),
  and the live gdshader gate through the bridge in one
  session — bad 1 ("Expected Semicolon"), good 0 with the
  positive control live in the same session, so the zero is
  verified — with glsld (1 / 0) and Lua (bad 6 / good 0,
  sacrificial first query) regression reads unchanged.

## 1.0.1 — 2026-11-16

- **Nil-route clean-file fix.** The diagnostics settle wait
  required protocol evidence (a publish, a progress completion,
  or a quiescent report) postdating the last document sync —
  but nil publishes `publishDiagnostics` only for non-empty
  sets, so a clean .nix file produced no evidence, ever, and
  the call held to the ready deadline and failed
  `ServerNotReady` instead of returning the empty set. New
  per-server stanza opt-in `no_publish_means_empty_ms`
  (validated 250..=120000, and required to sit below
  `ready_timeout_ms`): for an opted-in server only, a full
  window of silence after the last content sync — nothing in
  flight, diagnostics generation stable across two polls —
  settles the wait with the accumulated (empty) set. The
  window is measured from the last content sync
  (didOpen/didChange/didClose); the bridge's own didSave
  re-assertions do not restart it. Servers without the opt-in
  are unchanged: silence still fails `ServerNotReady`, the
  Rust route's evidence rule is untouched, and a publish
  arriving inside the window still wins.

## 1.0.0 — 2026-11-09

The coverage release. Theodolite's claim is now an in-tree
record (COMPLIANCE.md) measured against the coverage SPEC's
five-part test, and the two bridge behaviours that record
exposed are fixed in the bridge itself.

- **Slice 0 readiness marker.** A read-class result served
  before a server has ever reported quiescence now carries
  `possibly_incomplete: "server_not_quiescent"` — a pre-load
  answer is never indistinguishable from a proven one (the
  Love Dolls failure shape). Applies to diagnostics, hover,
  location results, document/workspace symbols, and code
  actions under immediate readiness.
- **Symbol-result completeness.** `document_symbols` and
  `workspace_symbols` enforce a stated 1 MiB budget
  (`budget_bytes` in every envelope) instead of the general
  64 KiB result budget that made real outlines (a 369-symbol
  Lua file in the estate's own config) unreturnable. Overflow
  past the bound is the typed `ResultTooLarge` carrying the
  true count — results are complete or explicitly bounded,
  never silently truncated.
- **COMPLIANCE.md** (new): per-language coverage verdicts with
  exact server pins, transcripts, and licenses. Covered:
  Rust, Lua, TOML, Python, TypeScript, C#, WGSL, GDScript.
  Partial with named causes: Kotlin (androidTest classpath),
  D (serve-d diagnostics channel), RON (server method
  surface), Ren'Py (evaluation slice; standalone server
  spot-checked, not adopted).
- `bridge.toml.example` documents the Slice 0 rule: pair
  quiescent readiness with a cold-sized ready timeout for
  workspace-loading servers; the marker covers the rest.
- Test suite: 100 tests (suite counts per module in the
  release record), strict clippy clean.

## 0.2.2 — 2026-11

- Answer a modern-version initialize at the era guard.

## 0.2.1 — 2026-11

- Registry server.json for ai.qompass/theodolite; description
  trimmed to the registry limit.
- didSave for save-advertising servers, re-asserted while
  diagnostics evidence is outstanding (D-408).

## 0.2.0 — 2026-10

- rmcp migration: modern protocol era only (2026-07-28);
  dual-era/legacy support dropped (estate D-113).
