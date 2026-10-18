#!/usr/bin/env sh
# Self-test for dep-gate.sh (issue #207). Drives the gate with a stubbed `cargo`
# so the vacuous-pass fix is verified deterministically, with no real crates:
#   - a cargo error must FAIL (the #207 defect: it used to pass vacuously)
#   - empty output must FAIL
#   - a forbidden crate in the tree must FAIL
#   - a clean tree must PASS
# Also self-tests userid-gate.sh and file-cap-gate.sh (#745: what the
# `#[cfg(test)]` module exclusion does and does not hide), doc-cap-gate.sh
# (#848), and test-isolated.sh (#747).
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

# --- userid-gate.sh self-tests (ADR-0181/ADR-0184, #687) ---------------------
ugate="$(dirname "$0")/userid-gate.sh"

ucheck() {
	name=$1
	want=$2
	dir=$3
	set +e
	sh "$ugate" "$dir" >/dev/null 2>&1
	got=$?
	set -e
	if [ "$got" -eq "$want" ]; then
		echo "ok   - $name (rc=$got)"
	else
		echo "FAIL - $name (want rc=$want, got rc=$got)"
		fails=$((fails + 1))
	fi
}

udir="$tmp/usrc"
mkdir -p "$udir"
printf 'fn ok() {}\n' >"$udir/clean.rs"
ucheck "userid: clean tree passes" 0 "$udir"
printf 'struct MyUserIdLike;\n' >"$udir/nearmiss.rs"
ucheck "userid: substring near-miss passes" 0 "$udir"
printf 'use entanglement_core::UserId;\n' >"$udir/hit.rs"
ucheck "userid: real UserId fails" 1 "$udir"
# "Anywhere" (header): a doc comment or a string literal counts too.
rm "$udir/hit.rs"
printf '/// see [`UserId`]\n' >"$udir/doc.rs"
ucheck "userid: UserId in a doc comment fails" 1 "$udir"
rm "$udir/doc.rs"
printf 'const S: &str = "UserId";\n' >"$udir/lit.rs"
ucheck "userid: UserId in a string literal fails" 1 "$udir"
rm "$udir/lit.rs"
mkdir -p "$udir/a/b"
printf 'type T = UserId;\n' >"$udir/a/b/deep.rs"
ucheck "userid: UserId in a nested dir fails" 1 "$udir"
rm "$udir/a/b/deep.rs"
printf 'UserId\n' >"$udir/notes.md"
ucheck "userid: non-.rs file is ignored" 0 "$udir"
rm "$udir/notes.md"
# #770: a grep error must FAIL, never read as "no match". Root reads anything,
# so the case is only meaningful (and only run) unprivileged.
if [ "$(id -u)" -ne 0 ]; then
	printf 'fn x() {}\n' >"$udir/unreadable.rs"
	chmod 000 "$udir/unreadable.rs"
	ucheck "userid: unreadable .rs fails (not vacuous)" 1 "$udir"
	rm -f "$udir/unreadable.rs"
fi
ucheck "userid: missing dir fails (not vacuous)" 1 "$tmp/no-such-dir"
mkdir -p "$tmp/no-rs"
printf 'UserId\n' >"$tmp/no-rs/notes.md"
ucheck "userid: dir without .rs files fails (not vacuous)" 1 "$tmp/no-rs"

# --- file-cap-gate.sh self-tests (#745) --------------------------------------
# Each case writes one crate/src/x.rs into a fresh tree with an empty
# allowlist; the gate runs from that tree's root.
fgate="$here/file-cap-gate.sh"
fdir="$tmp/fcap"

code() { # code <n> — n production lines
	i=0
	while [ "$i" -lt "$1" ]; do echo "fn f$i() {}"; i=$((i + 1)); done
}
tests_block() { # an inline cfg(test) module, braces hidden in literals
	cat <<'RS'
#[cfg(test)]
#[allow(clippy::all)]
mod tests {
    #[test]
    fn literals() {
        let a = "}}} {";
        let b = '}';
        let c = '{';
        let d = r#"} " {{"#;
        let e = "multi
}
line";
        // } stray
        /* } /* nested { */ } */
        let f: &'static str = "\"}";
    }
}
RS
}

fcheck() {
	name=$1
	want=$2
	set +e
	(cd "$fdir" && sh "$fgate" allow.txt) >/dev/null 2>&1
	got=$?
	set -e
	if [ "$got" -eq "$want" ]; then
		echo "ok   - $name (rc=$got)"
	else
		echo "FAIL - $name (want rc=$want, got rc=$got)"
		fails=$((fails + 1))
	fi
}

