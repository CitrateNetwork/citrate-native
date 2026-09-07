#!/usr/bin/env python3
"""NAT-B-018 / NAT-B-036 tripwire.

`audit.toml` bills itself as the documentation source-of-truth for the
cargo-audit advisory ignores, but CI applied its ignores inline via
`--ignore` flags in `.github/workflows/cargo-audit.yml`. Pre-fix the two
lists had drifted (audit.toml listed one advisory; CI suppressed three),
so an assessor reading audit.toml saw a different suppression set than the
one actually in force.

This check fails when the set of `RUSTSEC-*` ids ignored in `audit.toml`
is not identical to the set passed via `--ignore` in the workflow. Run it
from the repo root; exits non-zero (with a diff) on mismatch.
"""
from __future__ import annotations

import re
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
AUDIT_TOML = REPO_ROOT / "audit.toml"
WORKFLOW = REPO_ROOT / ".github" / "workflows" / "cargo-audit.yml"

RUSTSEC_RE = re.compile(r"RUSTSEC-\d{4}-\d{4}")


def ignores_from_audit_toml(text: str) -> set[str]:
    """Advisory ids that appear inside the `ignore = [ ... ]` array as
    quoted string entries (not the ones only mentioned in comments)."""
    # Grab the ignore array body.
    m = re.search(r"ignore\s*=\s*\[(.*?)\]", text, re.DOTALL)
    if not m:
        return set()
    body = m.group(1)
    # Only quoted entries count as actual ignores.
    return set(re.findall(r'"(RUSTSEC-\d{4}-\d{4})"', body))


def ignores_from_workflow(text: str) -> set[str]:
    """Advisory ids passed via `--ignore <id>` in the workflow run block."""
    return set(re.findall(r"--ignore\s+(RUSTSEC-\d{4}-\d{4})", text))


def main() -> int:
    audit = ignores_from_audit_toml(AUDIT_TOML.read_text())
    workflow = ignores_from_workflow(WORKFLOW.read_text())

    if audit == workflow:
        print(f"OK: audit.toml and cargo-audit.yml agree on {len(audit)} ignore(s).")
        return 0

    print("MISMATCH between audit.toml and cargo-audit.yml advisory ignores:")
    only_audit = sorted(audit - workflow)
    only_wf = sorted(workflow - audit)
    if only_audit:
        print(f"  in audit.toml but NOT ignored in CI: {only_audit}")
    if only_wf:
        print(f"  ignored in CI but NOT in audit.toml: {only_wf}")
    print(
        "\nEvery suppressed advisory must be justified in audit.toml (the "
        "assessor-facing source of truth) AND applied in the workflow. "
        "Reconcile the two lists."
    )
    return 1


if __name__ == "__main__":
    sys.exit(main())
