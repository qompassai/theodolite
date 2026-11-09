# Changelog

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
