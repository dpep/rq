# rq — build / install / test helpers.
#
#   make            - same as `make help`
#   make build      - dev build      → ./target/debug/rq
#   make release    - optimized build → ./target/release/rq
#   make install    - cargo install --path . (into ~/.cargo/bin)
#   make uninstall  - cargo uninstall rq
#   make test       - cargo test
#   make check      - the pre-push gate: fmt + clippy + tests + docs, stop on failure
#   make dogfood    - run rq on its own source (Q=<query>); reproducible
#   make bench      - search-latency benchmark over REPO (default: .)
#   make recall     - fuzzy-ranking recall on pinned Ruby + Rust corpora (BASE=<ref>)
#   make fuzz       - name index vs scorer on many names and every recall query (N=, SEED=)
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
.PHONY: help build release install uninstall test check dogfood bench recall fuzz lint fmt clean

help:
	@echo "rq targets:"
	@echo "  make build      dev build      → target/debug/$(BIN)"
	@echo "  make release    optimized build → target/release/$(BIN)"
	@echo "  make install    cargo install --path . (→ ~/.cargo/bin)"
	@echo "  make uninstall  cargo uninstall $(BIN)"
	@echo "  make test       cargo test"
	@echo "  make check      pre-push gate: fmt + clippy + tests + docs"
	@echo "  make dogfood    run rq on real source (Q=<query>, REPO=<path>, ARGS=<flags>)"
	@echo "  make bench      search-latency benchmark (REPO=. by default)"
	@echo "  make recall     fuzzy-ranking recall on pinned corpora (BASE=<ref>, ARGS=<flags>)"
	@echo "  make fuzz       name index vs scorer on random names (N=<names>, SEED=<n>)"
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
# runs the query.
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
	@cd "$(REPO)" && RQ_DB="$(DOGFOOD_DB)" $(CURDIR)/target/debug/$(BIN) $(Q) $(ARGS)

# The benchmark is an #[ignore]d test inside the lib, not an example: an example
# is a separate crate, and reaching index/search/store from one meant publishing
# all three. --nocapture because its output *is* the result.
bench:
	RQ_BENCH_REPO="$(REPO)" $(CARGO) test --release search_latency -- --ignored --nocapture

# Fuzzy-ranking recall: where the name each query was derived from ranks, over
# pinned Ruby and Rust corpora (docs/RECALL.md). BASE builds a git ref
# and lists the sources that lost #1 or the top 10 against it. Not part of
# `check` or CI: the corpora are fetched once into ~/.cache/rq-recall, and each
# binary takes about a minute.
#   make recall
#   make recall BASE=main
#   make recall BASE=HEAD~1 ARGS="--fail-on-loss"
BASE ?=
recall: release
	@CARGO="$(CARGO)" script/recall.py $(if $(BASE),--base $(BASE)) $(ARGS)

# The name index's property tests, large and in release. `cargo test` runs
# them small: the random names with a fixed seed, the recall queries sampled.
# This picks a fresh seed unless SEED is given, and prints it so a failure can
# be replayed, and sweeps every recall query.
N    ?= 20000
SEED ?= $(shell od -An -N6 -tu8 /dev/urandom | tr -d ' ')
fuzz:
	@echo "fuzz: $(N) names, SEED=$(SEED), every recall query"
	@RQ_FUZZ_NAMES=$(N) RQ_FUZZ_SEED=$(SEED) RQ_FUZZ_STRIDE=1 $(CARGO) test --release --lib \
		the_index_takes_exactly_what_score_accepts

lint:
	$(CARGO) fmt --check
	$(CARGO) clippy --all-targets -- -D warnings

fmt:
	$(CARGO) fmt

clean:
	$(CARGO) clean