mkdir -p "$fdir/crate/src"
: >"$fdir/allow.txt"
src="$fdir/crate/src/x.rs"

{ printf '#[cfg(test)]\nmod tests;\n'; code 400; } >"$src"
fcheck "file-cap: early mod tests; hides nothing (400)" 0
{ printf '#[cfg(test)]\nmod tests;\n'; code 401; } >"$src"
fcheck "file-cap: early mod tests; hides nothing (401)" 1
{ code 400; tests_block; } >"$src"
fcheck "file-cap: inline tests block at end is free" 0
{ code 401; tests_block; } >"$src"
fcheck "file-cap: code before an end block still counts" 1
{ code 200; tests_block; code 200; } >"$src"
fcheck "file-cap: inline tests block in the middle is free" 0
{ code 200; tests_block; code 201; } >"$src"
fcheck "file-cap: code after a middle block counts" 1
{ code 300; printf '#[cfg(test)]\nfn helper() {\n'; code 97; printf '}\n'; } >"$src"
fcheck "file-cap: cfg(test) helper fn counts (400)" 0
{ code 300; printf '#[cfg(test)]\nfn helper() {\n'; code 98; printf '}\n'; } >"$src"
fcheck "file-cap: cfg(test) helper fn counts (401)" 1
{ printf '#[cfg(test)] mod tests { fn t() { let _ = "}"; } }\n'; code 400; } >"$src"
fcheck "file-cap: one-line block with brace in string" 0
{ printf '#[cfg(test)] mod tests { fn t() { let _ = "{"; } }\n'; code 401; } >"$src"
fcheck "file-cap: brace in string does not swallow the rest" 1
# An escaped-quote char literal (`'\''`) must close on its real quote.
quote_brace_block() {
	cat <<'RS'
#[cfg(test)]
mod tests {
    const Q: [char; 2] = ['\'','{'];
}
RS
}
{ quote_brace_block; code 400; } >"$src"
fcheck "file-cap: '\\'' then '{' in test block (400)" 0
{ quote_brace_block; code 401; } >"$src"
fcheck "file-cap: '\\'' then '{' does not swallow the rest" 1
{ code 399; cat <<'RS'; tests_block; } >"$src"
fn a() { let v = ['\'','"']; }
RS
fcheck "file-cap: '\\'' then '\"' before a test mod does not over-count" 0

# --- doc-cap-gate.sh self-tests (#848) ----------------------------------------
# Each case rebuilds a fresh docs tree; the gate is pointed at it.
dgate="$here/doc-cap-gate.sh"
ddir="$tmp/dcap"

lines() { # lines <n> — n lines of prose
	i=0
	while [ "$i" -lt "$1" ]; do echo "line $i"; i=$((i + 1)); done
}

dcheck() {
	name=$1
	want=$2
	set +e
	shift 2
	[ "$#" -gt 0 ] || set -- "$ddir"
	sh "$dgate" "$@" >/dev/null 2>&1
	got=$?
	set -e
	if [ "$got" -eq "$want" ]; then
		echo "ok   - $name (rc=$got)"
	else
		echo "FAIL - $name (want rc=$want, got rc=$got)"
		fails=$((fails + 1))
	fi
}

dreset() { rm -rf "$ddir" && mkdir -p "$ddir/sub dir" "$ddir/adr"; }

