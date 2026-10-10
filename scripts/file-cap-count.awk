# Prints the "code lines" of one Rust source file for file-cap-gate.sh
# (#745): every line except a `#[cfg(test)]`-gated module — an inline
# `mod x { ... }` block wherever it sits (brace-matched) or a `mod x;`
# declaration line — plus the attribute/blank/comment lines between the
# `#[cfg(test)]` and its `mod`. Any other `#[cfg(test)]` item (helper fn,
# impl, static, use) still counts: it is not the test module.
#
# Only the exact `#[cfg(test)]` form is exempt. A compound cfg
# (`#[cfg(all(test, ...))]`) or a multi-line attribute between the
# `#[cfg(test)]` and its `mod` is not recognized, so that module counts —
# deliberately fail-safe: an unrecognized shape over-counts, never hides.
#
# The brace matcher runs a tiny lexer over the whole file so braces inside
# strings, raw strings, char literals and (nested) comments never move the
# depth, and a `#[cfg(test)]` inside a multi-line string is not a marker.
# POSIX awk only (CI's mawk and local gawk both run it).

function identc(ch) { return ch ~ /[A-Za-z0-9_]/ }

# Lexer state: st 0 code, 1 "string", 2 raw string (closed by `"` + rawh
# hashes), 3 block comment (nesting depth cdep). Code braces move `depth`.
function lex(s,    i, n, c, j, h, p) {
	n = length(s); i = 1
	while (i <= n) {
		c = substr(s, i, 1)
		if (st == 1) {
			if (c == "\\") { i += 2; continue }
			if (c == "\"") st = 0
			i++; continue
		}
		if (st == 2) {
			if (c == "\"" && substr(s, i + 1, rawh) == substr(HASHES, 1, rawh)) {
				st = 0; i += 1 + rawh; continue
			}
			i++; continue
		}
		if (st == 3) {
			if (substr(s, i, 2) == "*/") { cdep--; if (cdep == 0) st = 0; i += 2; continue }
			if (substr(s, i, 2) == "/*") { cdep++; i += 2; continue }
			i++; continue
		}
		if (substr(s, i, 2) == "//") return
		if (substr(s, i, 2) == "/*") { st = 3; cdep = 1; i += 2; continue }
		if (c == "\"") { st = 1; i++; continue }
		p = substr(s, i - 1, 1)
		if (c == "r" && (!identc(p) || (p == "b" && !identc(substr(s, i - 2, 1))))) {
			j = i + 1; h = 0
			while (substr(s, j, 1) == "#") { j++; h++ }
			if (substr(s, j, 1) == "\"") { st = 2; rawh = h; i = j + 1; continue }
		}
		if (c == "'") {
			# A char literal can hold a brace; a lifetime (`'a`) cannot.
			if (substr(s, i + 1, 1) == "\\") {
				# Skip the escaped char itself: in `'\''` it is the quote.
				j = i + 3
				while (j <= n && substr(s, j, 1) != "'") j++
				i = j + 1; continue
			}
			if (substr(s, i + 2, 1) == "'") { i += 3; continue }
			i++; continue
		}
		if (c == "{") { if (++depth > peak) peak = depth }
		else if (c == "}") depth--
		i++
	}
}

# The block may open on its `mod` line or a later one (`mod tests\n{`), and
# may close on the line it opens (`peak` catches the one-liner).
function start_block(line) {
	d0 = depth; peak = depth; excl++; lex(line)
	inmod = !(peak > d0 && depth <= d0)
}

BEGIN {
	HASHES = "################################"
	DECL = "^[ \t]*(pub(\\([^)]*\\))?[ \t]+)?mod[ \t]+[A-Za-z_][A-Za-z0-9_]*[ \t]*;"
	BLOCK = "^[ \t]*(pub(\\([^)]*\\))?[ \t]+)?mod[ \t]+[A-Za-z_][A-Za-z0-9_]*[ \t]*([{]|$)"
}

{
	line = $0
	if (inmod) {
		excl++; lex(line)
		if (peak > d0 && depth <= d0) inmod = 0
		next
	}
	if (pending) {
		if (line ~ /^[ \t]*$/ || line ~ /^[ \t]*\/\// || line ~ /^[ \t]*#\[/) {
			pbuf++; lex(line); next
		}
		pending = 0
		if (line ~ DECL) { excl += pbuf + 1; lex(line); next }
		if (line ~ BLOCK) { excl += pbuf; start_block(line); next }
		lex(line); next
	}
	if (st == 0 && line ~ /^[ \t]*#\[cfg\(test\)\]/) {
		rest = line
		sub(/^[ \t]*#\[cfg\(test\)\][ \t]*/, "", rest)
		if (rest == "") { pending = 1; pbuf = 1; next }
		if (rest ~ DECL) { excl++; next }
		if (rest ~ BLOCK) { start_block(line); next }
	}
	lex(line)
}

END { print NR - excl }
