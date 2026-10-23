//! Pins the production constructor of the relay's node-agent client.
//!
//! `NodeAgentClient::try_new` is the only constructor the GUI uses for the
//! signing relay. It must refuse (a) a non-loopback or userinfo-bearing URL
//! and (b) a missing or empty supervision token, so the relay never polls an
//! unauthenticated or remote queue. The helper `validate_node_agent_url` has
//! its own unit tests; this file pins that `try_new` actually calls it and
//! that the token check fails closed.
//!
//! It is its own test binary with a single `#[test]`, because it points the
//! token-file env override at a temp file and env vars are process-global.
//! No network: `try_new` only builds a client, it never connects.

use citrate_desktop_app::services::relay_service::{NodeAgentClient, NODE_AGENT_TOKEN_PATH_ENV};

fn err_text(r: Result<NodeAgentClient, impl std::fmt::Display>) -> Option<String> {
    r.err().map(|e| e.to_string())
}

#[test]
fn try_new_fails_closed_on_missing_token_and_non_loopback_url() {
    let dir = tempfile::tempdir().expect("tempdir");
    let token_path = dir.path().join("supervision.token");
    std::env::set_var(NODE_AGENT_TOKEN_PATH_ENV, &token_path);

    // 1. No token file at all: refused, and refused for the token reason.
    let e = err_text(NodeAgentClient::try_new("http://127.0.0.1:19600"))
        .expect("try_new must refuse when the supervision token file is absent");
    assert!(e.contains("supervision token"), "wrong refusal reason: {e}");

    // 2. Empty / whitespace-only token: same refusal.
    std::fs::write(&token_path, "  \n").expect("write empty token");
    let e = err_text(NodeAgentClient::try_new("http://127.0.0.1:19600"))
        .expect("try_new must refuse an empty supervision token");
    assert!(e.contains("supervision token"), "wrong refusal reason: {e}");

    // 3. Positive control: a real token on a loopback URL is accepted, so the
    //    refusals above are the token check and not something else.
    std::fs::write(&token_path, "tok_0123456789abcdef\n").expect("write token");
    assert!(
        NodeAgentClient::try_new("http://127.0.0.1:19600").is_ok(),
        "loopback URL + token must be accepted"
    );
    assert!(NodeAgentClient::try_new("http://[::1]:19600").is_ok());

    // 4. With a valid token present, the URL check is the only thing left to
    //    refuse these. Each must be refused for the URL reason.
    for bad in [
        "http://evil.example:19600",
        "http://10.0.0.5:19600",
        "https://127.0.0.1:19600",
        "http://127.0.0.1:19600@evil.example/",
        "http://localhost:1@attacker.tld/",
    ] {
        let e = err_text(NodeAgentClient::try_new(bad))
            .unwrap_or_else(|| panic!("try_new must refuse {bad:?}"));
        assert!(
            !e.contains("supervision token"),
            "{bad:?} refused for the wrong reason: {e}"
        );
    }

    std::env::remove_var(NODE_AGENT_TOKEN_PATH_ENV);
}
