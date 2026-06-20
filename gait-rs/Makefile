# Install location — override on the command line, e.g.
#   make install INSTALL_DIR=$$HOME/.local/bin
INSTALL_DIR ?= /usr/local/bin

.PHONY: build install

build:
	cargo build --release -p gait-bin

install: build
	install -d $(INSTALL_DIR)
	install -m 0755 target/release/gait $(INSTALL_DIR)/gait
	@echo "installed → $(INSTALL_DIR)/gait"
