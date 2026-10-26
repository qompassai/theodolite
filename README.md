# theodolite

A headless, deterministic LSP bridge MCP server (IDEA-0092,
`ai.qompass/theodolite`): pinned language-server sessions exposed
as bounded read tools for editor-free contexts — primo workers,
gates, sandbox agents.

Theodolite is the complement of periscope (IDEA-0013): periscope
answers through the human's live Neovim; theodolite owns its own
pinned server sessions and never touches an editor.

Design authority: `~/workspace/lsp-mcp-study-20261025/SPEC.md`.
Build record: `BUILD-REPORT.md` in this directory.
Consumer charter (routing vs periscope / SCIP / phlow checks,
client setup, mutation posture): `docs/CHARTER.md`.

## Surface (read class; v1 + P3 read-class completion)

Tools: `session_status`, `diagnostics` (push accumulation merged
with pull where the server advertises it, per-diagnostic channel
provenance), `hover`, `definition`, `declaration`,
`type_definition`, `implementation`, `references`,
`document_symbols`, `workspace_symbols`, `code_actions` (listing
only — payloads never returned, never executed),
`rename_preview`, `session_restart`. There is no generic
any-LSP-method tool, no `edit_file`, no formatting tool, and no
`workspace/executeCommand` passthrough — the surface stays a
closed list by construction.

Servers (config data, not code): bacon-ls (diagnostics owner),
rust-analyzer (semantics), lua-language-server (second proof
server), serve-d (D; config entry and unit coverage landed,
live-session parity passed 2026-10-26 — see Capability notes).

## Capability notes — serve-d

- Plain stdio LSP with no required argv. Upstream's `--require`
  accepts feature names (`d`, `workspaces`) only; a component
  name there makes the server intentionally refuse to start, so
  the registry entry carries no args.
- serve-d fronts DCD for completion/definition and loads the dub
  workspace after the handshake; the entry therefore declares
  `first-response` readiness and a PATH carrying `dub`/`dcd-server`.
  serve-d activates its project seconds after the handshake on
  its own schedule, and its workspace indexing never presents a
  quiescent window inside the readiness timeout — so readiness
  is the handshake plus the first real answered request against
  a workspace file (SPEC §3/§7), not drained progress.
- Its custom `served/*` requests and `coded/*` notifications stay
  outside the bridge's closed read surface by construction; the
  standard read methods are what the tools exercise. Configuration
  serve-d asks for via `workspace/configuration` (the `d.*`
  section) is answerable through the entry's `settings` field.
- Primo status (verified 2026-10-26): the packaged serve-d
  0.7.6-1 cannot launch — it links `libphobos2-ldc-shared.so.111`
  and `libdruntime-ldc-shared.so.111`, while the system LDC 1.43
  ships only the `.113` pair. A source-built serve-d (dub + LDC
  1.43) now exists on primo at `~/.local/bin/serve-d`; machine
  copies of this config point `binary` there, while the shipped
  example keeps `/usr/bin/serve-d`. serve-d prints its version
  on stderr only; the version probe accepts the trimmed stderr
  when stdout is empty (SPEC §4.1). `dcd-server`/`dcd-client`
  0.16.3 and dfmt 0.15.2 on the same machine run fine. A live
  parity run for this entry PASSED on primo on 2026-10-26
  against the source-built binary: the bridge session reached
  ready under `first-response`, and its `document_symbols` and
  `hover` results for the probe file match a direct LSP probe
  of the same binary exactly (symbols `add`/`main` with
  identical ranges; hover text identical). Calls made before
  serve-d's delayed project activation completes return the
  server's own empty answers — a direct client at the same
  moment gets the same emptiness; that is serve-d's timing,
  faithfully passed through, not a bridge loss.

## Mutation class (P5; gated, off by default)

Two further tools exist, registered only under the mutation
gate: `code_action_apply` and `rename_apply`. `rename_preview`
belongs to the read class — it computes and renders a rename's
edit set and writes nothing; the gate does not govern it.

- **Posture.** Config carries `mutation = "off"` (default) or
  `mutation = "gated"` together with a non-empty
  `mutation_allowlist` of client names. Any other combination —
  unknown value, gated without an allowlist, an allowlist while
  off — refuses to start with a typed `ConfigInvalid`; an
  ambiguous posture never drifts on silently.
- **Identity and registration.** The gate's identity is the MCP
  handshake's `clientInfo.name` — a routing label inside the
  host's trust boundary (SPEC §8), not an authentication layer.
  Only when the posture is gated AND the connected client is
  allowlisted do the two apply tools appear in `tools/list`;
  under every other combination they are absent, and calling
  one answers `UnsupportedMethod`. A gated config with no
  handshake identity fails closed.
- **Decisions.** Every apply is decided by a pure function
  (`gate::decide`) over posture, identity, allowlist, handle
  state, and version state. Every decision — allow or deny, with
  its reason — is recorded in the session transcript
  (`session_status` → `gate_decisions`, metadata only, bounded).
