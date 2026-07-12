.DEFAULT_GOAL := help
CARGO ?= cargo
PKG ?= 

## ---------- targets ----------
.PHONY: help build run run-json run-tui pipe sessions inspect test test-unit test-integration lint fmt check-fmt verify clean check tree check-lean coverage

# Minimum line-coverage % the release gate enforces. First measured baseline
# (issue #107) was 65% workspace lines; floor set just under it to absorb CI
# variance. Ratchet up as coverage improves — never lower it.
COV_MIN ?= 60

help: ## show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | awk 'BEGIN{FS=":.*?## "}{printf "  \033[36m%-15s\033[0m %s\n", $$1, $$2}'

build: ## cargo build --workspace
	$(CARGO) build --workspace

run: build ## build + run the stdio head once (one dummy turn)
	$(CARGO) run -p entanglement-runtime -- run "Hello, Holly!" $(ARGS)

run-json: build ## stream one turn as NDJSON events (like opencode run --format json)
	$(CARGO) run -p entanglement-runtime -- run --format json "Hello, Holly!"

run-tui: build ## launch the terminal UI
	$(CARGO) run -p entanglement-runtime -- tui

pipe: build ## stdio pipe head — InMsg NDJSON on stdin, OutEvent NDJSON on stdout
	$(CARGO) run -p entanglement-runtime -- pipe

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

# Hygiene gate (ADR-0006): entanglement-core must pull in zero UI/transport crates.
# Grep for forbidden names followed by a version tag as `cargo tree` prints them.
tree: ## fail if entanglement-core pulls a forbidden UI/transport crate
	@out=$$($(CARGO) tree -p entanglement-core 2>/dev/null); \
	if echo "$$out" | grep -Ei '(clap|axum|tower|tonic|crossterm|ratatui|reqwest|hyper) v[0-9]'; then \
		echo "FAIL: forbidden crate leaked into entanglement-core (see ADR-0006)"; exit 1; \
	else \
		echo "entanglement-core deps clean: no UI/transport crates"; \
	fi

# Lean gate (ADR-0025): entanglement-runtime with --no-default-features must
# stay free of CLI/TUI/transport crates so library consumers get a light build.
check-lean: ## fail if lean (no-default-features) runtime pulls CLI/TUI/transport crates
	@out=$$($(CARGO) tree -p entanglement-runtime --no-default-features -e normal 2>/dev/null); \
	if echo "$$out" | grep -Ei '(clap|ratatui|crossterm|syntect|pulldown-cmark|diffy|reqwest|hyper|tracing-subscriber) v[0-9]'; then \
		echo "FAIL: heavy crate leaked into lean entanglement-runtime (see ADR-0025)"; exit 1; \
	else \
		echo "lean entanglement-runtime deps clean"; \
	fi
	$(CARGO) clippy -p entanglement-runtime --no-default-features --all-targets -- -D warnings

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
