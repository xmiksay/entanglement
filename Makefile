.DEFAULT_GOAL := help
CARGO ?= cargo
PKG ?= 

## ---------- targets ----------
.PHONY: help build run run-json run-tui test test-unit test-integration lint fmt check-fmt verify clean check tree

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

verify: check-fmt tree lint test ## full CI-equivalent gate locally

clean: ## cargo clean
	$(CARGO) clean
