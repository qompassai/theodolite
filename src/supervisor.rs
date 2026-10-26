//! Session supervisor: the bridge's process-tree ownership mechanism
//! (SPEC section 3.6, P4 item 1).
//!
//! The bridge never spawns a language server directly. It spawns this
//! supervisor as its direct child, in the supervisor's own process
//! group, and the supervisor spawns the server inside that group. A
//! server and every daemon it leaks therefore share one process-group
//! id, and the supervisor can reap the whole tree by enumerating
//! `/proc` for that group and killing by explicit pid — never by
//! negative pgid, so the supervisor can never kill itself or the
//! bridge.
//!
//! The supervisor exits when the server exits, forwarding the exit
//! status (a server killed by signal S kills the supervisor with S,
//! so the bridge's reaper observes the same status shape it would
//! have seen from a direct spawn). Independently of stdio, the
//! supervisor polls the bridge's liveness via `/proc` (pid presence
//! plus start-time match, so a recycled pid does not count as alive);
//! a dead bridge means the tree is killed at once. On stdin EOF the
//! supervisor gives the server `--grace-ms` to exit on its own, then
//! kills the tree.
//!
//! This module uses only `std` plus spawning `/bin/kill`: the crate
//! forbids unsafe code, so no libc signal or killpg calls are
//! available. `/proc` parsing is pure and unit-tested.

use std::fs;
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

/// argv[1] value that selects supervisor mode in `main`.
pub const SUPERVISE_FLAG: &str = "--supervise";

/// Absolute path of the kill(1) helper used for tree kills.
const KILL_PATH: &str = "/bin/kill";
/// How often the supervisor's watch loop polls (server exit, bridge
/// liveness, stdin-EOF grace deadline).
const WATCH_POLL_MS: u64 = 50;
/// Sweeps of the process-group scan during one tree kill; repeated
/// sweeps catch members forked while an earlier sweep was running.
const TREE_SWEEP_COUNT: usize = 3;
/// Gap between tree-kill sweeps.
const TREE_SWEEP_INTERVAL_MS: u64 = 100;
/// After a tree kill, how long to wait for the server itself to be
/// reaped before the supervisor exits anyway.
const POST_KILL_REAP_TIMEOUT_MS: u64 = 2_000;

/// Parse `(pgrp, starttime_ticks)` out of a `/proc/<pid>/stat` line.
///
/// The comm field (field 2) is parenthesized and may itself contain
/// spaces and parentheses, so parsing starts after the LAST `)`.
/// Token indices relative to the post-comm tail: 0 = state,
/// 1 = ppid, 2 = pgrp, 19 = starttime (proc field 22 overall).
pub fn parse_stat(text: &str) -> Option<(u64, u64)> {
    let tail = text.rsplit(')').next()?.trim();
    let mut tokens = tail.split_whitespace();
    let pgrp: u64 = tokens.nth(2)?.parse().ok()?;
    let starttime: u64 = tokens.nth(16)?.parse().ok()?;
    Some((pgrp, starttime))
}

/// This process's start time in clock ticks since boot, read from
/// `/proc/self/stat`. The bridge records its own value at spawn time
/// and passes it to each supervisor so the supervisor can tell "the
/// bridge" apart from a later process that recycled the bridge's pid.
pub fn self_starttime_ticks() -> Option<u64> {
    let text = fs::read_to_string("/proc/self/stat").ok()?;
    parse_stat(&text).map(|(_, start)| start)
}

/// Read `(pgrp, starttime_ticks)` for an arbitrary pid.
fn stat_of(pid: u32) -> Option<(u64, u64)> {
    let text = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_stat(&text)
}

/// Whether the recorded bridge process is still alive: its `/proc`
/// entry exists and, when a start time was recorded, the start time
/// still matches. A recorded start of 0 means "unknown at spawn time"
/// and degrades to existence-only.
fn bridge_alive(pid: u32, recorded_start: u64) -> bool {
    match stat_of(pid) {
        None => false,
        Some((_, start)) => recorded_start == 0 || start == recorded_start,
    }
}

