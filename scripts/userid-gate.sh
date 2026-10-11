#!/usr/bin/env sh
# UserId hygiene gate backing `make userid` (ADR-0181/ADR-0184, #687).
#
# `UserId` must not appear anywhere in `entanglement-runtime/src` — not in
# library seams, not in heads. The type is provider-defined, core-re-exported,
# and rides the wire for out-of-crate embedders only; runtime seams are
# `SessionId`-keyed closures/traits the embedder supplies. This gate keeps the
# invariant from regressing one convenience import at a time.
#
# Mirrors dep-gate.sh's no-vacuous-pass posture: a missing source directory is
# a hard FAIL (misconfiguration), never a clean pass.
set -eu

src_dir=${1:-entanglement-runtime/src}

if [ ! -d "$src_dir" ]; then
	echo "FAIL (userid): source directory '$src_dir' not found — the gate cannot verify anything, refusing to pass vacuously" >&2
	exit 1
fi

# A directory with no `.rs` files at all is as unverifiable as a missing one
# (#779) — e.g. a typo'd path pointing at a docs dir.
if [ -z "$(find "$src_dir" -type f -name '*.rs' | head -n 1)" ]; then
	echo "FAIL (userid): no .rs files under '$src_dir' — the gate cannot verify anything, refusing to pass vacuously" >&2
	exit 1
fi

# Word-boundary match without GNU \b so a hypothetical `MyUserIdLike` type
# never trips the gate; only the real token does.
pattern='(^|[^A-Za-z0-9_])UserId([^A-Za-z0-9_]|$)'

# POSIX-only find + grep (#770). The old `grep -rEn PAT DIR --include='*.rs'
# 2>/dev/null` relied on GNU option permutation: under POSIXLY_CORRECT the
# trailing `--include` became a filename operand, grep exited 2, and the gate
# read that error as "no match" — a vacuous pass. Here grep's rc 1 (no match in
# this batch) is the only non-zero status tolerated; anything else (unreadable
# file, bad regex) makes `find` exit non-zero and FAILs the gate. `/dev/null`
# forces grep to prefix the filename without the non-POSIX `-H`.
if ! hits=$(find "$src_dir" -type f -name '*.rs' -exec sh -c '
	grep -En "$0" /dev/null "$@"
	rc=$?
	[ "$rc" -le 1 ]
' "$pattern" {} +); then
	echo "FAIL (userid): scanning $src_dir errored — the gate cannot verify anything, refusing to pass vacuously" >&2
	exit 1
fi

if [ -n "$hits" ]; then
	echo "FAIL (userid): UserId found in $src_dir (forbidden by ADR-0181):" >&2
	printf '%s\n' "$hits" >&2
	exit 1
fi

echo "OK (userid): no UserId in $src_dir (ADR-0181)"
