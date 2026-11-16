//! Manifest agreement: the agent card and server.json are a
//! provisioning pair (D-1605). Client configs for the estate's
//! agent runtimes are generated from card + server.json pairs
//! (D-1606), so the join keys must agree exactly:
//!
//! - card `name` == server.json `title`
//! - card `version` == server.json `version`
//! - the card's MCP-interface extension `registryName` ==
//!   server.json `name`
//!
//! The card must also stay honest AND spec-valid (D-1607): it
//! declares exactly one interface, bound with the qompass MCP
//! custom binding (a URI protocolBinding, per the A2A v1.0
//! custom-transport rule) whose url carries the stdio command
//! under that binding's own semantics; no core A2A binding is
//! claimed, and the invocation facts ride in the extension
//! params. `stateTransitionHistory` was removed in A2A v1.0 and
//! must never appear.

use std::path::PathBuf;

/// The repo root holding `server.json` and `.well-known/`:
/// this crate's manifest dir, or its nearest ancestor that
/// carries a server.json (workspace layouts).
fn repo_root() -> PathBuf {
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for _ in 0..4 {
        if dir.join("server.json").exists() {
            return dir;
        }
        if !dir.pop() {
            break;
        }
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read_json(rel: &str) -> serde_json::Value {
    let path = repo_root().join(rel);
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

#[test]
fn card_and_server_json_agree_on_the_join_keys() {
    let card = read_json(".well-known/agent-card.json");
    let server = read_json("server.json");
    assert_eq!(card["name"], server["title"], "card name / server title");
    assert_eq!(
        card["version"], server["version"],
        "card version / server version"
    );
    let extension = &card["capabilities"]["extensions"][0];
    assert_eq!(
        extension["params"]["registryName"], server["name"],
        "extension registryName / server name"
    );
}

#[test]
fn card_declares_no_a2a_endpoint_and_carries_invocation() {
    let card = read_json(".well-known/agent-card.json");
    let interfaces = card["supportedInterfaces"].as_array().expect("interfaces");
    assert_eq!(interfaces.len(), 1, "exactly one declared interface");
    assert_eq!(
        interfaces[0]["protocolBinding"],
        "https://qompass.ai/extensions/mcp-interface/v1"
    );
    assert_eq!(interfaces[0]["protocolVersion"], "1.0");
    assert!(
        card["capabilities"].get("stateTransitionHistory").is_none(),
        "stateTransitionHistory was removed in A2A v1.0"
    );
    let invocation = &card["capabilities"]["extensions"][0]["params"]["invocation"];
    assert_eq!(invocation["transport"], "stdio");
    assert_eq!(invocation["protocolEra"], "2026-07-28");
    assert_eq!(
        interfaces[0]["url"], invocation["command"],
        "interface url carries the invocation command under the binding"
    );
    let skills = card["skills"].as_array().expect("skills");
    assert!(!skills.is_empty(), "at least one skill");
    for skill in skills {
        for field in ["id", "name", "description", "tags"] {
            assert!(
                skill.get(field).is_some(),
                "skill missing required field {field}"
            );
        }
    }
}
