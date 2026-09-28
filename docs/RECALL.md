# Fuzzy-ranking recall

`make recall` measures what a ranking change does to fuzzy queries on real code.
Unit tests and fixtures pin individual orderings; this measures the aggregate
over thousands of queries, where a gain on one kind of query and a loss on another
show up side by side. D12, D14 and D15 in [DECISIONS.md](DECISIONS.md) cite its
numbers.

```sh
make recall                          # target/release/rq on its own
make recall BASE=main                # against a build of any git ref
make recall BASE=main ARGS=--json    # the same, for a machine
script/recall.py --help              # every flag; make passes them through ARGS
```

It is **not** part of `cargo test`, `make check` or CI. It needs the network once
(to fetch the corpora) and takes about a minute per binary.
`--queries FILE` runs another query set in the same format (only its repos are
indexed), and `--misses` lists every query whose source isn't #1. Run it by hand for any
change to scoring, recall or the match chain, and put its numbers in the
decision that records the change.

## What it measures

Every query was derived from a real symbol name, and that name, the query's
**source**, is its ground truth. For each query the report says whether the
source ranks **#1**, in the **top 10**, or is **found** at all (`--limit 0`).
Rank is by name, so any definition of the source counts.

6,879 queries over six corpora (plus the regress cases below, over four more),
reported per language and per repo as well as in total. A ranking change has to hold up on each language, not just on the
mixed total.

- **Ruby**: rails and discourse, 2,372 queries. 58 are hand-picked (no source,
  but they count toward churn) and 2,314 are derived from 440 randomly sampled
  names of five or more letters.
- **Rust**: tokio and ripgrep, sampled at random (250 and 150 names), and rq and
  trekr, 150 names each drawn from those the last 300 commits before the pin
  added or changed. Those last two are the dogfood set: definitions a developer
  on this project actually went looking for. 4,507 queries.

Each sampled Ruby name yields up to six queries, one per fuzzy **type**:

| type | recipe | `test_float_limits` → |
|---|---|---|
| `abbr3` | first 3 letters of each word | `tesflolim` |
| `abbr2` | first 2 letters of each word | `teflli` |
| `first+last` | first 3 letters of the first word + the last word | `teslimits` |
| `consonants` | first letter, then consonants, 6 letters | `tstflt` |
| `typo` | one adjacent transposition | `test_floa_tlimits` |
| `glob` | `first3*last3` | `tes*lim` |

A derived query other than a glob is kept only if no name in its repo starts
with it, so every one reaches the fuzzy layers. Rust names also get three
literal types:

| type | recipe | example |
|---|---|---|
| `exact` | the name as written | `JoinHandle` |
| `case` | the other convention: snake to camelCase, Camel to snake | `join_handle` |
| `qualified` | `Owner::name`, for a method or associated fn | `Runtime::block_on` |

Rust ground truth is read from source with a regex, not from rq's index, so a
definition rq fails to extract is still asked for and counts as a miss. `exact`
therefore measures extraction coverage as much as ranking. Names defined only
in tests, benches or examples are left out.

The set lives in [`script/recall/queries.tsv`](../script/recall/queries.tsv)
(`repo`, `query`, `type`, `source`).
[`derive.py`](../script/recall/derive.py) generates the Rust rows
(`sample` for tokio and ripgrep, `touched --rev <pin>` for rq and trekr, seed 1).
The Ruby rows came from an earlier, uncommitted version of the same recipes.

## Regress cases

[`script/recall/regress.tsv`](../script/recall/regress.tsv) holds hand-written cases
that have one right answer. Each row gives a query and the `file:line name` its #1
must match (a regex). With a leading `!`, the #1 must *not* match it. They come
from the language testers (Go on hugo and gin, Python on django, TypeScript on
excalidraw, Rust on tokio and ripgrep) and from Rust extraction gaps
(`JoinHandle`, `TryRecvError::Empty`, `select`, `block_on`), and fields in four
languages (`Hit.also_in`, `AppState.zenModeEnabled`, D48). Every run reports how
many hold #1 per binary, and lists each case that isn't #1 or changed. A case that
held and no longer does fails `--fail-on-loss`. The four corpora only this set uses
are pinned like the rest and indexed only when it runs. `--no-regress` skips them.

## Reading the report

- **Headline**: source #1 / top 10 / found, counts and share of the 2,314
  sourced queries, plus index and query time for each binary.
- **Per type**: #1 and top 10 by query type. A change aimed at typos should move
  the `typo` row and leave the others alone.
- **With a baseline**: how many sources moved up and down, how many queries'
  top 10 changed at all, and every source that **lost #1** or **lost the top
  10**, with what took #1 instead.

