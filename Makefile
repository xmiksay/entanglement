.DEFAULT_GOAL := help
CARGO ?= cargo
PKG ?= 

## ---------- targets ----------
.PHONY: help build install run run-json run-tui pipe serve sessions inspect test test-unit test-integration test-gates lint fmt check-fmt verify clean check tree check-lean coverage tag

# Forbidden-crate sets for the dependency-hygiene gates (issue #207; ADR-0006,
# amended by ADR-0053; ADR-0025). These are the *policy*; scripts/dep-gate.sh is
# the shared *mechanism* (unified edge policy + a hard fail on cargo error/empty
# output, closing the vacuous-pass defect #207).
#
# CORE_FORBIDDEN — UI/web-server crates that must never reach entanglement-core.
# reqwest/hyper/tower are NOT here: they now ride in legitimately via provider
# (ADR-0053). Beyond ADR-0053's named set (clap/axum/tonic/crossterm/ratatui)
# this also bans the web/websocket stacks the old grep let sail through
# (warp/actix/rocket/tungstenite/ureq — the #207 blocklist-completeness gap).
CORE_FORBIDDEN ?= clap|axum|warp|actix-web|actix|rocket|tonic|tungstenite|crossterm|ratatui|ureq
# LEAN_FORBIDDEN — CLI/TUI/transport crates that must stay out of the
# no-default-features runtime library (ADR-0025's set, amended by ADR-0053).
# `axum` rides the `serve` head's feature (#153, ADR-0048), so it must not leak
# into the lean build either.
LEAN_FORBIDDEN ?= clap|ratatui|crossterm|syntect|pulldown-cmark|diffy|tracing-subscriber|axum

# Minimum line-coverage % the release gate enforces. First measured baseline
# (issue #107) was 65% workspace lines; floor set just under it to absorb CI
# variance. Ratchet up as coverage improves — never lower it.
COV_MIN ?= 60

help: ## show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | awk 'BEGIN{FS=":.*?## "}{printf "  \033[36m%-15s\033[0m %s\n", $$1, $$2}'

build: ## cargo build --workspace
	$(CARGO) build --workspace

install: ## install the `skutter` binary into $CARGO_HOME/bin (cargo install --path)
	$(CARGO) install --path entanglement-runtime --locked

run: build ## build + run the stdio head once (one dummy turn)
	$(CARGO) run -p entanglement-runtime -- run "Hello, Holly!" $(ARGS)

run-json: build ## stream one turn as NDJSON events (like opencode run --format json)
	$(CARGO) run -p entanglement-runtime -- run --format json "Hello, Holly!"

run-tui: build ## launch the terminal UI
	$(CARGO) run -p entanglement-runtime -- tui

pipe: build ## stdio pipe head — InMsg NDJSON on stdin, OutEvent NDJSON on stdout
	$(CARGO) run -p entanglement-runtime -- pipe

serve: build ## WebSocket serve head — local loopback HTTP+WS on 127.0.0.1 (ARGS='--port 4517')
	$(CARGO) run -p entanglement-runtime -- serve $(ARGS)

sessions: build ## list past (resumable) sessions
	$(CARGO) run -p entanglement-runtime -- sessions

inspect: build ## inspect resolved prompt/agents/skills, no engine (ARGS='prompt --agent build' | agents | 'skills --disclosures')
	$(CARGO) run -p entanglement-runtime -- inspect $(ARGS)

check: ## cargo check --workspace (fast typecheck)
	$(CARGO) check --workspace

test: ## all tests (unit + integration)
	$(CARGO) test --workspace

test-unit: ## unit tests only
	$(CARGO) test --workspace --lib --bins

test-integration: ## integration tests only (tests/ dirs)
	$(CARGO) test --workspace --test '*'

lint: ## cargo clippy, warnings = errors
	$(CARGO) clippy --all-targets -- -D warnings

fmt: ## cargo fmt (write)
	$(CARGO) fmt --all