dreset; lines 400 >"$ddir/a.md"
dcheck "doc-cap: 400 lines passes" 0
dreset; lines 401 >"$ddir/a.md"
dcheck "doc-cap: 401 lines fails" 1
dreset; lines 10 >"$ddir/a.md"; lines 401 >"$ddir/sub dir/b.md"
dcheck "doc-cap: nested (spaced path) over-cap doc fails" 1
dreset; lines 10 >"$ddir/a.md"; lines 900 >"$ddir/adr/0001-x.md"
dcheck "doc-cap: adr/ is exempt" 0
dreset; lines 10 >"$ddir/a.md"; lines 900 >"$ddir/notes.txt"
dcheck "doc-cap: non-Markdown files are ignored" 0
dreset; lines 900 >"$ddir/adr/0001-x.md"
dcheck "doc-cap: no docs outside adr/ fails (not vacuous)" 1
dcheck "doc-cap: missing dir fails (not vacuous)" 1 "$tmp/no-such-docs"
dreset; lines 10 >"$ddir/a.md"; mkdir -p "$ddir/x/adr"; lines 401 >"$ddir/x/adr/b.md"
dcheck "doc-cap: nested x/adr/ is NOT exempt" 1
dreset; lines 10 >"$ddir/a.md"; lines 900 >"$ddir/adr/0001-x.md"
dcheck "doc-cap: trailing-slash dir keeps adr/ exempt" 0 "$ddir/"
dreset; lines 401 >"$ddir/a.md"
dcheck "doc-cap: trailing-slash dir still catches over-cap" 1 "$ddir//"
dreset; lines 10 >"$ddir/a.md"; lines 401 >"$ddir/[ab].md"; : >"$ddir/a"
dcheck "doc-cap: glob characters in a name stay inert" 1
dreset; lines 10 >"$ddir/a.md"; lines 401 >"$tmp/outside.md"; ln -s "$tmp/outside.md" "$ddir/link.md"
dcheck "doc-cap: symlinked over-cap doc fails" 1
dreset; lines 10 >"$ddir/a.md"; ln -s "$tmp/no-such-target.md" "$ddir/dangling.md"
dcheck "doc-cap: dangling .md symlink fails (fail-closed)" 1
dreset; lines 10 >"$ddir/a.md"; rm -rf "$tmp/outdir"; mkdir -p "$tmp/outdir"; lines 401 >"$tmp/outdir/b.md"; ln -s "$tmp/outdir" "$ddir/linked"
dcheck "doc-cap: over-cap doc inside a symlinked dir fails" 1
dreset; lines 10 >"$ddir/a.md"; ln -s .. "$ddir/sub dir/loop"
dcheck "doc-cap: symlink loop fails (fail-closed)" 1
dreset; lines 10 >"$ddir/a.md"
dcheck "doc-cap: two arguments is a usage error" 2 "$ddir" "$ddir"
dcheck "doc-cap: a dir starting with - is a usage error" 2 "-name"
dcheck "doc-cap: a glob character in the dir is a usage error" 2 "d*"
nl='
'
dreset; lines 10 >"$ddir/a.md"
if lines 401 2>/dev/null >"$ddir/new${nl}line.md"; then
	dcheck "doc-cap: newline-in-name over-cap doc fails" 1
	rm -f "$ddir/new${nl}line.md"; lines 10 >"$ddir/new${nl}line.md"
	dcheck "doc-cap: newline-in-name under-cap doc passes" 0
else
	echo "skip - doc-cap: filesystem refuses newline in a file name"
fi

# --- test-isolated.sh self-tests (#747) ---------------------------------------
# A stubbed `cargo test` that writes `$FAKE_WRITE` (relative to the sandboxed
# var it names, e.g. `XDG_DATA_HOME:entanglement/sessions/x`) and exits
# `$FAKE_RC`: proves the sandbox is really where `dirs::*` would resolve, and
# that a write there — or a red suite — fails the gate.
igate="$here/test-isolated.sh"
cat >"$tmp/cargo-iso" <<'STUB'
#!/usr/bin/env sh
if [ -n "${FAKE_WRITE:-}" ]; then
	var=${FAKE_WRITE%%:*}
	eval "base=\${$var}"
	mkdir -p "$base/${FAKE_WRITE#*:}"
fi
exit "${FAKE_RC:-0}"
STUB
chmod +x "$tmp/cargo-iso"

icheck() {
	name=$1
	want=$2
	set +e
	CARGO="$tmp/cargo-iso" FAKE_WRITE="$FAKE_WRITE" FAKE_RC="$FAKE_RC" \
		sh "$igate" >/dev/null 2>&1
	got=$?
	set -e
	if [ "$got" -eq "$want" ]; then
		echo "ok   - $name (rc=$got)"
	else
		echo "FAIL - $name (want rc=$want, got rc=$got)"
		fails=$((fails + 1))
	fi
}

FAKE_WRITE=""                                       FAKE_RC=0 icheck "isolated: clean suite passes"          0
FAKE_WRITE=""                                       FAKE_RC=1 icheck "isolated: red suite fails"             1
FAKE_WRITE="XDG_DATA_HOME:entanglement/sessions/x"  FAKE_RC=0 icheck "isolated: data-dir write fails"        1
FAKE_WRITE="XDG_CONFIG_HOME:entanglement"           FAKE_RC=0 icheck "isolated: config-dir write fails"      1
FAKE_WRITE="HOME:.local/share/entanglement"         FAKE_RC=0 icheck "isolated: HOME write fails"            1

if [ "$fails" -ne 0 ]; then
	echo "$fails dep-gate self-test(s) failed" >&2
	exit 1
fi
echo "all dep-gate self-tests passed"
