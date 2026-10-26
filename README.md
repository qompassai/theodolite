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

## v1 surface (read-only)

Tools: `session_status`, `diagnostics`, `hover`, `definition`,
`references`, `document_symbols`, `session_restart`. The mutation
class is not registered at all in v1; there is no generic
any-LSP-method tool and no rename apply.

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
  `setpriv --pdeathsig SIGKILL` in scrubbed environments, reaped by
  the bridge; SIGKILLing the bridge takes its servers down with it
  (orphan gate in the test suite).
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

## Tests

```sh
cargo test            # unit + stdio suites + parity harness
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

The parity gate (`tests/parity.rs`) compares bridge diagnostics
against standalone bacon-ls byte-for-byte; it skips loudly where no
bacon-ls binary exists and runs for real on primo.
