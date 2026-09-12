# Targets carry a `##` comment; `help` scrapes them, so a new target
# documents itself by existing.

CARGO ?= cargo
VERSION := $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)

.DEFAULT_GOAL := help
.PHONY: help build release run install test bench fmt lint check clean tag

help: ## Show this help
	@printf '\033[1mttmux\033[0m — make targets\n\n'
	@grep -hE '^[a-z][a-z-]*:.*##' $(MAKEFILE_LIST) \
	  | sort \
	  | awk -F':.*##' '{printf "  \033[36m%-10s\033[0m %s\n", $$1, $$2}'
	@printf '\n'

build: ## Debug build
	$(CARGO) build

release: ## Optimised build
	$(CARGO) build --release

run: ## Build and run ttmux
	$(CARGO) run

install: ## Install ttmux into ~/.cargo/bin
	$(CARGO) install --path .

test: ## Run the whole test suite
	$(CARGO) test --all-targets

bench: ## Print throughput from an optimised build
	$(CARGO) test --release --test perf -- --nocapture --test-threads 1

fmt: ## Format the tree
	$(CARGO) fmt

lint: ## Clippy, warnings are errors
	$(CARGO) clippy --all-targets -- -D warnings

check: ## What CI runs: formatting, lints, tests
	$(CARGO) fmt --check
	$(CARGO) clippy --all-targets -- -D warnings
	$(CARGO) test --all-targets

clean: ## Remove build artifacts
	$(CARGO) clean

# Pushing the tag is what starts .github/workflows/release.yml.
tag: check ## Tag the version in Cargo.toml and push it, which cuts a release
	git tag -a v$(VERSION) -m 'ttmux v$(VERSION)'
	git push origin v$(VERSION)
