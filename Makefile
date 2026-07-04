.DEFAULT_GOAL := help
CARGO ?= cargo
PKG ?= 

## ---------- targets ----------
.PHONY: help build run run-json test test-unit test-integration lint fmt check-fmt verify clean check tree

help: ## show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | awk 'BEGIN{FS=":.*?## "}{printf "  \033[36m%-15s\033[0m %s\n", $$1, $$2}'

build: ## cargo build --workspace
	$(CARGO) build --workspace

run: build ## build + run the stdio head once (one dummy turn)
	$(CARGO) run -p brain-stdio -- run "Hello, brain!" $(ARGS)

run-json: build ## stream one turn as NDJSON events (like opencode run --format json)
	$(CARGO) run -p brain-stdio -- run --format json "Hello, brain!"

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

# Hygiene gate from PLAN.md: brain-core must pull in zero UI deps.
tree: ## cargo tree for brain-core (verify no clap/crossterm/tonic leak in)
	$(CARGO) tree -p brain-core

verify: check-fmt lint test ## full CI-equivalent gate locally

clean: ## cargo clean
	$(CARGO) clean