A loss is a question, not a verdict. Some queries are genuinely ambiguous
(`teswri` fits `test_write` and `test_writer`), and some losses are a
deliberate rule doing its job (D12's test-path cases). Read each one; record
the ones you accept in the decision.

To compare two settings of one binary, such as an environment variable against the
default, pass `--base-bin` and `--bin` a two-line `sh` wrapper that sets the variable and `exec`s
rq. Wrap both sides: the wrapper's own start adds ~2 ms to `wall`, though not to `query`
or `first answer`, which rq measures itself.

`--fail-on-loss` exits 1 if any source lost #1 or the top 10, for a change that
is meant to be a pure gain. `--bench REPS` adds latency: the hand-picked queries
through every binary, interleaved and rotated per rep so machine load lands on
each equally, reported as the median and p90 of per-query medians. `--json`
prints the same report as one object (`corpus`, `runs[]` with `by_type`, `diff`
with `lost_first`/`lost_top10`, `bench`); its field names are stable.

## Anchored call sites

`--anchored` adds a second measurement, of `--anchor` (D18), on the binary
under test only. [`script/recall/anchored.tsv`](../script/recall/anchored.tsv)
holds 446 Ruby call sites (190 rails, 256 discourse): the query is the called
name, the anchor is the call site, and the truth is the one definition
`trekr --def` resolves it to at confidence 0.9 or more. Only names defined at
least twice are sampled, so there is always something to rank. Rank is by
location, not name. Each query runs plain and anchored, reported overall, by
receiver kind, and by whether the truth sits in the anchor's own file.
[`derive_anchored.py`](../script/recall/derive_anchored.py) regenerates the set
(needs `trekr`); moving a corpus pin means rerunning it.

## Reproducibility

- **Pinned corpora.** Each corpus is pinned at a commit in
  [`script/recall/corpus.json`](../script/recall/corpus.json), shallow-fetched
  once into `~/.cache/rq-recall` (`$RQ_RECALL_CACHE` or `--cache` to move it),
  never vendored. The query set was derived at those commits, so moving a pin
  means regenerating the queries and re-baselining.
- **Frozen recency.** Recency decays against the wall clock, so real file and
  commit dates would drift the numbers week to week. Each checkout is one commit
  on `main` dated 2000-01-01, with every file's mtime set to match, which zeroes
  the signal. `origin` still points upstream, so the repo identity is the real
  one.
- **Deterministic index.** Each binary indexes into its own throwaway `RQ_DB`
  (never your real index) with one parse worker. Parallel workers commit files in
  a different order every run, and rowid order breaks score ties and decides
  where a capped net truncates. A binary diffed against itself shows no change.
- **Baselines** are built from `git archive` of the ref and cached by commit
  under `target/recall/`.

The D12 tables were measured on local clones at these commits, with their real
dates. The pinned harness reproduces them to within two queries per cell:

| | source #1 | top 10 | found |
|---|---|---|---|
| before D14 (`05f93c8`) | 915 (39.5%) | 1,346 (58.2%) | 1,566 (67.7%) |
| D14 + D15 + window (0.52.2) | 1,134 (49.0%) | 1,604 (69.3%) | 1,887 (81.5%) |

Pinned-harness numbers since, from D23 and D24 (D26 removed the `RQ_RECALL` switch and
the FTS path, with no answer changed):

| | source #1 | top 10 | found |
|---|---|---|---|
| name index, before D24 (`RQ_RECALL=scan`) | 1,218 (52.6%) | 1,677 (72.5%) | 1,991 (86.0%) |
| D24, the name index as default | 1,304 (56.4%) | 1,750 (75.6%) | 2,000 (86.4%) |
| D24 with `RQ_RECALL=fts` | 1,207 (52.2%) | 1,648 (71.2%) | 1,894 (81.8%) |

### Rust baseline

Rust entered the harness at 0.55.1 (`402b389`). Before this, Rust ranking had
never been measured.

| | source #1 | top 10 | found |
|---|---|---|---|
| rust, all four corpora | 3,558 (78.9%) | 3,848 (85.4%) | 3,867 (85.8%) |
| tokio | 1,190 (67.7%) | 1,312 (74.6%) | 1,321 (75.1%) |
| ripgrep | 806 (81.3%) | 867 (87.5%) | 869 (87.7%) |
| rq | 826 (90.3%) | 866 (94.6%) | 867 (94.8%) |
| trekr | 736 (87.3%) | 803 (95.3%) | 810 (96.1%) |

After Rust extraction and D32–D33 (against main at `a406932`, whose Rust numbers
equal the baseline):

| | source #1 | top 10 | found |
|---|---|---|---|
| rust, all four corpora | 3,658 (81.2%) | 4,027 (89.3%) | 4,055 (90.0%) |
| tokio | 1,285 (73.1%) | 1,472 (83.7%) | 1,489 (84.7%) |
| ripgrep | 805 (81.2%) | 883 (89.1%) | 885 (89.3%) |
| rq | 831 (90.8%) | 869 (95.0%) | 871 (95.2%) |
| trekr | 737 (87.4%) | 803 (95.3%) | 810 (96.1%) |

Ruby is unchanged at 1,304 / 1,750 / 2,000. The regress cases go from 30 to 33 of
36.

tokio's baseline `found` rate was low because definitions were missing from the index,
not because they ranked badly. Items inside `cfg_*! { … }` blocks,
`macro_rules!`, enum variants and `type` aliases were not extracted.
