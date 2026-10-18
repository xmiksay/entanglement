#!/usr/bin/env sh
# Enforces the 400-line cap on the prose docs (issue #848): every `*.md` under
# the docs dir, recursively, except the immutable ADR log (`<dir>/adr/` only —
# a nested `adr/` elsewhere is ordinary docs), whose records are never edited
# and so never split. The count is plain `wc -l` — docs have no test-module
# exemption and no allowlist: an over-cap module is split into a sub-page
# directory (the module file stays as its index), so there is no grandfathered
# debt to track.
#
# Fail-closed by construction: paths never pass through word splitting (find
# hands them straight to the checker via `-exec … {} +`, so spaces, newlines
# and glob characters are inert). `find -L` follows symlinks, so a symlinked
# `.md` is counted through its target and a symlinked directory is descended
# into — neither can dodge the cap; under `-L` only a dangling link is still
# `-type l`, and it lands in the checker's unreadable branch, while a symlink
# loop is a find error. Any count that is not a plain number fails the gate
# instead of passing it. A missing dir, or one with no Markdown at all, fails
# rather than passing vacuously.
#
# Usage: doc-cap-gate.sh [docs-dir]  (default: docs; exit 2 on a usage error)
set -eu

CAP=400
usage() {
	echo "usage: doc-cap-gate.sh [docs-dir]  ($1)" >&2
	exit 2
}
[ "$#" -le 1 ] || usage "at most one argument"
dir=${1:-docs}
case $dir in
-*) usage "a dir starting with '-' would read as a find option: pass ./$dir" ;;
*[*?[]*) usage "a glob character in the dir would widen the adr prune" ;;
esac
# `docs/` would build `-path docs//adr`, which never matches — strip slashes.
while [ "$dir" != / ] && [ "${dir%/}" != "$dir" ]; do dir=${dir%/}; done

if [ ! -d "$dir" ]; then
	echo "FAIL (doc-cap): docs dir not found: $dir" >&2
	exit 1
fi

# The checker prints one `.` per file checked (stdout, for the count) and each
# violation to stderr; it exits non-zero on any violation or unreadable file,
# and find propagates that.
rc=0
dots=$(find -L "$dir" -path "$dir/adr" -prune -o \( -type f -o -type l \) -name '*.md' -exec sh -c '
	cap=$1
	shift
	bad=0
	for f do
		printf .
		n=$(wc -l 2>/dev/null <"$f") || n=
		n=$(printf "%s" "$n" | tr -d " \t")
		case $n in
		"" | *[!0-9]*)
			printf "  %s: unreadable (not a regular file, or a dangling link)\n" "$f" >&2
			bad=1
			;;
		*)
			if [ "$n" -gt "$cap" ]; then
				printf "  %s: %s lines (cap %s)\n" "$f" "$n" "$cap" >&2
				bad=1
			fi
			;;
		esac
	done
	exit "$bad"
' doc-cap "$CAP" {} +) || rc=$?

count=${#dots}
if [ "$rc" -ne 0 ]; then
	echo "FAIL (doc-cap): docs over the ${CAP}-line cap (or unreadable) — split along a section seam into a sub-page directory" >&2
	exit 1
fi
if [ "$count" -eq 0 ]; then
	echo "FAIL (doc-cap): no *.md files under $dir (outside $dir/adr)" >&2
	exit 1
fi

echo "OK (doc-cap): $count docs under $dir (outside $dir/adr), none over $CAP lines"
