---
created: 2026-06-10T00:00:00Z
branch: audit/secrem02-signing-seam
author: Fable 5 (Claude Code)
sprint: SECREM-02-followup-remediation
status: active
repo: citrate-gui-native
baseline_test_count: 520
---

# citrate-gui-native — SECREM-02 Remediation Log

> Coverage matrix: `citrate-security/planset/2026-06-10-followup-remediation.md`.
> The other half of the **Phase-2 signing seam** (with citrate-node-agent).
> Protocol: re-verify → red test → fail-closed fix → suite green → mutation pass.

## Phase 2 — signing-seam (gui-native side)

| Finding | Sev | WP | Red test(s) | Fix | Suite | Disposition |
|---|---|---|---|---|---|---|
| FUA-GUI-02 | Med | 2.1 | `relay_service.rs::validate_node_agent_url_accepts_loopback` / `…_rejects_remote_and_non_http` | `validate_node_agent_url` rejects non-loopback host + non-`http` scheme; `NodeAgentClient::try_new` enforces it; `main.rs` uses `try_new` and **does not start the relay** against a bad node-agent address — `gui/citrate_desktop_app/src/services/relay_service.rs`, `gui/citrate_gui_native/src/main.rs` | relay tests 21 ✓ | **FIXED** |
| FUA-GUI-01 (auth half) | High | 2.1 | covered by node-agent gate tests + the relay continuing to function with the token | `NodeAgentClient` now loads the shared supervision token and presents `Authorization: Bearer` on every request (`authed()`), so the seam keeps working now that the node-agent surface is gated. The queue is no longer reachable by an unauthenticated process — the precondition that made FUA-GUI-01 reachable is closed. | 21 ✓ | **PARTIAL — auth closed; calldata-arg + per-write confirm = follow-up** |

## FUA-GUI-01 residual (tracked, not yet done)

The relay's `validate()` already refuses any **non-zero-value** write and checks
the **contract + 4-byte selector**, so the auto-sign surface is narrow; and the
node-agent queue is now **authenticated** (a malicious local process can't inject
into it). The remaining defense-in-depth — **ABI-decoding the calldata arguments**
and cross-checking them against the job context, plus a **per-write human
confirmation** for value-bearing writes in the Slint UI — is a larger change and
is carried forward as a focused follow-up under WP 2.1. With the queue
authenticated, its residual risk (a compromised/buggy node-agent feeding bad args)
is materially lower than the original "any local process" exposure.

## Notes
- Baseline (Phase 0): gui-native workspace **520**; relay_service tests **19 → 21**
  (+2 loopback-enforcement tests). Rule 2 satisfied.
- Shared contract: token file at `CITRATE_NODE_AGENT_TOKEN_FILE` /
  `$HOME/.citrate/node-agent/supervision.token` — same as node-agent.
- `NodeAgentClient::new` stays infallible (tests/e2e use a loopback mock);
  `try_new` is the production constructor that enforces loopback.
- Branch: `audit/secrem02-signing-seam`.
