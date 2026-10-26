# theodolite v1 — BUILD REPORT

Built 2026-10-26 (sandbox), staged per the SPEC at
`~/workspace/lsp-mcp-study-20261025/SPEC.md` (IDEA-0092; Owner build
approval 2026-10-26). Scope as commissioned: **read-only v1** — the
mutation class is unimplemented and unregistered, per SPEC §12
question 2's smaller-safer option, adopted by the commissioning
brief.

## What was built

A single Rust crate (edition 2024, `#![forbid(unsafe_code)]`,
toolchain pinned to `nightly-2026-09-25` in `rust-toolchain.toml`)
with two binaries:

- **`theodolite`** — the MCP server (stdio, hand-rolled NDJSON
  JSON-RPC, 2 MiB frame cap, following pratique's estate shape).
- **`theodolite-fake-lsp`** — a scripted LSP test double (SPEC §9.4)
  used by the gate suite; not a product binary.

Internal layout follows SPEC §2's split as modules of one crate
(builder's call, allowed by §2): `lsp` (bridge-protocol: bounded
framing + typed LSP subset), `session` (bridge-session: child
supervision, document sync, readiness, diagnostics accumulation),
`tools` (bridge-tools: tool definitions + shaping), `shaping`
(canonical ordering + byte budgets), `config` (§5 data model),
`errors` (§4.6 taxonomy), `mcp` (transport). `bridge-gate` has no
module because v1 has no mutation class; the `GateDenied` code
exists in the taxonomy from birth.

### Tool list (complete v1 surface)

| Tool | Class | Notes |
|---|---|---|
| `session_status` | session | §4.2 transcript: server, binary, actual + expected version, workspace, bridge version, toolchain pin, config hash, phase, pid/liveness, open documents, protocol violations, stderr tail when dead |
| `diagnostics` | read | Push accumulation + settle predicate; optional per-file scope; owner metadata per §7.2 |
| `hover` | read | Markup preserved verbatim; range normalized |
| `definition` | read | Sorted location list (Location and LocationLink shapes) |
| `references` | read | `include_declaration` explicit, default true |
| `document_symbols` | read | Flattened (hierarchical + flat shapes), container names, sorted by range |
| `session_restart` | session | The only respawn path |

## Architecture vs SPEC, section by section

### §1 Placement — DONE
Own MCP server crate, standalone; no phlow dependency, no periscope
coupling, no Neovim anywhere. Graduation reasoning is the SPEC's
own; nothing in the build alters it.

### §2 Shape — DONE with one judgment call
Rust latest (nightly-2026-09-25, rustc 1.100.0-nightly), Tiger Style
Rust, hand-rolled bounded NDJSON MCP. Judgment call: single crate
with modules instead of a four-crate workspace (§2 explicitly
leaves this to the builder); module names mirror the §2 split.

### §3 Process model
- §3.1 Spawn discipline — **DONE**: absolute path from config, own
  process group (`process_group(0)`), `setpriv --pdeathsig SIGKILL`
  wrapper, scrubbed environment (fixed PATH + HOME + locale + the
  entry's declared env only).
- §3.2 Session identity/bound — **DONE**: keyed `(workspace,
  server)`, `MAX_SESSIONS = 4`, fifth key is a loud `SessionLimit`
  error (test-proven, no eviction).
- §3.3 Lazy start, explicit readiness — **DONE**: initialize
  handshake + `initialized`, readiness predicates `immediate` /
  `quiescent`; calls wait on state up to `ready_timeout_ms`
  (default 30 000) then fail `ServerNotReady`. No fixed sleep is a
  readiness mechanism anywhere. **Hardening beyond the SPEC text:**
  quiescence additionally requires *observed* load activity
  (a `$/progress` begin or a non-quiescent `serverStatus`) to have
  drained — quiescence before any activity is a server that has not
  started loading, and the naive predicate declared a cold
  rust-analyzer "ready" while its hover answers were still null.
  This is the isaacphi sleep-1s failure in a subtler costume; the
  fix was verified against the real server (see Gates).
- §3.4 Crash handling — **DONE**: reaper thread (waitpid via
  `try_wait` on a dedicated thread), dead sessions stay dead with
  the root-cause detail, next call gets `ServerDied`, only
  `session_restart` respawns (all test-proven).
- §3.5 Document sync — **DONE**: bridge-owned open table,
  stat+size+SHA-256 re-sync at the start of every file-naming call,
  full-text `didChange` with monotonic versions, `didClose` on
  delete, language ids from the config entry's map (never
  hardcoded — the jonrad failure). No filesystem watcher.
- §3.6 Shutdown — **PARTIAL**: on transport EOF the bridge stops
  accepting calls, sends LSP `shutdown` + `exit` per session with
  the named grace (default 2 000 ms), then kills survivors and the
  reaper reaps them. The one deviation: survivors get SIGKILL to
  the child *leader* rather than the whole process group, because
  `killpg` needs libc FFI and the crate is `#![forbid(unsafe_code)]`
  (the commissioning brief's constraint). The orphan-critical half
  — bridge SIGKILL ⇒ children die — is fully enforced by pdeathsig
  and test-proven; language servers that spawn detached
  grandchildren of their own are the only residue the deviation
  could leave, and none of the three v1 servers does so in normal
  operation. SIGINT/SIGTERM handlers are **DEFERRED**: transport
  EOF is the MCP lifecycle's normal close and is handled; signal
  handling without unsafe signal-hook code needs a self-pipe or a
  signal thread design that belongs with the primo hardening pass.
- §3.7 Resource bounds — **DONE**: `MAX_FRAME_BYTES = 16 MiB`
  enforced before allocation (oversized/negative/garbage/truncated
  all test-proven), per-request timeouts (typed `ServerTimeout`,
  test-proven against a never-answering server), stderr drained
  into a 256 KiB ring attached to `session_status` when dead.

### §4 Determinism contract
- §4.1 Pinned servers — **DONE**: absolute path + exact version
  probe at session start; mismatch is `ServerVersionMismatch`
  before any session exists (test-proven). Field note: pin the
  toolchain's *real* rust-analyzer binary, never a rustup proxy —
  proxies depend on ambient env (RUSTUP_HOME) that §3.1 scrubs, so
  a proxy fails its probe loudly under the bridge (observed in the
  sandbox; the config example carries the warning).
- §4.2 Session transcript — **DONE** (see `session_status` above;
  start time is monotonic ms since bridge start, no wall clock).
- §4.3 Canonical ordering — **DONE**: locations, diagnostics, and
  symbols sorted by the SPEC's exact keys before shaping; server
  order never reaches the client (fake server returns deliberately
  scrambled order; gates assert the sorted result).
- §4.4 Byte-bounded results — **DONE**: `MAX_RESULT_BYTES = 65 536`;
  overflow is `ResultTooLarge` carrying the true item count and the
  budget, zero partial payload (test-proven with 1 500 references).
  No pagination cursors.
- §4.5 No wall-clock in result content — **DONE** for tool results;
  the byte-identical double-run gate passes (transcript metadata in
  `session_status` is the SPEC's own exemption).
- §4.6 Stable errors — **DONE**: typed enum, stable codes, server
  JSON-RPC errors preserved verbatim as `ServerError` with the
  server's code. One principled refinement: a caller in flight when
  a session dies receives the specific death error (e.g.
  `MalformedServerTraffic`); a caller arriving after the death is
  recorded receives `ServerDied` with the same detail string —
  identical *conditions* render identically; the conditions differ.

### §5 Server configuration — DONE with one judgment call
TOML, refuse-to-start on any violation with typed `ConfigInvalid`
(unknown fields denied; duplicate ids, relative binary paths,
empty probes, unknown readiness, and any `mutation != "off"` all
test-proven refusals). Routing by explicit `server` argument or by
extension with ambiguity/absence as errors — no default server.
Judgment call: the SPEC's illustrative parallel `languages` /
`extensions` arrays cannot express which language id belongs to
which extension, so the implementation adds one explicit map,
`language_ids` (extension → language id), as the didOpen authority;
positional pairing of the two arrays is accepted when the map is
absent and lengths are equal. `initializationOptions` and
`settings` are config data, passed through / answered verbatim and
covered by the config hash in every transcript.
`workspace/configuration` requests with no declared settings are
answered with nulls and logged as a transcript violation rather
than isaacphi's silent `[{}]`.

### §6 Tool surface — DONE for the commissioned read-only scope
Implemented: `session_status`, `diagnostics`, `hover`,
`definition`, `references`, `document_symbols`, `session_restart`.
- **DEFERRED (beyond the commissioned v1 scope, not silently
  dropped):** `workspace_symbols` and `code_actions` (read-class in
  the SPEC's table; the commissioning brief enumerated the v1 tool
  set without them), and the definition family as separate tools
  (`declaration`, `typeDefinition`, `implementation` — `definition`
  itself is implemented). Adding any of them is a shaping review,
  not new machinery.
- **DEFERRED by design:** pull diagnostics (`textDocument/
  diagnostic`) where a server offers it; v1 diagnostics are the
  push accumulation plus the settle predicate. The parity gate's
  counterparty (bacon-ls) is a push server, so the gate is
  unaffected.
- Mutation class — **NOT IMPLEMENTED, NOT REGISTERED** (brief's
  ruling): `tools/list` is exactly the read set (test-proven), and
  calls to `code_action_apply` / `rename_preview` / `rename_symbol`
  / `edit_file` return `UnsupportedMethod`. Rename apply remains
  out, per the SPEC's own plain statement.

### §7 Server set — DONE as config + gates
All three servers are config entries (`bridge.toml.example` ships
all three shapes). bacon-ls carries `diagnostics_owner = true`;
`diagnostics` against a non-owner server returns that server's own
push set plus metadata naming the owner (§7.2's "say so, don't
substitute" rule). rust-analyzer uses `quiescent`; bacon-ls and
lua-language-server use `immediate`. In the sandbox, rust-analyzer
was exercised for real (see Gates); bacon-ls and lua-language-server
binaries do not exist here, so their live exercise is part of the
primo mirror step.

### §8 Non-goals — DONE
No editing anywhere; server-pushed `workspace/applyEdit` is refused
with a typed error response and logged as a transcript violation
(test-proven — the fake server logs the refusal); no
`workspace/executeCommand`, no generic method passthrough (unknown
server→client methods get a JSON-RPC -32601 and a violation
record); no Neovim, no Diver config reads, no watcher, no work
between calls besides reaping and notification accumulation, stdio
only, no result caching beyond live session state, no telemetry,
no file contents in any log or transcript.

### §9 Gates — see the gate table below.

### §10 Non-overlap charter — DONE
Nothing in the build touches an editor, a committed-tree index, or
a batch check runner.

### §11 Register/name — DONE before the build
Name `theodolite`, IDEA-0092 (Owner, 2026-10-25); the crate
carries the name and the registry line in its metadata intent.
Publishing is a separate step, not part of this build.

### §12 Open questions — disposition
1. Name/ID: ruled (theodolite, IDEA-0092).
2. Mutation posture: the commissioning brief ruled read-only v1 —
   implemented exactly so.
3. `MAX_SESSIONS = 4`: fixed named constant, as the brief's
   position proposed.
4. Settle predicate: shipped as specified (two-poll stability,
   250 ms apart, plus no in-flight progress), named in transcripts
   only via behavior; revisit on primo evidence.

## Gates — exact numbers

Toolchain: nightly-2026-09-25 (rustc 1.100.0-nightly), sandbox.
`RUSTC_WRAPPER=` was used for clippy (the sccache wrapper breaks
clippy-driver in this environment, per estate record).

| Gate | Result |
|---|---|
| `cargo build` (debug + release) | PASS |
| `cargo test` | **31 passed / 0 failed** — 8 unit + 22 stdio + 1 parity (vacuous, see below) |
| `cargo clippy --all-targets -- -D warnings` | PASS, zero warnings |
| `cargo fmt --check` | PASS |

Test split: **15 validation / 16 adversarial** (the estate's
half/half rule, ±1 on an odd total).

Validation (15): config loads + positional pairing; framing
roundtrip (all three message shapes); shaping sort + dedup; URI
codec + relativize; version pin accepted (recorded in transcript,
classified as §4.1 validation); hover verbatim; definition sorted;
references sorted; document symbols flattened + sorted;
diagnostics settle + sort; session transcript fields; double-run
byte-identical (SPEC §9 gate 3); session restart fresh transcript;
real rust-analyzer smoke (hover + definition on a fixture crate,
quiescent readiness — **sandbox note:** cold rust-analyzer needed
~35 s to first readiness here, so the smoke's config declares
`ready_timeout_ms = 180000`; primo numbers should re-baseline the
default); parity gate (SKIPPED here, counted as validation-class).

Adversarial (16): config violations typed (relative path,
duplicate id, mutation posture, bad readiness); framing rejects
oversized/negative/garbage before allocation; budget overflow
typed with true count; path escapes refused in all forms (unit);
oversized frame end-to-end; garbage frame kills session; truncated
frame kills session; unknown response id kills session; crash
mid-request → `ServerDied`, no silent respawn, restart recovers;
never-answering server → `ServerTimeout` inside its bound;
oversized references → `ResultTooLarge` (1 500 items, zero partial
payload); workspace-escape tool arguments (`../`, absolute
outsider, encoded traversal); mutation tools absent from
`tools/list` and refused by name; server-pushed `workspace/
applyEdit` refused (-32001) and logged; fifth session key →
`SessionLimit`; orphan gate (below).

### The parity gate — SKIPPED here, harness real

`tests/parity.rs` contains the full gate: it drives bacon-ls
standalone over a fixture crate (initialize, didOpen, settle),
drives the bridge's `diagnostics` over the same fixture at the same
probed version, canonicalizes both sides through the crate's own
shaping, and asserts byte equality. In the sandbox **no bacon-ls
binary exists** (`which bacon-ls`, `~/.cargo/bin`, and
`$THEODOLITE_BACON_LS` all absent), so the test prints its SKIP
reason and passes vacuously — 0.02 s runtime confirms it did not
run a comparison. It is recorded here as **SKIPPED, not passed**.

### The orphan gate — PASSED here

SPEC §9 gate 10 is a release blocker and it runs in the sandbox:
the suite starts the bridge, opens a session on the fake server,
records the fake's pid, SIGKILLs the bridge, and asserts the fake's
`/proc` entry is gone within 5 s. It passed (pdeathsig discipline
via `setpriv`, the periscope `0ec568e` precedent).

### Bugs found by the gates and fixed during the build

1. **Pending-registration race:** a request registered after the
   reader thread's fail-all drain but before the death was visible
   could hang until its deadline. Requests now wait in slices and
   re-check the phase; write failures racing a death resolve as
   `ServerDied` with the root-cause detail.
2. **Naive quiescence:** see §3.3 — readiness now requires observed
   activity to have drained.
3. **Initialize-time death:** a child dying immediately after
   answering `initialize` (the truncated/garbage fixtures) aborted
   session recording via the `initialized` write; the session is
   now recorded and its death governs later calls, so transcripts
   and `session_restart` work for stillborn sessions too.

## Primo mirror + parity gate — exact steps

The sandbox has no Neovim and no bacon-ls/lua-language-server; the
Diver-side and parity verification happens at the mirror step, per
estate practice:

1. Mirror the crate to primo (Owner's repo home for the estate
   line: `~/.GH/Qompass/theodolite` when created; until then a
   staging dir under `~/.local/share/theodolite/`). Do not push or
   publish without a separate Owner ruling.
2. Record primo's actual pins and write `bridge.toml` from
   `bridge.toml.example`:
   - bacon-ls: `bash -lc 'command -v bacon-ls'` (it lives in
     `~/.cargo/bin`, invisible to non-login PATH probes) and
     `bacon-ls --version` for the exact string. Confirm the binary
     is the real one, not a shim.
   - rust-analyzer: the toolchain's real binary path (not the
     rustup proxy — see §4.1) + `--version` string; set the entry
     `env.PATH` to include primo's cargo bin dir so RA can run
     `cargo metadata`.
   - lua-language-server: `paru -Q` / `pacman -Ql` to find the real
     binary (per the estate's tool-presence rule), then
     `--version`.
3. Build on primo with the pinned toolchain:
   `RUSTC_WRAPPER= cargo build --release` (the sccache wrapper
   breaks clippy on primo the same way; tests:
   `RUSTC_WRAPPER= cargo test --release`).
4. Run the parity gate for real:
   `THEODOLITE_BACON_LS=<primo bacon-ls path> cargo test --release --test parity -- --nocapture`
   — success is the equality assertion executing (not the SKIP
   line). Any delta fails the landing.
5. Live-server smokes on primo: `session_status` + `diagnostics`
   + one position call per server against a real workspace
   (a Diver Lua tree for LuaLS, a Rust crate for bacon-ls/RA),
   checking transcripts record primo's pinned versions.
6. Re-baseline `ready_timeout_ms` from primo's cold rust-analyzer
   readiness time (sandbox cold start was ~35 s; SPEC default is
   30 s — primo evidence decides whether the default moves).
7. Orphan gate re-run on primo (`cargo test --test mcp_stdio
   orphan_gate`) — release blocker there too.

## Judgment calls (all noted inline above)

1. Single crate with §2-named modules instead of a workspace.
2. `language_ids` map added to the §5 config shape (the SPEC's
   parallel arrays are ambiguous); positional pairing kept as a
   fallback.
3. In-flight vs post-death error codes (§4.6 refinement).
4. SIGKILL targets the child leader, not the process group
   (§3.6, `forbid(unsafe_code)` constraint).
5. `workspace/configuration` without declared settings answers
   nulls + logs a violation, instead of failing the server.