/// Pids in process group `pgid`, excluding `exclude`. Reads `/proc`
/// directly; processes that vanish mid-scan are skipped.
fn group_members(pgid: u64, exclude: u32) -> Vec<u32> {
    let mut members = Vec::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return members;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        if pid == exclude || pid == 0 {
            continue;
        }
        if let Some((member_pgid, _)) = stat_of(pid)
            && member_pgid == pgid
        {
            members.push(pid);
        }
    }
    members.sort_unstable();
    members
}

/// SIGKILL every member of `pgid` except `exclude`, by explicit pid
/// list. The helper runs in its own process group so it is never a
/// member of the tree it kills.
fn kill_tree(pgid: u64, exclude: u32) {
    for sweep in 0..TREE_SWEEP_COUNT {
        let members = group_members(pgid, exclude);
        if members.is_empty() {
            return;
        }
        let mut command = Command::new(KILL_PATH);
        command
            .arg("-KILL")
            .arg("--")
            .args(members.iter().map(u32::to_string))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let _ = command.process_group(0).spawn().and_then(|mut c| c.wait());
        if sweep + 1 < TREE_SWEEP_COUNT {
            thread::sleep(Duration::from_millis(TREE_SWEEP_INTERVAL_MS));
        }
    }
}

/// Exit the supervisor mirroring `status`: the exit code when the
/// server exited normally; otherwise re-raise the terminating signal
/// against the supervisor itself so the bridge observes a signal
/// death indistinguishable from a direct spawn's.
fn propagate(status: &ExitStatus) -> ! {
    if let Some(code) = status.code() {
        std::process::exit(code);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            let me = std::process::id().to_string();
            let _ = Command::new(KILL_PATH)
                .arg(format!("-{signal}"))
                .arg("--")
                .arg(me)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
            thread::sleep(Duration::from_millis(500));
            std::process::exit(128 + signal);
        }
    }
    std::process::exit(1);
}

/// Parsed supervisor arguments (the part after `--supervise`).
struct SupervisorArgs {
    bridge_pid: u32,
    bridge_start: u64,
    grace_ms: u64,
    server_argv: Vec<String>,
}

fn parse_supervisor_args(argv: &[String]) -> Result<SupervisorArgs, String> {
    let mut bridge_pid: Option<u32> = None;
    let mut bridge_start: u64 = 0;
    let mut grace_ms: u64 = 2_000;
    let mut index = 0;
    while index < argv.len() {
        match argv[index].as_str() {
            "--" => {
                let server_argv: Vec<String> = argv[index + 1..].to_vec();
                if server_argv.is_empty() {
                    return Err("supervisor: empty server argv".to_string());
                }
                return Ok(SupervisorArgs {
                    bridge_pid: bridge_pid
                        .ok_or_else(|| "supervisor: --bridge-pid is required".to_string())?,
                    bridge_start,
                    grace_ms,
                    server_argv,
                });
            }
            "--bridge-pid" | "--bridge-start" | "--grace-ms" => {
                let flag = argv[index].clone();
                let value = argv
                    .get(index + 1)
                    .ok_or_else(|| format!("supervisor: {flag} needs a value"))?;
                let parsed: u64 = value
                    .parse()
                    .map_err(|_| format!("supervisor: {flag} value is not a number"))?;
                match flag.as_str() {
                    "--bridge-pid" => {
                        bridge_pid = Some(
                            u32::try_from(parsed)
                                .map_err(|_| "supervisor: --bridge-pid out of range")?,
                        );
                    }
                    "--bridge-start" => bridge_start = parsed,
                    _ => grace_ms = parsed,
                }
                index += 2;
            }
            other => return Err(format!("supervisor: unexpected argument {other}")),
        }
    }
    Err("supervisor: missing -- separator before the server argv".to_string())
}

