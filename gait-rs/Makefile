# Install location — override via Makefile.local (gitignored) or on the command line:
#   echo 'INSTALL_DIR = $(HOME)/develop' > Makefile.local
-include Makefile.local
INSTALL_DIR ?= /usr/local/bin
MAN_DIR     ?= $(INSTALL_DIR)/../share/man/man1

.PHONY: build install man manual fmt lint test check hooks

build:
	cargo build --release -p gait-bin -p gait-mangen

TARGET_DIR := $(shell cargo metadata --no-deps --format-version 1 2>/dev/null | python3 -c "import sys,json; print(json.load(sys.stdin)['target_directory'])")

manual:
	GAIT_MANUAL_UPDATE=1 cargo run -p gait-mangen -- $(TARGET_DIR)/man gait-manual/SKILL.md

man:
	@mkdir -p $(TARGET_DIR)/man
	$(TARGET_DIR)/release/gait-mangen $(TARGET_DIR)/man gait-manual/SKILL.md
	@echo "man pages → $(TARGET_DIR)/man/"

install: build man
	install -d $(INSTALL_DIR)
	install -m 0755 $(TARGET_DIR)/release/gait $(INSTALL_DIR)/gait
	@echo "installed → $(INSTALL_DIR)/gait"
	install -d $(MAN_DIR)
	install -m 0644 $(TARGET_DIR)/man/*.1 $(MAN_DIR)/
	@echo "man pages → $(MAN_DIR)/"

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
