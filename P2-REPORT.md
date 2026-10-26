# Theodolite P2 — primo landing: report

**Date:** 2026-10-26 · **By:** Pax (personal execution, per the phase
authorization) · **Tree:** sandbox repo @ 7e6434e; primo staging
~/.local/share/theodolite/ (content == repo + machine bridge.toml +
target/).

## Gate results (all firsthand on primo)

| Gate | Result |
|---|---|
| Mirror + pins | PASS — bridge.toml records exact pins: bacon-ls 0.23.0 at ~/.cargo/bin/bacon-ls; rust-analyzer = the nightly-2026-09-25 toolchain's real binary, "rust-analyzer 1.100.0-nightly (f7575a9 2026-09-24)"; lua-language-server 3.19.2. The bacon-ls entry carries Diver's own initializationOptions verbatim (found necessary — see parity findings). |
| Release build | PASS (7.0 s). |
| Full test suite | **31 passed / 0 failed** — 22 stdio + 8 unit + 1 parity. |
| Clippy `-D warnings` (all targets) | PASS. |
| rustfmt `--check` | PASS. |
| **Parity gate** | **PASS** — bridge diagnostics byte-equal standalone bacon-ls over the same fixture (2 real diagnostics: one E0308 in two location entries), 7.56 s. |
| Live smoke: bacon-ls | PASS (the parity flow + the orphan flow both drive the real pinned server through the bridge). |
| Live smoke: rust-analyzer | PASS — hover on a fixture binding returned real content (`let value: u32`, size/align) in **2.47 s** cold. **ready_timeout re-baseline: the 30 s default stands with a wide margin on primo** (sandbox cold was ~35 s; primo is far faster). |
| Live smoke: lua-language-server | PASS WITH A FINDING — session + tool path work (0.33 s answer), but the diagnostics call returned an empty set for a file where standalone LuaLS verifiably reports `Undefined global` — the bridge's settle predicate returned before LuaLS's first push. This is the P4 settle-predicate item with primo evidence now attached (see Findings). |
| Orphan gate | PASS for the bridge's direct child: bacon-ls died **0.001 s** after the bridge's SIGKILL (pdeathsig). Grandchild residue confirmed — see Findings. |

## Findings (each cost real time; all measured)

1. **The parity harness had three defects, all exposed by its first
   primo run** (fixed in 7e6434e; the gate was never faked — it hung,
   then failed loudly, then passed):
   - DirectClient read on a blocking reader whose deadlines were only
     consulted between messages; a quiet server hung it 44 minutes.
     Fixed with a reader thread + recv_timeout.
   - A bare `initialize` leaves bacon-ls in a degraded mode (one
     empty publish, then silence): it needs Diver's
     initializationOptions (background backend, locations file) and
     the bridge-shaped params.
   - **bacon-ls publishes, on open, the state of `.bacon-locations`.**
     On a cold directory the backend computes the real results into
     that file but does not republish them to the already-open
     session (editor edit/save cycles are the republish trigger in
     real use). Measured two-arm on primo: cold client = empty set
     after 60 s despite a populated locations file; warm client =
     the full set at 2.0 s. The harness now warms the fixture with a
     throwaway session and measures both sides fresh against the
     same state.
2. **Bridge cold-workspace consequence of the same behavior:** a
   bridge `diagnostics` call against a never-warmed workspace can
   return the pre-computation set until a later sync cycle. Noted
   for P3's pull-diagnostics work — a pull would sidestep the
   republish gap entirely for servers that support it.
3. **LuaLS settle evidence (for P4):** bridge settle = progress
   tokens drained + generation stable across two polls. LuaLS's
   first push for a small workspace arrives after that predicate
   can already fire on generation 0. The P4 settle revisit should
   weigh a per-server minimum-observation window or a first-push
   requirement for push-diagnostics owners.
4. **Grandchild residue (for P4 §3.6):** the headless `bacon`
   daemon a bacon-ls session spawns survives both its server's
   death and the bridge's, reparented to init, holding the
   workspace's `.bacon.socket` and file watches (pgid = the dead
   server's pid). Pax cleaned the probe-spawned strays on primo;
   one pre-existing daemon belonging to a live parent process was
   left untouched. The P4 process-group kill closes this class;
   until then, long-lived bridge use against bacon-ls accumulates
   one daemon per session.

## State

- Sandbox repo: 7e6434e (parity fix), bridge.toml intentionally
  uncommitted (machine-local config; bridge.toml.example remains
  the template).
- Primo staging: ~/.local/share/theodolite/ with target/release
  built; probe fixtures remain under primo /tmp (bacon-probe*-ws,
  ra-smoke-ws, lua-smoke-ws) as evidence, all inert.
- Next: P3 (read-class completion) — briefed separately; the
  pull-diagnostics item in P3 should read Finding 2 first.
