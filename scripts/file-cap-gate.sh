#!/usr/bin/env sh
# Enforces AGENTS.md's "files must not exceed 400 lines" rule (issue #451).
#
# Scans every `*/src/**/*.rs` file (the three crates' library/binary code —
# `*/tests/**` integration suites and any file literally named `tests.rs`
# are dedicated test modules by this repo's convention: `#[cfg(test)] mod
# tests;` in the parent declares the gate, not the child file itself, so
# they carry no such marker to split on and are excluded outright) and
# counts "code lines" as everything before the file's own top-level
# `#[cfg(test)]` marker (an inline `mod tests` boundary) — or the whole file
# if it has none. A file over 400 code lines fails the gate unless listed in
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

# code-lines-before-first-#[cfg(test)] marker (or whole file if absent).
code_lines() {
	awk '
		/^#\[cfg\(test\)\]/ { print NR - 1; found = 1; exit }
		END { if (!found) print NR }
	' "$1"
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