check-fmt: ## cargo fmt --check (CI)
	$(CARGO) fmt --all -- --check

# Core hygiene gate (ADR-0006, amended by ADR-0053): entanglement-core must pull
# in zero UI/web-server crates. Since ADR-0053 inverted the seam, core depends on
# entanglement-provider and so legitimately carries `reqwest`/`hyper` (the LLM
# transport) transitively — those are no longer forbidden. The shared
# scripts/dep-gate.sh runs `cargo tree` and hard-fails on a cargo error or empty
# output (no more vacuous pass, #207).
tree: ## fail if entanglement-core pulls a forbidden UI/web-server crate
	@CARGO='$(CARGO)' sh scripts/dep-gate.sh entanglement-core '$(CORE_FORBIDDEN)' -p entanglement-core

# Lean gate (ADR-0025, amended by ADR-0053): entanglement-runtime with
# --no-default-features must stay free of CLI/TUI crates so library consumers get
# a light build. `reqwest`/`hyper` ride in via core → provider and are not
# forbidden here; the CLI/TUI crates still are. Same shared mechanism as `tree`.
check-lean: ## fail if lean (no-default-features) runtime pulls CLI/TUI crates
	@CARGO='$(CARGO)' sh scripts/dep-gate.sh lean-runtime '$(LEAN_FORBIDDEN)' -p entanglement-runtime --no-default-features
	$(CARGO) clippy -p entanglement-runtime --no-default-features --all-targets -- -D warnings

# Self-test for the shared dep-gate mechanism (stubbed cargo; proves the #207
# vacuous-pass fix). Fast, no build — kept out of `verify` (the real gates cover
# CI) but available on demand.
test-gates: ## run scripts/dep-gate.test.sh (dep-gate self-test)
	@sh scripts/dep-gate.test.sh

verify: check-fmt tree check-lean lint test ## full CI-equivalent gate locally

# Release gate (issue #107): workspace line coverage via cargo-llvm-cov. Fails
# below COV_MIN, writes lcov.info + a Cobertura XML for artifact upload / badges.
# Install the tool locally with: cargo install cargo-llvm-cov --locked
coverage: ## cargo llvm-cov --workspace, fail under COV_MIN%
	$(CARGO) llvm-cov --no-report --workspace
	$(CARGO) llvm-cov report --lcov --output-path lcov.info
	$(CARGO) llvm-cov report --cobertura --output-path cobertura.xml
	$(CARGO) llvm-cov report --fail-under-lines $(COV_MIN)

clean: ## cargo clean
	$(CARGO) clean

# Release tagging (issue #362). Refuses on a dirty tree or a red `make verify`
# so tagging stays a single trustworthy command; does NOT push — that's a
# separate, explicit `git push origin $(VERSION)` (see docs/releasing.md).
tag: verify ## cut a release tag (VERSION=v0.1.0 make tag): refuses dirty tree / red verify, does not push
	@test -n "$(VERSION)" || (echo "tag: VERSION=vX.Y.Z is required, e.g. 'make tag VERSION=v0.1.0'" >&2; exit 1)
	@git diff --quiet && git diff --cached --quiet || (echo "tag: working tree is dirty, commit or stash first" >&2; exit 1)
	@case "$(VERSION)" in v[0-9]*.[0-9]*.[0-9]*) ;; *) echo "tag: VERSION must look like vX.Y.Z, got '$(VERSION)'" >&2; exit 1 ;; esac
	@pkg_version=$$($(CARGO) metadata --no-deps --format-version 1 | grep -o '"version":"[^"]*"' | head -1 | cut -d'"' -f4); \
	if [ "$(VERSION)" != "v$$pkg_version" ]; then \
		echo "tag: VERSION=$(VERSION) does not match workspace.package.version $$pkg_version (bump Cargo.toml first)" >&2; exit 1; \
	fi
	git tag -a "$(VERSION)" -m "$(VERSION)"
	@echo "Tagged $(VERSION). Push with: git push origin $(VERSION)"
