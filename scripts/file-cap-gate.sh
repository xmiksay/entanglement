#!/usr/bin/env sh
# Enforces AGENTS.md's "files must not exceed 400 lines" rule (issue #451).
#
# Scans every `*/src/**/*.rs` file (the three crates' library/binary code —
# `*/tests/**` integration suites and any file literally named `tests.rs`
# are dedicated test modules by this repo's convention: `#[cfg(test)] mod
# tests;` in the parent declares the gate, not the child file itself, so
# they are excluded outright) and counts "code lines": every line except a
# `#[cfg(test)]`-gated module — an inline `mod x { … }` block anywhere in the
# file (brace-matched, string/char/comment-aware) or a `mod x;` declaration
# (scripts/file-cap-count.awk). Other `#[cfg(test)]` items (helper fns,
# impls) ship beside production code and still count. Issue #745: the old
# "everything before the first `#[cfg(test)]`" rule let an early
# `mod tests;` hide the rest of the file. A file over 400 code lines fails the gate unless listed in
# the allowlist file (grandfathered debt, issue #451); an allowlisted file
# that has shrunk back under the cap fails too, forcing the allowlist to
# shrink as files get split instead of silently going stale.
#
# Usage: file-cap-gate.sh [allowlist-file]  (default: scripts/file-cap-allowlist.txt)
set -eu

CAP=400
allowlist=${1:-scripts/file-cap-allowlist.txt}

if [ ! -f "$allowlist" ]; then
	echo "FAIL (file-cap): allowlist file not found: $allowlist" >&2
	exit 1
fi

counter="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)/file-cap-count.awk"

# Lines outside the file's `#[cfg(test)]`-gated module(s); see the awk file.
code_lines() {
	awk -f "$counter" "$1"
}

is_allowlisted() {
	path=$1
	grep -qxF "$path" "$allowlist"
}

fail=0
violations=""
stale_allowlist=""

files=$(find . -path ./target -prune -o -path '*/src/*' -name '*.rs' -print \
	| grep -v '/tests\.rs$' \
	| sed 's|^\./||' \
	| sort)

for f in $files; do
	lines=$(code_lines "$f")
	over=0
	if [ "$lines" -gt "$CAP" ]; then
		over=1
	fi

	if [ "$over" -eq 1 ]; then
		if is_allowlisted "$f"; then
			: # grandfathered — allowed to stay red until split
		else
			fail=1
			violations="$violations
  $f: $lines lines (cap $CAP)"
		fi
	else
		if is_allowlisted "$f"; then
			fail=1
			stale_allowlist="$stale_allowlist
  $f: $lines lines, now under cap — remove from $allowlist"
		fi
	fi
done

if [ -n "$violations" ]; then
	echo "FAIL (file-cap): files over the ${CAP}-line cap and not in $allowlist:" >&2
	printf '%s\n' "$violations" >&2
fi

if [ -n "$stale_allowlist" ]; then
	echo "FAIL (file-cap): allowlist entries no longer over the cap — shrink $allowlist:" >&2
	printf '%s\n' "$stale_allowlist" >&2
fi

if [ "$fail" -eq 1 ]; then
	exit 1
fi

echo "OK (file-cap): no file exceeds $CAP code lines outside $allowlist"
