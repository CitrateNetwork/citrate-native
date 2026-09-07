#!/usr/bin/env bash
# NAT-B-006 tripwire: the wallet builds against ONE copy of each
# citrate-chain crate, pinned by an immutable `rev` — never a floating
# `branch=`. A duplicate crate name resolved from two different sources
# (e.g. `?rev=385e82e1` alongside a transitive `?branch=main`) means the
# audited key-management code is not the only code that ships, and a
# `branch=` source recompiles whatever `main` points at the moment the
# lock is regenerated.
#
# Fails (exit 1) when Cargo.lock contains either:
#   1. any `source = "...branch=..."` line, or
#   2. any crate name appearing more than once with differing `source`s.
#
# RED at the audited commit (edu-app pulls citrate-chain@branch=main);
# GREEN once a workspace [patch] collapses every citrate-chain source to
# the audited rev.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOCK="$ROOT/Cargo.lock"

if [[ ! -f "$LOCK" ]]; then
    echo "FAIL: Cargo.lock not found at $LOCK" >&2
    exit 1
fi

status=0

# 1. No branch-tracking git sources.
if grep -nE '^source = "git\+.*branch=' "$LOCK" >/dev/null; then
    echo "FAIL (NAT-B-006): Cargo.lock has branch-tracking git source(s):" >&2
    grep -nE '^source = "git\+.*branch=' "$LOCK" >&2
    status=1
fi

# 2. No crate name resolved from two different sources.
dupes="$(
    awk '
        /^name = "/    { name = $3; gsub(/"/, "", name) }
        /^source = "/  {
            src = $0
            sub(/^source = "/, "", src); sub(/"$/, "", src)
            key = name SUBSEP src
            if (!(key in seen)) { seen[key] = 1; count[name]++ }
        }
        END {
            for (n in count) if (count[n] > 1) print n
        }
    ' "$LOCK" | sort -u
)"

if [[ -n "$dupes" ]]; then
    echo "FAIL (NAT-B-006): crate(s) resolved from >1 source:" >&2
    echo "$dupes" >&2
    status=1
fi

if [[ "$status" -eq 0 ]]; then
    echo "OK (NAT-B-006): single-source, rev-pinned chain deps."
fi
exit "$status"
