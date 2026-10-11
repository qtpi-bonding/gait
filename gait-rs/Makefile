# Install location — override via Makefile.local (gitignored) or on the command line:
#   echo 'INSTALL_DIR = $(HOME)/develop' > Makefile.local
-include Makefile.local
INSTALL_DIR ?= /usr/local/bin

.PHONY: build install fmt lint test manual check check-min hooks

build:
	cargo build --release -p gait-bin

TARGET_DIR := $(shell cargo metadata --no-deps --format-version 1 2>/dev/null | python3 -c "import sys,json; print(json.load(sys.stdin)['target_directory'])")

install: build
	install -d $(INSTALL_DIR)
	install -m 0755 $(TARGET_DIR)/release/gait $(INSTALL_DIR)/gait
	@echo "installed → $(INSTALL_DIR)/gait"

fmt:
	cargo fmt --all

lint:
	cargo fmt --all -- --check
	cargo clippy --workspace --all-targets -- -D warnings
	@if cargo tree -p gait-acp -e normal | rg -q 'rmcp|gait-codemode'; then \
		echo "gait-acp: mcp/code-mode dependencies present (ok)"; \
	else \
		echo "gait-acp is missing rmcp/gait-codemode — mcp/code-mode features on its gait-core dep were dropped" >&2; \
		exit 1; \
	fi

test:
	cargo test --workspace

# Regenerate docs/manual.md (the `committed_manual_is_current` test checks it).
manual:
	GAIT_UPDATE_MANUAL=1 cargo test -p gait-bin committed_manual_is_current

# Run the same checks the pre-commit hook enforces.
check: lint test

# gait-min composition check: formatting, Min feature tests, a Min binary
# build, and a hard dependency-absence assertion. Uses an explicit `if`
# (not `&&`/`||`) so a broken `rg`/pipe fails loudly instead of silently
# reporting success, and checks `-e normal` (excludes dev-dependency edges,
# so it reflects what actually ships in the binary) for the full excluded
# package list, including `wasmtime` alongside the crates that pull it in.
check-min:
	cargo fmt --check
	cargo clippy -p gait-bin --no-default-features --features min --all-targets -- -D warnings
	cargo test -p gait-bin --no-default-features --features min
	cargo build -p gait-bin --no-default-features --features min --bin gait-min
	@if cargo tree -p gait-bin -e normal --no-default-features --features min | rg -q 'gait-workflow|gait-acp|termimad|syntect|termbg|rmcp|wasmtime'; then \
		echo "min composition pulled in an excluded dependency" >&2; \
		exit 1; \
	fi

# Activate the repo's git hooks (fmt + clippy on commit) for this clone.
hooks:
	git -C .. config core.hooksPath .githooks
	@echo "git hooks activated → .githooks (fmt + clippy on commit)"
