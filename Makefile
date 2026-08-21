# rq — build / install / test helpers.
#
#   make            - same as `make help`
#   make build      - dev build      → ./target/debug/rq
#   make release    - optimized build → ./target/release/rq
#   make install    - cargo install --path . (into ~/.cargo/bin)
#   make uninstall  - cargo uninstall rq
#   make test       - cargo test
#   make check      - the pre-push gate: fmt + clippy + tests, stop on failure
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
.PHONY: help build release install uninstall test check dogfood bench lint fmt clean

help:
	@echo "rq targets:"
	@echo "  make build      dev build      → target/debug/$(BIN)"
	@echo "  make release    optimized build → target/release/$(BIN)"
	@echo "  make install    cargo install --path . (→ ~/.cargo/bin)"
	@echo "  make uninstall  cargo uninstall $(BIN)"
	@echo "  make test       cargo test"
	@echo "  make check      pre-push gate: fmt + clippy + tests"
	@echo "  make dogfood    run rq on real source (Q=<query>, REPO=<path>, ARGS=<flags>)"
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

# The gate to run before pushing. Lives in a script, not here, because a
# release runs the same one — and because a shell pipeline's exit status is its
# last command's, which is how a filtered `cargo clippy` reports success while
# failing.
check:
	@script/check.sh

# The repo to index, for both dogfood and bench. rq's own source is Rust and
# small; ranking problems — ambiguity, same-name collisions — only really show
# up on someone else's code at scale.
REPO     ?= .

# Dogfood rq on real source. Reproducible and self-contained: builds, fully
# indexes REPO into a throwaway DB under target/ (never your real index), then
# runs the query. --no-record keeps it side-effect free.
#   make dogfood Q=Store
#   make dogfood Q=index ARGS="--explain --limit 5"
#   make dogfood REPO=~/code/lib/ruby/rails Q=Middleware
# The query runs *from inside* REPO: search is scoped to the cwd's repo, and
# being in it is also what earns the current-repo boost, so this ranks the way
# a real search there would. Indexing a large repo takes a while, every run.
Q        ?= Store
ARGS     ?=
DOGFOOD_DB := $(CURDIR)/target/dogfood.db
dogfood: build
	@rm -f "$(DOGFOOD_DB)" "$(DOGFOOD_DB)-wal" "$(DOGFOOD_DB)-shm"
	@RQ_DB="$(DOGFOOD_DB)" ./target/debug/$(BIN) --index "$(REPO)" >/dev/null
	@cd "$(REPO)" && RQ_DB="$(DOGFOOD_DB)" $(CURDIR)/target/debug/$(BIN) $(Q) --no-record $(ARGS)

# The benchmark is an #[ignore]d test inside the lib, not an example: an example
# is a separate crate, and reaching index/search/store from one meant publishing
# all three. --nocapture because its output *is* the result.
bench:
	RQ_BENCH_REPO="$(REPO)" $(CARGO) test --release search_latency -- --ignored --nocapture

lint:
	$(CARGO) fmt --check
	$(CARGO) clippy --all-targets -- -D warnings

fmt:
	$(CARGO) fmt

clean:
	$(CARGO) clean
