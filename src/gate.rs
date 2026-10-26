//! The decide()-class gate for the mutation class (SPEC §6, P5
//! item 2), modeled on phlow-approval semantics: policy + scope +
//! a recorded decision, as a pure function.
//!
//! The gate has two moments, both decided here and nowhere else:
//!
//! 1. **Registration.** Mutation tools exist in `tools/list` only
//!    when the config posture is `gated` AND the connected client's
//!    identity (the MCP `initialize` handshake's `clientInfo.name`)
//!    is on the config allowlist. Otherwise the tools are absent —
//!    not listed, not merely refused — so a client cannot even
//!    learn their argument shapes from a bridge that would deny it.
//! 2. **Apply.** Every apply call re-decides with the full input
//!    set (posture, identity, allowlist, handle validity, version
//!    state). Registration is not a standing permission: a handle
//!    minted in a posture that has since been reconfigured, or
//!    against documents that have since drifted, is denied at the
//!    moment of writing.
//!
//! `decide` is pure: no I/O, no clock, no session access. Callers
//! gather the inputs, call it, and record the outcome (the session
//! transcript's gate log for apply decisions; the transcript's
//! posture/identity fields for registration).

use crate::config::{Config, MutationPosture};

/// The state of an action handle at apply time, as the caller
/// observed it in the session's handle store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandleState {
    /// The operation carries no handle (rename apply recomputes).
    NotApplicable,
    /// The handle exists, is unused, and belongs to this session.
    Valid,
    /// No such handle in this session: forged, expired with a dead
    /// session, or minted by another session (indistinguishable,
    /// and deliberately reported as one class).
    Missing,
    /// The handle exists but its single use is spent.
    Used,
    /// The handle exists and belongs to a different session.
    WrongSession,
}

/// The version state of the edit set's target documents, comparing
/// the hashes captured when the set was computed against the bytes
/// on disk now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionState {
    /// The operation has no pre-computed edit set to re-validate.
    NotApplicable,
    /// Every target still hashes to its computed-at value.
    Current,
    /// At least one target changed since the set was computed.
    Drifted,
}

/// One gate outcome. `Deny` carries the reason verbatim: it is
/// rendered into the typed `GateDenied` error and recorded in the
/// session transcript, so it is written for both audiences.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateDecision {
    /// The operation may proceed to the applier.
    Allow,
    /// Refused; nothing may be written.
    Deny(String),
}

impl GateDecision {
    /// Whether this decision allows the operation.
    pub fn is_allow(&self) -> bool {
        matches!(self, GateDecision::Allow)
    }

    /// The recorded reason: the denial verbatim, or the standing
    /// allow summary for the transcript.
    pub fn reason(&self) -> String {
        match self {
            GateDecision::Allow => {
                "posture gated, identity allowlisted, handle and version checks passed".to_string()
            }
            GateDecision::Deny(reason) => reason.clone(),
        }
    }
}

/// Decide one mutation-class operation. Every input is explicit;
/// every denial names the check that failed. Checks run in
/// blast-radius order (posture, identity, handle, version) and the
/// first failure is the answer.
pub fn decide(
    posture: MutationPosture,
    identity: Option<&str>,
    allowlist: &[String],
    handle: HandleState,
    version: VersionState,
) -> GateDecision {
    if posture != MutationPosture::Gated {
        return GateDecision::Deny("mutation posture is off".to_string());
    }
    let Some(identity) = identity else {
        return GateDecision::Deny(
            "no client identity: the MCP initialize handshake carried no clientInfo name"
                .to_string(),
        );
    };
    if !allowlist.iter().any(|name| name == identity) {
        return GateDecision::Deny(format!(
            "client {identity:?} is not on the mutation allowlist"
        ));
    }
    match handle {
        HandleState::NotApplicable | HandleState::Valid => {}
        HandleState::Missing => {
            return GateDecision::Deny(
                "unknown action handle (forged, expired, or minted by another session)".to_string(),
            );
        }
        HandleState::Used => {
            return GateDecision::Deny(
                "action handle was already used; handles are single-use".to_string(),
            );
        }
        HandleState::WrongSession => {
            return GateDecision::Deny("action handle belongs to a different session".to_string());
        }
    }
    if version == VersionState::Drifted {
        return GateDecision::Deny(
            "a target document changed since the edit set was computed; refusing a blind write"
                .to_string(),
        );
    }
    GateDecision::Allow
}

/// The registration decision (gate moment 1): would this client,
/// under this config, be offered the mutation tools at all?
pub fn mutation_registered(config: &Config, identity: Option<&str>) -> bool {
    decide(
        config.mutation,
        identity,
        &config.mutation_allowlist,
        HandleState::NotApplicable,
        VersionState::NotApplicable,
    )
    .is_allow()
}
