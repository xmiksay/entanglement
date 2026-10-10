#!/usr/bin/env sh
# Runs the test suite with HOME and the XDG base dirs pointed at a fresh empty
# temp dir, then fails if anything was written there (issue #747).
#
# `dirs::data_dir()`/`config_dir()` resolve under these vars, so a test that
# still reaches the developer's real session logs, grants, config or managed
# files would write into the sandbox instead — and be caught here, rather than
# silently leaking `tmp-*` dirs into ~/.local/share/entanglement/sessions.
#
# Usage: test-isolated.sh [cargo test args...]   (default: --workspace)
set -eu

[ $# -eq 0 ] && set -- --workspace

sandbox=$(mktemp -d "${TMPDIR:-/tmp}/entanglement-isolated-XXXXXX")
trap 'rm -rf "$sandbox"' EXIT

# Cargo and rustup themselves live under the real HOME; pin them before HOME
# moves so the toolchain still resolves.
CARGO_HOME=${CARGO_HOME:-$HOME/.cargo}
RUSTUP_HOME=${RUSTUP_HOME:-$HOME/.rustup}
export CARGO_HOME RUSTUP_HOME

# A developer's own ENTANGLEMENT_* overrides would steer tests at real files
# the sandbox can't see — run with none of them.
for var in $(env | sed -n 's/^\(ENTANGLEMENT_[A-Za-z0-9_]*\)=.*/\1/p'); do
	unset "$var"
done

HOME=$sandbox/home \
	XDG_DATA_HOME=$sandbox/data \
	XDG_CONFIG_HOME=$sandbox/config \
	XDG_CACHE_HOME=$sandbox/cache \
	XDG_STATE_HOME=$sandbox/state \
	"${CARGO:-cargo}" test "$@"

leaked=$(find "$sandbox" -mindepth 1 | sed "s|^$sandbox/||")
if [ -n "$leaked" ]; then
	echo "FAIL (test-isolated): the test suite wrote to the user's home/XDG dirs:" >&2
	echo "$leaked" | head -50 >&2
	exit 1
fi
echo "ok (test-isolated): nothing written to HOME/XDG dirs"