/// Spawn the three stdio forwarder threads. stdin EOF (or a failed
/// write to a dead server) sets `stdin_eof`; the watch loop treats
/// EOF as the graceful-shutdown trigger.
fn spawn_forwarders(child: &mut Child, stdin_eof: Arc<AtomicBool>) {
    let mut server_stdin = child.stdin.take();
    let eof_flag = stdin_eof.clone();
    thread::spawn(move || {
        let mut input = std::io::stdin();
        let mut buffer = [0u8; 8192];
        loop {
            match input.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(count) => {
                    let Some(writer) = server_stdin.as_mut() else {
                        break;
                    };
                    if writer.write_all(&buffer[..count]).is_err() || writer.flush().is_err() {
                        break;
                    }
                }
            }
        }
        // Dropping server_stdin here closes the server's stdin,
        // which is how a cooperative server learns about EOF.
        eof_flag.store(true, Ordering::SeqCst);
    });
    if let Some(mut server_stdout) = child.stdout.take() {
        thread::spawn(move || {
            let mut out = std::io::stdout();
            let mut buffer = [0u8; 8192];
            loop {
                match server_stdout.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => {
                        if out.write_all(&buffer[..count]).is_err() || out.flush().is_err() {
                            break;
                        }
                    }
                }
            }
        });
    }
    if let Some(mut server_stderr) = child.stderr.take() {
        thread::spawn(move || {
            let mut err = std::io::stderr();
            let mut buffer = [0u8; 8192];
            loop {
                match server_stderr.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => {
                        if err.write_all(&buffer[..count]).is_err() || err.flush().is_err() {
                            break;
                        }
                    }
                }
            }
        });
    }
}

/// After a tree kill, wait (bounded) for the server to be reaped and
/// propagate its status; if it somehow survives, exit 137 anyway —
/// the tree kill already SIGKILLed every enumerated member.
fn reap_after_kill(child: &mut Child) -> ! {
    let deadline = Instant::now() + Duration::from_millis(POST_KILL_REAP_TIMEOUT_MS);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => propagate(&status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    std::process::exit(137);
                }
                thread::sleep(Duration::from_millis(WATCH_POLL_MS));
            }
            Err(_) => std::process::exit(137),
        }
    }
}

/// Supervisor mode entry point. `argv` is everything after
/// `--supervise`. Returns the process exit code on early failure;
/// the normal paths end in `propagate`/`reap_after_kill` (`!`).
pub fn run_supervisor(argv: &[String]) -> i32 {
    let args = match parse_supervisor_args(argv) {
        Ok(args) => args,
        Err(detail) => {
            eprintln!("theodolite supervisor: {detail}");
            return 2;
        }
    };
    let my_pid = std::process::id();
    // The tree group is what /proc says this process's group is
    // (the bridge sets it via process_group(0), making it == my_pid;
    // measuring beats assuming).
    let my_pgid = stat_of(my_pid).map_or(u64::from(my_pid), |(pgid, _)| pgid);

    let mut command = Command::new(&args.server_argv[0]);
    command
        .args(&args.server_argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            eprintln!(
                "theodolite supervisor: cannot spawn {}: {error}",
                PathBuf::from(&args.server_argv[0]).display()
            );
            return 127;
        }
    };

    let stdin_eof = Arc::new(AtomicBool::new(false));
    spawn_forwarders(&mut child, stdin_eof.clone());

    let mut grace_deadline: Option<Instant> = None;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // The server is gone; no daemon it leaked may
                // outlive it inside this tree.
                kill_tree(my_pgid, my_pid);
                propagate(&status);
            }
            Ok(None) => {}
            Err(error) => {
                eprintln!("theodolite supervisor: wait failed: {error}");
                kill_tree(my_pgid, my_pid);
                return 1;
            }
        }
        if !bridge_alive(args.bridge_pid, args.bridge_start) {
            kill_tree(my_pgid, my_pid);
            reap_after_kill(&mut child);
        }
        if stdin_eof.load(Ordering::SeqCst) {
            let deadline = *grace_deadline
                .get_or_insert_with(|| Instant::now() + Duration::from_millis(args.grace_ms));
            if Instant::now() >= deadline {
                kill_tree(my_pgid, my_pid);
                reap_after_kill(&mut child);
            }
        }
        thread::sleep(Duration::from_millis(WATCH_POLL_MS));
    }
}
