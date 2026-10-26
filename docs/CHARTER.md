# Theodolite — consumer charter

Theodolite is a headless, deterministic LSP bridge MCP server
(`ai.qompass/theodolite`). It owns pinned language-server sessions
and exposes them as bounded, canonically ordered MCP tools. It
never touches an editor.

This charter states which estate tool answers which question, so
consumers route deliberately instead of by habit. It restates the
non-overlap charter of the design spec (SPEC §10).

## Routing: who answers what

- **periscope** — when a live editor running the real Diver config
  is in the loop. Periscope answers "what does the human's editor
  currently show?": editor state, buffers, the format/lint
  pipelines as the editor runs them. If a person is sitting in
  Neovim and the question is about that session, the answer comes
  through periscope.
- **theodolite** — when there is no live editor in the loop:
  primo worker contexts, build and release gates, sandbox agents,
  CI-style checks. Theodolite answers "what do this workspace's
  pinned language servers say, headlessly and deterministically?"
  Same servers, same versions, same answers, with no editor
  involved and no editor state to depend on.
- **SCIP indexes** — for static questions over committed trees,
  answered offline: definitions, references, and cross-references
  as committed, without running a server at all. If the question
  is about a tree nobody has open, index it; do not start a
  session.
- **phlow checks** — for build / lint / test verdicts. Those are
  operator-approved commands with pass/fail outcomes, and phlow is
  where they live. Theodolite is a phlow client-side peer, not a
  phlow component; phlow may consume it as an MCP client like any
  other.

Rule of thumb: editor in the loop → periscope; no editor →
theodolite; no live question at all → SCIP; a verdict, not an
answer → phlow checks.

## Client setup

Theodolite serves MCP over stdio (NDJSON JSON-RPC). A client
starts it with a config file:

```sh
theodolite --config bridge.toml
# or: THEODOLITE_CONFIG=bridge.toml theodolite
```

The config is data, not code — see `bridge.toml.example` in the
repository root for the full shape. Each `[[server]]` entry pins:

- `binary` — the absolute path of the real server binary on the
  target machine. Never point it at a rustup proxy: proxies need
  ambient environment the bridge deliberately scrubs. Pin the
  toolchain's real binary.
- `version_probe` and `expect_version` — the probe argv and the
  exact `--version` output recorded on the target machine. The
  bridge probes every binary at session start and refuses a
  mismatch; a stale pin is a typed startup/session error, never a
  silent substitution.
- `languages`, `extensions`, `language_ids` — what the server
  owns.
- `readiness` — how the bridge knows the server is ready (state
  predicates, never fixed sleeps).
- `env` — the scrubbed environment the server is spawned with.

Timeouts (`request_timeout_ms`, `ready_timeout_ms`,
`shutdown_grace_ms`) are config data with spec defaults.

**Parity expectation.** Two theodolite instances agree when their
configs agree: same servers, same binary versions, same probe
expectations, same initialization options verbatim. That is the
point of pinning — a gate on one machine and a worker on another
read the same diagnostics because they run the same servers the
same way. If a result differs, diff the configs before suspecting
the code.

Sessions key on `(workspace, server)`, at most 4. A dead session
is never silently respawned; `session_restart` is the only
respawn path.

## Mutation posture

The read class is the default surface. Two apply tools exist —
`code_action_apply` and `rename_apply` — and they are governed by
a decide()-class gate in the config:

- `mutation = "off"` (the default) registers no apply tools at
  all. They are absent from `tools/list`, and calling one answers
  `UnsupportedMethod`.
- `mutation = "gated"` together with a non-empty
  `mutation_allowlist` of client names registers the apply tools
  **only** for connected clients whose MCP handshake
  `clientInfo.name` appears in the allowlist.
- Any other combination — unknown value, gated without an
  allowlist, an allowlist while off — refuses to start with a
  typed `ConfigInvalid`. An ambiguous posture never drifts on
  silently.

**Identity is a routing label, not authentication.** The gate's
identity is the handshake's `clientInfo.name`: a self-declared
label inside the host's trust boundary. It decides which surface a
connected client is offered; it does not prove who the client is,
and it must not be treated as an auth layer. Run the bridge only
where the host already trusts its stdio clients.

Every apply decision — allow or deny, with its reason — is
recorded in the session transcript (`session_status` →
`gate_decisions`, metadata only, bounded). Applies go through
opaque single-use handles minted by `code_actions` (for
`code_action_apply`) or a fresh recompute with hash revalidation
(for `rename_apply`), validate the whole edit set before writing
anything, and write per file atomically. `rename_preview` belongs
to the read class: it computes and renders a rename's edit set and
writes nothing, under either posture.

Guidance: leave the posture `off` unless a workflow genuinely
needs applies; when it does, allowlist the exact client name that
workflow presents, and treat any unexpected name in
`gate_decisions` as a signal to inspect, not noise.
