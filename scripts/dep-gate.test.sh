#!/usr/bin/env sh
# Self-test for dep-gate.sh (issue #207). Drives the gate with a stubbed `cargo`
# so the vacuous-pass fix is verified deterministically, with no real crates:
#   - a cargo error must FAIL (the #207 defect: it used to pass vacuously)
#   - empty output must FAIL
#   - a forbidden crate in the tree must FAIL
#   - a clean tree must PASS
# Run via `make test-gates`.
set -eu

here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
gate="$here/dep-gate.sh"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# Stub cargo: prints $FAKE_OUT and exits $FAKE_RC, ignoring its arguments.
cat >"$tmp/cargo" <<'STUB'
#!/usr/bin/env sh
printf '%s' "${FAKE_OUT:-}"
[ -n "${FAKE_OUT:-}" ] && printf '\n'
exit "${FAKE_RC:-0}"
STUB
chmod +x "$tmp/cargo"

fails=0
# check <name> <expected-rc> — run the gate under the stub, compare exit status.
check() {
	name=$1
	want=$2
	set +e
	CARGO="$tmp/cargo" FAKE_OUT="$FAKE_OUT" FAKE_RC="$FAKE_RC" \
		sh "$gate" "test" 'clap|axum' -p dummy >/dev/null 2>&1
	got=$?
	set -e
	if [ "$got" -eq "$want" ]; then
		echo "ok   - $name (rc=$got)"
	else
		echo "FAIL - $name (want rc=$want, got rc=$got)"
		fails=$((fails + 1))
	fi
}

FAKE_OUT=""              FAKE_RC=1 check "cargo error fails (not vacuous)" 1
FAKE_OUT=""              FAKE_RC=0 check "empty output fails"              1
FAKE_OUT="dummy v0.1.0
├── clap v4.5.0"         FAKE_RC=0 check "forbidden crate fails"          1
FAKE_OUT="dummy v0.1.0
├── tokio v1.0.0"        FAKE_RC=0 check "clean tree passes"              0
# A near-miss name must not false-positive (anchoring guard).
FAKE_OUT="dummy v0.1.0
├── clap-verbosity v1.0" FAKE_RC=0 check "substring near-miss passes"    0

if [ "$fails" -ne 0 ]; then
	echo "$fails dep-gate self-test(s) failed" >&2
	exit 1
fi
echo "all dep-gate self-tests passed"
