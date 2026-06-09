INSTALL_DIR := /Users/aicoder/develop
TARGET_DIR  := /Users/aicoder/develop/cargo-target

.PHONY: build install

build:
	CARGO_TARGET_DIR=$(TARGET_DIR) cargo build --release

install: build
	cp $(TARGET_DIR)/release/gait $(INSTALL_DIR)/gait
	@echo "installed → $(INSTALL_DIR)/gait"
