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

## Surface (read-only; v1 + P3 read-class completion)

Tools: `session_status`, `diagnostics` (push accumulation merged
with pull where the server advertises it, per-diagnostic channel
provenance), `hover`, `definition`, `declaration`,
`type_definition`, `implementation`, `references`,
`document_symbols`, `workspace_symbols`, `code_actions` (listing
only — payloads never returned, never executed),
`session_restart`. The mutation class is not registered at all;
there is no generic any-LSP-method tool and no rename apply.

Servers (config data, not code): bacon-ls (diagnostics owner),
rust-analyzer (semantics), lua-language-server (second proof
server).

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
