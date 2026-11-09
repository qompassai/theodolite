# theodolite — compliance record

Bridge coverage measured against the five-part COVERED test of
the coverage SPEC (the estate's theodolite coverage spec, §1):

1. **Routed in the canonical config** — a `[[server]]` entry in
   the canonical `bridge.toml`, binary pinned by absolute path,
   `expect_version` matched by the bridge's startup probe on the
   named machine.
2. **Real session** — a recorded transcript against that server
   on a real tree shows `session_status` ok and the server
   answering through the bridge.
3. **Non-vacuous diagnostics** — a real finding on the real
   tree, or a seeded-defect probe the server flags through the
   bridge. An empty set from an unloaded workspace does not
   count.
4. **Non-empty semantic answer, spot-verified** — at least one
   of `document_symbols` / `hover` / `definition` returns a
   non-empty result for a file independently known to contain
   the symbol, and the returned symbol is the known one.
5. **Recorded** — this file: language, server, exact probed
   version, machine, tree, date, transcript, and the server's
   license (Arch package `License:` field verbatim where the
   server is an Arch package; crates.io metadata otherwise).

A language failing any part is PARTIAL with the mechanism
named, never "covered".

## Measurement context

- Date: 2026-11-09. Machine: primo (Linux 7.2.11-zen1-1-zen
  x86_64).
- Canonical config: primo `~/.local/share/theodolite/bridge.toml`,
  SHA-256 `1d7a848ed821b9916589f4f68945b7cc7e65e48a6e72fd16e9ba63c32dfd714b`
  (12 server stanzas).
- Bridge under test: the 1.0 build on branch
  `pax/theodolite-1.0-20261109` (binary SHA-256 recorded in the
  release record). All sessions ran through the bridge (MCP),
  never the server binary standalone.
- Transcripts: the lane archive
  `~/workspace/theodolite-1.0-20261109/transcripts/` on the
  recording machine; file names cited per row.
- Readiness hardening shipped in this release (Slice 0): a
  read-class result served before a server has ever reported
  quiescence carries `possibly_incomplete:
  "server_not_quiescent"`; symbol results state their
  `budget_bytes` bound (1 MiB) and are complete or a typed
  refusal with the true count.

## Coverage

### Rust — COVERED (paired servers)

- Diagnostics owner: bacon-ls, probed `0.23.0` (cargo-installed;
  crates.io license field: non-standard). Semantics:
  rust-analyzer, probed `rust-analyzer 1.100.0-nightly
  (f7575a9 2026-09-24)`, readiness quiescent (Arch package
  rust-analyzer 20260907-1, License: MIT OR Apache-2.0).
- Tree: the theodolite worktree itself. Transcripts:
  `b1-rust-quiescent.jsonl` (session.rs 141 symbols, tools.rs
  66, diagnostics 0 — true on the gated tree),
  `b3-rust-bacon.jsonl` (diagnostics source = bacon-ls, 0;
  symbols via bacon-ls are a typed -32601 — the pairing is by
  design), `b1-rust-symbolsfirst-cold.jsonl` (cold immediate
  session: pre-load answer served marked
  `server_not_quiescent`, never bare).
- Per-machine caveat: on primo, bacon-ls owns Rust diagnostics
  and rust-analyzer owns semantics. A machine without bacon-ls
  substitutes rust-analyzer for both roles; the pairing is a
  config fact, recorded here, not a code path.

### Lua — COVERED

- lua-language-server, probed `3.19.2`, readiness immediate
  (Arch package lua-language-server 3.19.2-1, License: MIT).
- Tree: the Diver Neovim config. Transcripts: `b3-lua.jsonl`
  (client.lua diagnostics 0 true; document_symbols = 369
  returned whole with the stated budget — the outline the
  pre-1.0 bridge refused; init.lua 0 + 11 symbols, same
  session), `b3-lua-seeded.jsonl` (planted syntax error flagged
  severity-1 at the planted line).

### TOML — COVERED

- taplo, probed `taplo 0.10.0`, readiness immediate (Arch
  package taplo-cli 0.10.0-2, License: MIT). Wrapper stanza
  (taplo's LSP takes argv the bare binary rejects).
- Tree: the theodolite worktree. Transcripts: `b3-toml.jsonl`
  (Cargo.toml diagnostics 0 true; symbols 37, first `package`),
  `b3-toml-seeded.jsonl` (planted unclosed bracket flagged).
- Note: taplo reports no quiescence evidence, so its symbol
  results carry the Slice 0 marker by design; the §1.4
  spot-check passes on content.

### Python — COVERED

- jedi-language-server, probed `0.47.0`, readiness immediate
  (Arch package jedi-language-server 0.47.0-1, License: MIT).
- Tree: the phlow repository. Transcripts: `b3-python.jsonl`
  (flow/__main__.py diagnostics 0, symbols [`main`];
  flow/__init__.py 0 + 2 symbols, same session),
  `b3-python-seeded.jsonl` (planted syntax error flagged).

### TypeScript — COVERED

- typescript-language-server, probed `6.0.0`, readiness
  immediate (Arch package typescript-language-server 6.0.0-1,
  License: Apache-2.0  MIT). Wrapper stanza (argv).
- Tree: scratch seeded file (the wiring-pass shape, re-recorded
  on the 1.0 build). Transcript: `b3-typescript.jsonl` —
  diagnostics catch the planted type error as code 2322
  severity-1; symbols [`greet`, `n`]; same file twice in one
  session.

### C# — COVERED (session discipline applies)

- csharp-language-server, probed `csharp-language-server
  0.7.1` (Roslyn payload 5.4.0-2.26060.1; Arch package
  csharp-language-server 5.4.0_2.26060.1-1, License: MIT),
  readiness first-response. User-level .NET root (SDK 8.0.427 +
  runtime 10.0.14) is the stanza's environment.
- Tree: Strobe Core (`Assets/Scripts/Core`). Transcripts:
  `b3-csharp.jsonl` (HeroineState.cs 23 symbols incl. the
  `HeroineState` type; Session.cs 58 incl. `SessionAction`;
  real analyzer findings IDE0005/IDE0300/IDE0060/CA1510 on the
  real sources), `b3-csharp-seeded4.jsonl` (planted type error
  in HeroineState.cs flagged CS0029 severity-1).
- Session discipline (measured, B3): semantic diagnostics need
  (a) a .csproj at the workspace root — without one the server
  runs in misc-files mode and serves analyzer hints only — and
  (b) a diagnostics call after project load completes; the
  first call can race the load.

### Kotlin — PARTIAL (androidTest classpath unwired)

- kotlin-language-server, probed `kotlin-language-server
  1.3.13`, readiness first-response, JDK 17 wrapper (Arch
  package kotlin-language-server 1.3.13-1, License: MIT).
- Tree: LightShow Android project; its only Kotlin file is the
  androidTest `BoardInteractionInstrumentedTest.kt`.
  Transcripts: `b4-kotlin.jsonl`, `b4-kotlin-poll.jsonl`.
- Holds: §1.1, §1.2, §1.4 (document_symbols = 35 — the
  instrumented-test class and its members, spot-verified, two
  sessions), §1.5. Diagnostics are non-vacuous (real findings:
  unreachable code, unused variable).
- Fails the B4 gate on §1.3's quality bar: with the SDK in the
  stanza env (ANDROID_HOME) and a 6-minute in-session import
  poll, the server's Gradle import does not wire the
  androidTest source set's dependency classpath — the
  unmodified real file carries 28 unresolved references, all
  androidx.test / JUnit / UiAutomator imports, counted in
  B4-RECORD.md. Session discipline notes (SDK env, import
  polling, Gradle daemon cleanup after sessions) live in the
  same record.

### D — PARTIAL (diagnostics channel silent)

- serve-d, probed by its five-line version string beginning
  `serve-d v0.7.6 with workspace-d v3.7.0`, readiness
  first-response, `diagnostics_owner = true` (Arch package
  serve-d 0.7.6-1, License: MIT).
- Tree: a fresh dependency-free dub project made for the
  measurement, plus the wiring pass's probe project.
  Transcripts: `b5-d-cold.jsonl`, `b5-d-warm.jsonl`,
  `b5-d-dprobe.jsonl`, `b5-d-twocall.jsonl`.
- Holds: §1.1, §1.2, §1.4 (document_symbols instant and
  spot-verified: `add`, `Greeter`, `name`, `greet`), §1.5.
- Fails §1.3: serve-d emits no diagnostics evidence through
  the bridge's open-and-wait flow — cold, warm, or repeated
  calls, >600 s of in-session waiting across four sessions;
  the bridge answers the typed ServerNotReady refusal every
  time (never a bare empty). `dub describe` answers in
  0.025 s, so the stall is inside serve-d's analysis
  pipeline, not dub. The canonical 300 s ready window stays;
  no timeout sizing on this evidence fixes the channel.

### WGSL — COVERED (semantic route: definition)

- wgsl-analyzer, probed `wgsl-analyzer 0.0.0` (Arch package
  wgsl-analyzer 20260930-2, License: MIT OR Apache-2.0),
  readiness immediate.
- Trees: LightShow shaders + scratch shaders. Transcripts:
  `b6-wgsl-bevy.jsonl` (real Bevy shader: 6 diagnostics, all
  the Bevy-preprocessor class — raw Bevy shaders are not plain
  WGSL), `b6-wgsl-plain.jsonl` (valid shader: 0, true),
  `b6-wgsl-seeded.jsonl` (planted error: 4 severity-1
  diagnostics), `b6-wgsl-hover3.jsonl` (definition of a
  variable at its use site returns its declaration exactly).
- Method surface, recorded: `document_symbols` is a typed
  -32601 (not implemented server-side) and `hover` returns
  null at every probed position; §1.4 is met by `definition`,
  which the SPEC's disjunction allows.

### RON — PARTIAL (no semantic method; diagnostics inert)

- ron-lsp, probed `ron-lsp 0.1.4`, readiness immediate
  (cargo-installed; crates.io license: MIT).
- Tree: Lane D remake data (Magic Coin `coin-dynamics.ron`).
  Transcripts: `b6-ron.jsonl`, `b6-ron-seeded.jsonl`,
  `b6-ron-seeded2.jsonl`, `b6-ron-seeded3.jsonl`.
- Holds: §1.1, §1.2, §1.5. Fails §1.4 (all three semantic
  methods absent: document_symbols -32601 "Method not found",
  hover null, definition empty) and §1.3 cannot be evidenced:
  seeded syntax breaks and even a garbage file each drew 0
  diagnostics, so the channel serves nothing distinguishable
  from a true negative. (Hypothesis, recorded as such: ron-lsp
  0.1.4 may implement pull diagnostics, which the bridge does
  not issue.)

### GDScript — COVERED (editor-bridge lifecycle)

- Server: the Godot editor's embedded language server, probed
  `4.7.2.stable.arch_linux.ed1daf0bf` (Arch package godot
  4.7.2-2, License: MIT), reached through the socat shim stanza
  (TCP 6005). No standalone GDScript server exists.
- Tree: a scratch Godot project carrying a controlled script
  plus a real Modification App script. Transcripts:
  `b7-gd-editor.jsonl` (probe script: diagnostics 0, symbols
  incl. `_ready` and `compute_total`; real file
  DebugTestTransitions.gd: 0 + 7 symbols incl. `DoIt`),
  `b7-gd-noeditor.jsonl` (no editor listening: typed ServerDied
  refusal in 0.1 s — never a hang).
- Lifecycle is part of the coverage claim: a session requires
  a running Godot editor for the project. A TCP-native stanza
  (no socat shim) is the first post-1.0 slice.

### Ren'Py (.rpy) — PARTIAL (evaluation slice; not adopted)

- No stanza ships in the canonical config for 1.0. The
  official VS Code extension (`renpy.language-renpy` v2.4.0,
  VSIX SHA-256
  `6fedf05bbef1298471c92aa02bbca93981d1a0b584722cbcb98df725fc6fcf7c`)
  contains no language server — it implements features
  in-process via editor provider APIs.
- A standalone server exists and was spot-checked through the
  bridge in an evaluation stanza: `rockettap/renpy-language-server`
  v0.2.1 (Python/pygls, unofficial). Transcripts:
  `b8-renpy-spotcheck4.jsonl` (real script.rpy: diagnostics 0,
  document_symbols 111 with all 11 grep-verified labels
  present), `b8-renpy-seeded.jsonl` (planted unknown-label jump
  flagged). Sizing caveat: the server scans the workspace
  inside initialize — scope the workspace to the script tree.
- Not adopted for 1.0: the server is unofficial and brand-new
  upstream; pinning it as estate infrastructure is an Owner
  adoption call (staged in the release record). The `renpy
  lint` comparison could not run — the distro Ren'Py install
  dies writing into its own lib dir (PermissionError) before
  linting.

## Tally

COVERED: Rust, Lua, TOML, Python, TypeScript, C#, WGSL,
GDScript (8). PARTIAL with named causes: Kotlin, D, RON,
Ren'Py (4). No language is silently absent: every language in
the canonical config has a line above, and Ren'Py — the one
estate language without a stanza — has its evaluation verdict
recorded.
