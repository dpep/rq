# rq — build / install / test helpers.
#
#   make            - same as `make help`
#   make build      - dev build      → ./target/debug/rq
#   make release    - optimized build → ./target/release/rq
#   make install    - cargo install --path . (into ~/.cargo/bin)
#   make uninstall  - cargo uninstall rq
#   make test       - cargo test
#   make dogfood    - run rq on its own source (Q=<query>); reproducible
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
.PHONY: help build release install uninstall test dogfood bench lint fmt clean

help:
	@echo "rq targets:"
	@echo "  make build      dev build      → target/debug/$(BIN)"
	@echo "  make release    optimized build → target/release/$(BIN)"
	@echo "  make install    cargo install --path . (→ ~/.cargo/bin)"
	@echo "  make uninstall  cargo uninstall $(BIN)"
	@echo "  make test       cargo test"
	@echo "  make dogfood    run rq on its own source (Q=<query>, ARGS=<flags>)"
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

# Dogfood rq on its own (Rust) source. Reproducible and self-contained: builds,
# fully indexes this repo into a throwaway DB under target/ (never your real
# index), then runs the query. --no-record keeps it side-effect free.
#   make dogfood Q=Store
#   make dogfood Q=index ARGS="--explain --limit 5"
Q        ?= Store
ARGS     ?=
DOGFOOD_DB := $(CURDIR)/target/dogfood.db
dogfood: build
	@rm -f "$(DOGFOOD_DB)" "$(DOGFOOD_DB)-wal" "$(DOGFOOD_DB)-shm"
	@RQ_DB="$(DOGFOOD_DB)" ./target/debug/$(BIN) --index . >/dev/null
	@RQ_DB="$(DOGFOOD_DB)" ./target/debug/$(BIN) $(Q) --no-record $(ARGS)

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
