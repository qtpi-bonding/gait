# Install location — override via Makefile.local (gitignored) or on the command line:
#   echo 'INSTALL_DIR = $(HOME)/develop' > Makefile.local
-include Makefile.local
INSTALL_DIR ?= /usr/local/bin

.PHONY: build install fmt lint test check hooks

build:
	# Keep the installed single binary capable of running Gait's in-process
	# local GGUF provider. Developers can still make a lean cloud-only build
	# directly with `cargo build --no-default-features` when they need one.
	cargo build --release -p gait-bin --features local

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

test:
	cargo test --workspace

# Run the same checks the pre-commit hook enforces.
check: lint test

# Activate the repo's git hooks (fmt + clippy on commit) for this clone.
hooks:
	git -C .. config core.hooksPath .githooks
	@echo "git hooks activated → .githooks (fmt + clippy on commit)"
