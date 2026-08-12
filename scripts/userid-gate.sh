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

# Word-boundary match without GNU \b so a hypothetical `MyUserIdLike` type
# never trips the gate; only the real token does.
pattern='(^|[^A-Za-z0-9_])UserId([^A-Za-z0-9_]|$)'

if hits=$(grep -rEn "$pattern" "$src_dir" --include='*.rs' 2>/dev/null); then
	echo "FAIL (userid): UserId found in $src_dir (forbidden by ADR-0181):" >&2
	printf '%s\n' "$hits" >&2
	exit 1
fi

echo "OK (userid): no UserId in $src_dir (ADR-0181)"
