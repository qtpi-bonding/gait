# Install location — override on the command line, e.g.
#   make install INSTALL_DIR=$$HOME/.local/bin
INSTALL_DIR ?= /usr/local/bin

.PHONY: build install fmt lint test check hooks

build:
	cargo build --release -p gait-bin

install: build
	install -d $(INSTALL_DIR)
	install -m 0755 target/release/gait $(INSTALL_DIR)/gait
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
