# rq — build / install / test helpers.
#
#   make            - same as `make help`
#   make build      - dev build      → ./target/debug/rq
#   make release    - optimized build → ./target/release/rq
#   make install    - cargo install --path . (into ~/.cargo/bin)
#   make uninstall  - cargo uninstall rq
#   make test       - cargo test
#   make bench      - search-latency benchmark over REPO (default: .)
#   make lint       - cargo fmt --check && cargo clippy (warnings = errors)
#   make fmt        - cargo fmt
#   make clean      - cargo clean
#
# Note: this machine's cargo came via Homebrew's keg-only rustup and may not be
# on PATH. Either add it (see CLAUDE.md) or run, e.g.:
#   make build CARGO=/opt/homebrew/opt/rustup/bin/cargo

CARGO ?= cargo
BIN   := rq

.DEFAULT_GOAL := help
.PHONY: help build release install uninstall test lint fmt clean

help:
	@echo "rq targets:"
	@echo "  make build      dev build      → target/debug/$(BIN)"
	@echo "  make release    optimized build → target/release/$(BIN)"
	@echo "  make install    cargo install --path . (→ ~/.cargo/bin)"
	@echo "  make uninstall  cargo uninstall $(BIN)"
	@echo "  make test       cargo test"
	@echo "  make bench      search-latency benchmark (REPO=. by default)"
	@echo "  make lint       cargo fmt --check && cargo clippy"
	@echo "  make fmt        cargo fmt"
	@echo "  make clean      cargo clean"

build:
	$(CARGO) build

release:
	$(CARGO) build --release

install:
	$(CARGO) install --path .

uninstall:
	$(CARGO) uninstall $(BIN)

test:
	$(CARGO) test

REPO ?= .
bench:
	$(CARGO) run --release --example bench -- $(REPO)

lint:
	$(CARGO) fmt --check
	$(CARGO) clippy --all-targets -- -D warnings

fmt:
	$(CARGO) fmt

clean:
	$(CARGO) clean