- **Handles.** `code_actions` mints an opaque single-use handle
  for each edit-carrying action, only while the apply class is
  registered for the client. The edit body and a listing-time
  SHA-256 snapshot of every target stay server-side; listings
  still never carry payloads. `code_action_apply` takes a handle
  only — a caller-supplied edit is a typed refusal. At apply
  time a forged, spent, cross-session, or version-drifted handle
  is a typed `GateDenied`, logged. An allowed apply consumes the
  handle before writing, so a spent handle never retries.
- **`rename_apply`** takes the preview's arguments, not a
  handle: it recomputes fresh, re-validates every tracked
  document's hash between compute and write (drift refuses the
  whole rename, nothing written), and answers `applied: true`
  where the preview answers `applied: false`.
- **The applier** is the bridge's only write path. It validates
  the whole set before writing anything (workspace boundary on
  every target, existence, UTF-8, span bounds, overlap), then
  writes per file atomically — temp sibling + rename, original
  permissions preserved — and re-syncs touched files. File
  create / rename / delete operations are typed refusals
  everywhere: the bridge edits file *contents* only, and a
  `workspace/applyEdit` pushed by a server is never honored.

## Run

```sh
cargo build --release
./target/release/theodolite --config bridge.toml
```

MCP over stdio, NDJSON JSON-RPC. The bridge refuses to start on an
invalid config. See `bridge.toml.example` for the config shape;
pin real binary paths and exact `--version` output on the target
machine.

## Properties

- Children are spawned by absolute path through
  `setpriv --pdeathsig SIGKILL` in scrubbed environments, one
  supervisor per session (next section); SIGKILLing the bridge
  takes every session tree down with it (orphan gates in the test
  suite).
- Sessions key on `(workspace, server)`; at most 4; a dead session
  is never silently respawned — `session_restart` is the only
  respawn path.
- Every result is canonically ordered and byte-bounded; overflow is
  a typed `ResultTooLarge` with the true item count, never a silent
  truncation. Errors are typed with stable codes (SPEC §4.6).
- Readiness is state-based (initialize handshake + per-server
  predicate; rust-analyzer requires observed load activity to have
  drained plus a successful probe). Fixed sleeps are never a
  readiness mechanism.

## Shutdown and process-tree ownership (SPEC §3.6)

Each session runs under a supervisor: the bridge re-executes
itself with `--supervise` as the session's direct child and
process-group leader, and the supervisor spawns the server inside
that group (the `setpriv --pdeathsig SIGKILL` wrapper now binds
the server to the supervisor). A server and every daemon it leaks
share the supervisor's process group, and the supervisor reaps
the whole tree — enumerating `/proc` by group, killing by
explicit pid, never by negative pgid — on every path:

- **Transport EOF / session close / eviction:** the bridge sends
  LSP `shutdown` + `exit` and waits `shutdown_grace_ms`; a
  surviving tree is escalated by closing the supervisor's stdin,
  whose EOF starts the supervisor's own grace-then-tree-kill.
  `Child::kill` on the supervisor alone is the last resort.
- **Server death (any cause):** the supervisor kills the rest of
  the tree and exits mirroring the server's status, so the
  bridge's reaper records the death exactly as for a direct spawn.
- **Bridge SIGKILL / SIGTERM:** no bridge code can run (SIGKILL)
  or no handler is installed (SIGTERM, deliberately — the LSP
  graceful step cannot run on either path and this is stated, not
  smoothed over). The guarantee moves to the supervisors: each
  polls the bridge's `/proc` presence plus its recorded start
  time every 50 ms — a recycled pid does not count as alive — and
  kills its tree the moment the bridge is gone.

Proving gates: `tree_gate_*` in `tests/mcp_stdio.rs` run a fake
server that leaks a real daemon grandchild and assert server,
daemon, and supervisor are all gone within 5 s after session
restart, transport EOF, SIGKILL, and SIGTERM respectively; the
pre-P4 orphan gate (server only) still runs.

Measured limits, stated as limits: after a bridge SIGKILL, tree
reaping is bounded by the supervisor's 50 ms poll plus its
`/proc` sweep, not instantaneous; and if a supervisor itself
wedges so hard that even its EOF path cannot run, the bridge's
last-resort kill takes the supervisor alone — a daemon could in
principle outlive that one path. No other residue class is known:
in the P4 primo smoke (a real bacon-ls session whose headless
backend daemon parks in the session tree), the daemon is gone
within the same 5 s bound on EOF, SIGKILL, and SIGTERM alike.

## Tests

```sh
cargo test            # unit + stdio suites + parity harness
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

The parity gate (`tests/parity.rs`) compares bridge diagnostics
against standalone bacon-ls byte-for-byte; it skips loudly where no
bacon-ls binary exists and runs for real on primo.
