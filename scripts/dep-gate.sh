#!/usr/bin/env sh
# Shared dependency-hygiene gate backing `make tree` and `make check-lean`
# (issue #207; ADR-0006, amended by ADR-0053; ADR-0025).
#
# Both gates run `cargo tree` for one crate's subgraph and reject any forbidden
# crate. This one script is the single mechanism so their edge policy stays
# unified: normal edges only (`-e normal`) — build-, dev-, and proc-macro-only
# deps are excluded because they are not what an embedder links, so they must
# neither trip the gate nor mask a forbidden normal dep.
#
# The defect this fixes (#207): the old inline gates piped `cargo tree` through
# `2>/dev/null` and never checked its exit status, so a *failed* `cargo tree`
# (e.g. a metadata error) produced empty output, grepped clean, and reported
# success — the gate passed vacuously while verifying nothing. Here a cargo
# error or empty output is a hard FAIL.
#
# Usage: dep-gate.sh <label> <forbidden-regex> <cargo-tree-selectors...>
#   label            human name shown in pass/fail messages
#   forbidden-regex  extended-regex alternation of forbidden crate names
#   selectors        passed verbatim to `cargo tree` (e.g. -p CRATE [--no-default-features])
set -eu

if [ "$#" -lt 3 ]; then
	echo "usage: dep-gate.sh <label> <forbidden-regex> <cargo-tree-selectors...>" >&2
	exit 2
fi

label=$1
forbidden=$2
shift 2

CARGO=${CARGO:-cargo}

# Capture stdout+stderr AND the exit status. A non-zero `cargo tree` must fail
# the gate loudly instead of degrading to a vacuous pass (#207).
if ! out=$("$CARGO" tree -e normal "$@" 2>&1); then
	printf '%s\n' "$out" >&2
	echo "FAIL ($label): 'cargo tree' errored — the gate cannot verify deps, refusing to pass vacuously (#207)" >&2
	exit 1
fi

# A successful-but-empty tree can only mean the selectors matched nothing; treat
# it as a misconfiguration rather than a clean pass.
if [ -z "$out" ]; then
	echo "FAIL ($label): 'cargo tree' produced no output — the gate cannot verify deps (#207)" >&2
	exit 1
fi

# cargo tree prints each dep as `<name> v<version>`; anchor on that so a
# forbidden name only matches a real dependency line.
if printf '%s\n' "$out" | grep -Eiq "(^|[^a-zA-Z0-9_-])($forbidden) v[0-9]"; then
	echo "FAIL ($label): forbidden crate in dependency tree (see ADR-0053):" >&2
	printf '%s\n' "$out" | grep -Ei "(^|[^a-zA-Z0-9_-])($forbidden) v[0-9]" >&2
	exit 1
fi

echo "OK ($label): dependency tree clean"
