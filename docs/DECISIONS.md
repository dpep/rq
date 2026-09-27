# Decisions

ADR-lite, borrowed from `rwr`. Each entry records what was decided, why, and **what would
reverse it** — so a rejected idea comes back with new evidence rather than fresh
confidence.

Performance entries state the number they were measured at. A decision without a
measurement is an opinion with a date on it.

---

## D1 — Two-phase candidate fetch: rejected on architecture

**Rejected**, 2026-08-21.

Recall hands the scorer up to `CANDIDATE_LIMIT` (8,000) fully-hydrated rows, and the
name-match chain rejects ~99.9% of them. Fetching a lite row (id, name, kind, parent),
running the name gate, and hydrating only survivors would save ~6 ms on a query with no
exact match.

Rejected because the seam falls in the wrong place. Today `score()` is one function over
one complete `SymbolRow` producing one `Vec<Feature>`, and adding a ranking signal is a
single `features.push(...)` — four were added in one day. Splitting it means every future
signal must first answer "which phase am I in?", and a signal needing a field the lite row
lacks either widens the lite row (erasing the win) or forfeits the early exit. That is a
permanent toll on the highest-traffic edit in the codebase, to buy 6 ms inside a budget we
are already comfortably within (~15 ms worst case against a 50 ms first-answer target).

The boundary would also be drawn by what SQLite charges rather than by what the domain
means, which is the kind of seam that reads as arbitrary once the storage changes. It
would additionally complicate the `--explain` guarantee, which is a stated first
principle rather than an implementation detail.

*Reverses if:* the first-answer budget is missed in normal use — not on a synthetic
worst case — or `--usage` shows no-exact-match queries are common enough that their cost
is the typical experience rather than the tail.

## D2 — Slimming candidate rows: deferred, not measurable yet

**Deferred**, 2026-08-21.

`language` and `repo_identity` are never read during scoring (`score.rs` mentions them
only in comments). Both are needed solely for rows that become results — `language` for
the `--lang` post-filter, `repo_identity` for display and the declaration-collapse key —
so recall fetches two columns for 8,000 candidates to use them on ten.

Removing the `identity` column and its `repositories` join is a contained change that
doesn't move any boundary, and on that basis it is defensible regardless of speed. It was
not adopted because the win could not be demonstrated: candidate materialization costs
~1 µs/row (measured by slope: 2,000 rows → 3 ms, 4,000 → 5 ms, 8,000 → 9 ms), so one of
six string columns is worth roughly 1 ms — inside the noise of a 10 ms measurement, and
well inside it on a machine that turned out to have another process on five cores.

An earlier note recorded this as "measured at zero". That was wrong: *unmeasurable* and
*zero* are different claims, and only the first is supported.

*Reverses if:* someone wants it for clarity rather than speed — the argument that ranking
data and display data are different things stands on its own — or a quiet-machine
benchmark resolves the ~1 ms.

## D3 — Trigram recall: inside budget, stop

**Closed**, 2026-08-21.

The trigram FTS pass costs ~9 ms on a query with no exact match. Warm, the SQL itself is
~3 ms; the rest is materializing rows. Three levers were measured and none paid: dropping
the `repositories` join (nothing, warm — the 35 ms it appeared to cost was cold cache),
dropping the identity column (see D2), and the two-phase fetch (see D1).

Crucially the cost **does not grow with the corpus**: rows fetched are capped at 8,000
whatever the index size, so a monorepo pays the same ceiling Rails does. What rises with
scale is how often a query reaches the cap, not the cost of reaching it. This is the
opposite of the `LIKE`-scan defect fixed in 0.47.0, which scaled with the whole symbol
table.

*Reverses if:* the FTS search half — the ~3 ms that *does* grow with corpus size — starts
to dominate on a much larger index.

## D4 — `documented` ranking signal: probed, not built

**Deferred**, 2026-08-21.

A definition preceded by a comment block is more likely to be the canonical one. Probed
on Rails: `ActiveSupport::Inflector` carries 12 doc lines against `Rails::Autoloaders::Inflector`'s
zero, and prevalence sits in the useful band — 21% of definitions in Rails `lib/`, 47% in
rq's own Rust. Ruby's `# :nodoc:`, which would invert the signal, is written trailing on
the definition line 1,674 times and on its own line 3 times, so a "comment on the line
above" rule never sees it.

Not built because its value shrank while the session ran: the three cases that motivated
it (`where`, `delegate`, `redirect_to`) were all fixed by `extent`, leaving two known
cases for the largest change on the list — a `core::Symbol` field, a migration, extraction
in six plugins, and lazy backfill.

*Reverses if:* more ranking failures turn up that separate on documentation and not on
body size, or a plugin needs the field for another reason and the marginal cost drops.

## D5 — Parse-job auto default: one per core, not a cap of 8

**Decided**, 2026-08-24. Supersedes the `clamp(1, 8)` cap and its rationale.

The cap's stated reason was that parsing is CPU-bound "but writes serialize through
one SQLite writer, so flooding every core rarely pays". Measured, that is not what
limits the pass.

Corpus: a GitLab checkout, 38,402 source files → 170,426 symbols (31k Ruby, 7k JS).
Machine: Apple M2, 8 cores, release build, cold index into a throwaway DB. Method:
job counts interleaved round-robin rather than run in blocks, so drift hits every
condition equally; 9 reps per job count, medians of the fused walk+parse+write phase.

| jobs | 1 | 2 | 4 | 6 | 8 | 12 | 16 |
|---|---|---|---|---|---|---|---|
| fused ms | 20105 | 10262 | 7300 | 5354 | 4657 | 5059 | 4451 |
| speedup | 1.00x | 1.96x | 2.75x | 3.76x | 4.32x | 3.97x | 4.52x |
| writes ms | 1030 | 959 | 1135 | 1331 | 1632 | 2272 | 2206 |
| writes as % of phase | 5% | 9% | 16% | 25% | 35% | 45% | 50% |

**Conditions, stated because they qualify the entry.** This is a shared laptop and
another agent was building concurrently for part of the run; sampled 1-minute load
average ranged 2.8–6.0. So the absolute milliseconds are not a clean-room number and
individual cells move by 10–20% between reps. The *slope* survives that: 4→8 jobs is
1.57x at the median, an effect several times the inter-rep spread (IQR at 4 jobs
6332–8238, at 8 jobs 4604–5167 — the two ranges do not overlap). The flat region
above 8 is the weaker claim, and it is recorded as flat-within-noise, not as zero.

**The writer is not the ceiling.** Writes are 5% of the phase at 1 job and 35% at 8 —
a growing *share* of a shrinking total, which is Amdahl arithmetic, not saturation.
Absolute write time rises only 1030→1632 ms across that range, and it rises because
the consumer thread is contending for a core, not because there is more to write (same
files, same symbols, same batch count every run). Writes also *overlap* parsing rather
than queueing behind it — the consumer writes while workers parse — so the phase never
approaches write time as a floor. The curve flattens at 8 because this machine has 8
cores, not because of SQLite. The cap was therefore correct here only by coincidence,
and bound well below the core count on anything larger.

**The field report this came from** — a monorepo user measuring ~25% faster indexing
with jobs raised toward their physical core count — could not be reproduced directly:
this box has 8 cores, so a cap of 8 never bound. The reproducible analogue is lifting a
cap that sits at half the core count, 4→8, which is 36% off the phase (1.57x). Same
shape, same order of magnitude, consistent with the report.

**`available_parallelism()` rather than a physical-core probe.** On Apple Silicon the
two coincide. On SMT x86 it reports logical, roughly 2× physical, and this machine
cannot settle whether that is right: 16 workers on 8 real cores is oversubscription,
not SMT, so it is a pessimistic stand-in — and even that measured flat (8→16 = 1.05x,
inside the spread), so the downside is bounded. Against that, `available_parallelism`
reports the cgroup/affinity budget inside a container, where a physical-core probe
returns the *host's* count and would oversubscribe a 2-CPU CI runner by an order of
magnitude. It is also std, so it costs no dependency. `--jobs`/`RQ_JOBS` still win over
auto, unchanged.

*Reverses if:* an SMT x86 machine measures logical-count workers slower than
physical-count on a large corpus — the one case no measurement here covers — or a
writer change (a second connection, a different journal mode) makes write time rather
than core count the thing that flattens the curve.

*Lead, not chased here:* with `--profile` now covering indexing (same session), a warm
pass on a 40-file repo shows `index: enumerate 5.9ms  git ls-files, 40 path(s)`.
`git_source_candidates` (`src/index/mod.rs`) forks `git ls-files` with one pathspec glob
per registered extension, and that cost is paid before any parsing starts — a plausible
suspect for the startup floor monorepo users report, and now directly measurable rather
than inferred.

## D6 — `symbols(repository_id, name_lower)` index: adopted

**Adopted**, 2026-09-25. Replaces `idx_symbols_repo(repository_id)`, which it subsumes.

Recall is scoped to the current repository in SQL (it used to be filtered afterwards,
which let other repos crowd out the cap). With only `idx_symbols_name_lower`, a scoped
prefix or first-character scan still walks every repo's names and discards the others.

Measured on an index of eight Ruby repos (175,935 symbols) searched from inside rails,
release build, 15 interleaved reps, quiet machine (load ~2.4):

| query | recall before | recall after | first answer before | after |
|---|---|---|---|---|
| `usr` (first-char anchor) | 9.4 ms | 4.7 ms | 11.3 ms | 6.9 ms |
| `Base` | 0.9 | 0.4 | 2.4 | 2.0 |
| `Foo.new` | 19.9 | 18.5 | 25.5 | 24.2 |
| `conpool` (FTS-bound) | 13.5 | 13.9 | 17.4 | 17.6 |

A single-repo index is unchanged (every row shares one `repository_id`). Output was
byte-identical throughout. Cost: store writes on a cold rails index 193 → 235 ms, but
writes overlap parsing so the index total moved 585 → 606 ms (+3.6%); the migration
builds the index once (~0.6 s on 176k symbols); the DB grows ~10%.

The trigram FTS layer can't use it: `MATCH` enumerates every repo's postings and the
scope filters after. Per-repo FTS tables would fix that and are not worth their weight
at this scale.

*Reverses if:* write cost starts to matter more than recall on the short-query path —
e.g. a warm that re-indexes large files continuously — or FTS-bound queries come to
dominate usage so the win lands on too few searches to pay for the writes.

## D7 — Per-search write batching, pre-decode dedup, `PreparedQuery`: rejected

**Rejected**, 2026-09-25. Rails, release build, back-to-back runs.

*Merging the per-search writes into one transaction.* A search writes its usage row
(`events` + `usage_daily`, one transaction) before printing, then rolls up and prunes
after. (Since D10 there is only the `usage_daily` write.) The usage write is 0.4–0.5 ms at the median; rollup + prune together 0.1 ms.
Merging saves at most the post-answer ~0.1 ms. Moving the usage write after the output
would take ~0.45 ms off `first answer` (≈2.0 ms) and nothing off the process's wall
time. It would also split one count across the three exits (`--show`, `--open`,
list). That's imperceptible, and a toll on every future exit path. (Revisited in D13
with a loaded-machine tail, and adopted.)

*The warm child checking "did anything move?" before taking its lock*, so the common
case writes nothing. The search's `record usage` p90 was 2.2–2.6 ms back-to-back with
detached children against 0.5 ms without them, which looked like lock-write contention.
It isn't: with the check ahead of the lock, children stop single-flighting and run
`git status` concurrently. p90 got worse (2.6–2.9 ms) and a 20-search burst took
204 ms to go quiet against 165–174 ms. The tail is CPU contention, which the lock
limits.

*Decoding a recall row only after checking its id against rows already found* (the
layers overlap on paper: exact ⊂ prefix ⊂ first-char). Counted on an 8-repo index:
duplicates are 0% of fetched rows for `usr`, `conpool` and `actconn`, and 14% for
`Foo.new` (constructor lookups). No measurable win.

*A `PreparedQuery` built once per search instead of per candidate.* Sampled over 300
`actconn` searches (6,186 candidates each): of 2,065 samples in `cmd_search`, recall
is ~1,300 and `score` ~590. Query-side work inside `score` (`parse_qualified`,
lowercasing the leaf, `align`'s query `Vec<char>`) is ~45 samples, about 2% or ~0.2 ms.
It would change `score`'s signature, the edit D1 protects, for that. The real cost
inside `score` is `align`'s DP (411 samples), which is candidate-side and can be
worked on without moving any seam.

*Reverses if:* scoring becomes the dominant share of a fuzzy search, or a signal needs
query-derived state expensive enough that computing it per candidate shows up on its
own.

## D8 — `PRAGMA mmap_size`: adopted. Fat LTO + one codegen unit: rejected

**Decided**, 2026-09-25. Release build, 15–25 interleaved reps, load ~3.7.

*mmap (256 MB cap).* Recall materializes thousands of rows on a fuzzy query (D2/D3),
and mmap reads pages in place instead of copying them through `read()`. Byte-identical
output throughout.

| query | index | recall before | after | first answer before | after |
|---|---|---|---|---|---|
| `conpool` | 8 repos, 176k symbols | 12.6 ms | 10.0 | 16.5 | 13.6 |
| `actconn` | 8 repos | 15.8 | 12.3 | 20.2 | 17.0 |
| `Foo.new` | 8 repos | 17.6 | 14.3 | 23.1 | 19.9 |
| `conpool` | rails only | 5.7 | 4.8 | 9.3 | 8.5 |
| `Middleware` (exact) | either | 0.2 | 0.2 | 1.5 | 1.5 |

The exact path is flat within ±0.1 ms. The mapping is shared page cache, not
per-process memory, and the 256 MB cap is above any index measured here (46 MB for 176k
symbols).

*Fat LTO + `codegen-units = 1`.* Clean release build 18–20 s → 48 s; binary 13.1 → 11.9
MB. Search on the 8-repo index moved −0.4 to +0.4 ms (e.g. `actconn` first answer 20.6
→ 20.2, `usr` 6.5 → 6.6); startup (`--version`) 6.5 → 6.6 ms. Nothing to buy with
2.4× the build time.

*Reverses if:* mmap — a platform where mmap I/O errors surface as SIGBUS in practice
(a network filesystem under the DB), or an index large enough that the cap binds and
needs revisiting. LTO — a profile showing cross-crate inlining matters (tree-sitter's
C is already compiled separately and doesn't benefit).

## D9 — Indexer micro-work: not pursued; the parser is the cost

**Deferred**, 2026-09-25. Candidates: lowercasing names on the worker threads rather
than the single writer, dropping per-symbol `file`/`language` `String`s, moving the
incremental walk's mtime `stat` from the walk thread onto the parse workers, and
flattening `align`'s DP table.

Sampled (`sample`, 1 ms) over a cold rails index, top-of-stack, busy samples only:
tree-sitter 80.4%, malloc/free 10.4% (mostly tree-sitter's own trees), filesystem
syscalls 3.9%, SQLite 2.2%, rq's own Rust 0.4%. The first two items live in that 0.4%,
and the writer isn't the ceiling anyway (D5). Removing the walk's stats outright took an
incremental `rq --index` walk from 114 to 88 ms at load ~20, so perhaps ~10 ms of a
quiet ~46 ms walk. Parallelizing would recover part of that, on paths that are explicit
or run in the background. `align`'s DP is ~20% of a fuzzy search's samples, so ~1 ms of
a ~10 ms rails first answer, inside budget and in subtle code.

Adopted instead, from the same investigation: bulk FTS on a cold budgeted warm, where
the per-row trigger was bounding the pass (writes 1,151 ms against 236 ms; first search
in a fresh rails 920 → 343 ms).

*Also noted:* `content_hash` uses `DefaultHasher`, which std doesn't promise is stable
across releases. A toolchain bump changes every stored hash, but hashes are consulted
only after an mtime mismatch (the walk and `refresh_file` both skip on mtime first), so
the cost is re-parsing files that were edited anyway. Not worth a hash dependency.

*Reverses if:* a language plugin's extraction (not its grammar) shows up in a profile, or
an incremental re-index becomes something users wait on interactively.

## D10 — Behavioral learning: deleted

**Deleted**, 2026-09-26. Measured on the author's own database, the one place it had
real traffic, six weeks after 0.40.0 gave the feature its first source of data.

`selection_stats` had **0 rows**. `usage_daily` shows why: 346 of 349 searches came from
Claude Code, and not one used `--show`, `--open`, or `--web`. Human `--open` and editor
`--record` traffic was effectively nil, and no editor hook was ever installed.

The bulk feed couldn't have rescued it either. `--show` records a pick only when the top
hit already clears the 0.85 confidence gate — so by construction it can only confirm the
result static ranking put first, never move a different one up. A signal that can only
agree with the ranker adds nothing to it.

ROADMAP Phase 3 set the rule in advance: an empty table by 2026-10-01 means delete. Gone:
`selection_stats` and the `events` log that fed it (a v13 migration drops both), the
rollup, the `learned` feature, and the `--record` and `--no-record` flags. `usage_daily`
stays, so `--usage` is unchanged, and `search` is now a function of the index, recency,
and the branch.

*Reverses if:* a source of picks appears that isn't already the top hit — an editor
integration where people choose from a list, say — at a volume that can outvote noise.
Rebuild it against that source, and measure before billing it as a feature.

## D11 — Best-first indexing: two tiers, not a priority heap

**Adopted in reduced form**, 2026-09-26. The [PRIORITY_INDEXING.md](PRIORITY_INDEXING.md)
design (shared heap, content / git-recency / neighbor signals, lazy deletion) was
measured against what users hit on a cold repo before anything was built.

*The problem is real above a few thousand files.* A warming search blocks until an
exact or prefix match is committed, and walk order decides when that is. Cold first
search, fresh DB, programmatic (piped), release build, load 15–60 (a shared machine —
the absolute ms are noisy, the ratios are not):

| repo | files | full index | first answer, walk order | with demand tier |
|---|---|---|---|---|
| rails | 3.3k | ~1.6 s | median 420 ms, max 1.2 s (16 queries) | median 140 ms, max 250 ms |
| discourse | 14.4k | ~1.8–2.4 s | median 1.4 s, max 7.0 s (20 queries) | median 245 ms, max 570 ms |

Queries were unique method names sampled across walk-order percentiles, none in their
file's path — path-guided warming already catches class names. Walk-order time tracks
the file's position almost linearly.

*Above the per-pass cap (`COLLECT_CAP`, 50k files) walk order stops answering at all.*
Simulated on discourse with `RQ_COLLECT_CAP=2000` (a repo ~7× one pass): 5 of 6
late-walk queries came back `warming` (exit 2) under walk order, and under a demand
tier bounded by the same cap; with the tier uncapped all 6 answered in 250–380 ms.
That is the monorepo case, and why the tier ignores the cap: the deadline bounds its
reading instead.

*What was built.* A search's warm first streams the candidates through the existing
`stream_walk` with a content needle — the query's leaf name, which any exact or prefix
match must contain — parsing and persisting only files that hold it; the walk-order
pass then skips those. And `BatchWriter` commits at least every 50 ms, since matches
arrive too sparsely to fill a 512-file batch and the poll only sees committed rows.
Cost: one read pass over the repo (discourse ~0.34 s at load 23, against ~11 s of parse
CPU), paid *before* walk order starts, so a query with no literal match — a typo, a
fuzzy abbreviation — waits for it: rails `usr`/`conpool`/`Middlewear`, 5 reps each,
medians +10 to +110 ms. Explicit `rq --index` is unchanged (discourse 2.40 → 2.31 s,
4 reps, within noise).

*Not built:*
- **The heap.** The two signals known before parsing (content match, then path
  resemblance) give a total order of two tiers. A heap earns itself only when
  priorities arrive late and interleave; none do.
- **Git recency.** `git log` over a big repo is itself seconds, and "recently
  committed" is a weak proxy for "what this query names" next to the content match,
  which is exact for every answer a warming search accepts.
- **Neighbor expansion.** It orders what to index *after* the answer, which the
  detached warm child covers in walk order anyway.

*Reverses if:* a repo large enough that the demand read itself overruns the wait budget
(extrapolated at ~40k files/s: ~2M files ≈ 50 s against the 60 s default) — then the
read, not the order, is the cost, and a persisted content index or `git grep` is the
lever; or fuzzy-query waits on a cold huge repo draw complaints, which interleaving the
two tiers would address.

## D12 — Fuzzy recall filter: adopted, then a 4× window for the filtered nets

**Adopted**, 2026-09-26. Rails (51k symbols) and discourse (73k) in one index, searched
repo-scoped; release build, shared machine (load 5–15, so every number is interleaved
against the baseline in the same run).

*Harness.* 2,372 fuzzy queries: 58 hand-picked, the rest derived from 440 randomly
sampled real names (word-prefix abbreviations, dropped vowels, adjacent transpositions,
globs), kept only if nothing matches by exact or prefix, so every one reaches the fuzzy
layers. The oracle is today's top 10. Each derived query also has a ground truth, the
name it came from. The ground-truth half is committed as `make recall`
([RECALL.md](RECALL.md)), over the same commits of both repos; it reproduces the
"today" and "D14 + D15 + window" rows below to within two queries per cell.

*Where the time went.* The two broad nets (first-letter range, trigram OR) hand the
scorer everything they catch, up to the 8,000 cap: a median of 3,784 rows, of which
`align`'s first check rejects nearly all. Walking the net is cheap (FTS postings plus a
symbols seek are ~3 ms for `conpool`). The cost is decoding each row, joining its file
and repo, and allocating it (D2's ~1 µs a row).

*What was built.* `score::could_match(name, kind, file)` is a necessary condition for
`score` to accept a row: the query's letters in order in the name, or in the file stem
for a primary kind, or a possible near miss. Recall registers it as a SQLite function
and applies it to the rows each net's cap already takes, so rejects are never decoded.
It is spelled once, next to the match chain. A test checks that the filter keeps
everything `score` accepts, on both passes, and it fails if either route is dropped.

| 35 queries, 21 reps | today | filtered |
|---|---|---|
| query phase (recall+score+sort), median | 10.3 ms | 6.9 ms |
| query phase, p90 | 16.6 | 10.7 |
| score | 1.8 | 0.4 |
| first answer, median | 12.3 | 9.2 |
| candidates scored | 3,784 | 322 |

Output is byte-identical on all 2,372 queries, both the top 10 and `--limit 0`. The
largest nets are faster too (`tescon`, `testag`: first answer −20 to −35%). The exact
path never reaches the filter and is unchanged. Cold cache was not measured, since
purging the page cache needs root. The filter reads the same symbol pages, so a cold
run should gain less.

*Widening the cap: adopted as its own change.* The cap truncates in the net's own order
(rowid for FTS, name for the first letter), before anything is scored. Net sizes are a
median of 5.9k rows, p90 22.8k and max 40.5k; 913 of the 2,372 queries (38%) exceed
8,000. With the filter in place, a wider window costs a fraction of what it used to:

| window | source ranks #1 | in top 10 | found at all |
|---|---|---|---|
| 8k (today) | 39.6% | 58.2% | 67.8% |
| 16k | 45.2 | 64.8 | 76.5 |
| 24k | 48.5 | 68.4 | 80.3 |
| 32k (= unbounded here) | 48.8 | 68.7 | 80.6 |

At 32k, 254 queries that answer nothing today find a match, often the obvious one:
`test_floa_tlimits` → `test_float_limits`, and `newconnection` reaches the
separator-exact `new_connection`. The filtered nets now read `NET_WINDOW` (4×) past
the cap.

A complete net exposed two ranking weaknesses that the truncated one had hidden, so
the window waited for their fixes: the typo retry's trigger (D14) and letters
matched before a word start (D15). With both in, against today:

| | source #1 | top 10 | found |
|---|---|---|---|
| today | 39.6% | 58.2% | 67.8% |
| D14 + D15 + window | 49.0% | 69.3% | 81.5% |

Of 2,314 sources, 379 move up and 84 down. Six lose #1 and three the top 10. They
are either genuinely ambiguous or decided by a deliberate rule:
- **Ambiguous.** `teswri` has `test_write` and `test_writer`. `scorse_for` and
  `twedele` are covered in D14. For `loatest`, `load_tests` (fuzzy 161) edges
  `LoadingTest` (154). For `tesm`, `TestMailer` is a fair reading of four letters.
  For `causch`, `canUseChat` and `CreateUserChat…` are both two-letter
  abbreviations of three words.
- **The test-path rule.** For `dectest`, `tes*red` and `tes*und`, the source lives
  in a test file. Once the net holds any non-test match, the −400 test-path penalty
  ranks it first, as that penalty intends. A truncated net used to hold only test
  candidates. *Rejected:* making the penalty a share of the match (0.4 × the match
  value, still 400 at exact). The aggregate rose (#1 49.8%), but test definitions
  flooded fuzzy results: 10 queries lost #1 and 10 the top 10. The flat cliff is
  doing real work.

Cost, at load ~7 over 15 reps: the typical query keeps most of the filter's win
(query phase median 10.3 → 7.7 ms, p90 24.7 → 13.8; 7.1 ms without the window). The
largest nets pay: the worst-case set's query phase is +25% against today, with p90
32 → 49 ms. It stays a constant ceiling, which keeps D3's property.

*Rejected:*
- **First letter as the only net** (the anchor already takes a letter range for ≤ 6
  chars). 8% of queries have a top-1 that starts elsewhere, and 28% have one in the top
  10: `aicreator` → `PostActionCreator`, `iman` → `SiteIconManager`. Names led by a
  sigil are rare (9 of ~10k top-10 hits).
- **A standalone subsequence scan** (`name LIKE '%u%s%r%'` over the repo). ~7 ms on
  discourse, ~95 ns a row, linear in repo size, which gives up D3's constant ceiling. A
  char-set bitmask column has the same shape, plus a migration.
- **`LIKE` in SQL instead of the scorer's gate.** Strict, it drops path-only hits (131
  top-10 hits across 83 queries come from a class named only by its file). With a
  primary-kind hatch it keeps every class in the net (1,320 against 304 candidates for
  `conpool`), which gives back half the win. It also spells the gate twice.
- **Filtering in Rust on borrowed columns.** Each rejected row still pays SQLite's
  joins and column reads: `twdl` recall 14 → 24 ms.
- **A stem-in-order path layer plus a dedicated near-miss net.** These reach candidates
  no net reaches today. `sleect` found `IsolatedExecutionState` by its file name, which
  suppressed the retry that finds `Select`, and displaced top-10s doubled.

*Lead, not chased:* since every search now records usage (0.52.0), about 9% of
searches in both builds spend 5–15 ms in `record usage`, before the first answer.
D13 found the cause and moved the write after the output; the lock suspicion was
wrong.

*Reverses if:* a new way to match can't be decided from name, kind and file (then the
filter can't be a necessary condition); or per-search function registration shows up
in a profile. The window reverses if the largest nets' cost starts to matter more
than their recall: first answers near the 50 ms budget on real use.

## D13 — The usage write after the answer: adopted. The lock was never contended

**Adopted**, 2026-09-26. Revisits D7's rejection. Rails, release build, the fuzzy
queries from D12 plus ten exact names; every burst interleaves the two builds search by
search on identical copies of one DB.

*The lead.* D12 saw about 9% of searches spend 5–15 ms in `record usage`, before the
first answer, and suspected the write lock: the detached `rq --warm` child writes too,
and `busy_timeout` sleeps 1, 2, 5, 10 ms between retries. The shape fits. The cause
doesn't.

*What the tail is.* It appears on a saturated machine: under 8 CPU spinners on 8 cores,
9–18% of usage writes take over 3 ms (max ~20 ms), against 0–2% without them. Probed
with a throwaway build:
- With `busy_timeout` set to 0 for the write, **0 of ~700** loaded searches got
  `SQLITE_BUSY`. Nothing held the lock.
- No detached child (`RQ_WARM_DETACH=0`): unchanged, 14%. `synchronous=OFF`: unchanged,
  14%. No checkpoint on close, so the next process appends to a live WAL instead of
  restarting it: unchanged.
- Split inside the write: preparing the statement is 0.02 ms, stepping it carries all of
  it. A second identical upsert in the same process is 0.02 ms (max 0.07), and a plain
  file append 0.16 ms at worst.

So it is the process's first write transaction stretching under CPU contention, not a
wait on another process. The mechanism inside SQLite and the kernel wasn't pinned, and
doesn't need to be: nothing about the write made it cheaper, so the fix is to move it.

*What was built.* The ranked list counts its search after printing, before the warm child
is spawned. A miss counts after its answer too. `--show`, `--open` and `--web` still
count before they fork, since they leave by their own exits and `--open` `exec`s; a
`--show` that falls through to the list is already counted there. D7's objection, a
count split across exits, now covers only those rare exits (none of the 349 searches in
D10 used them). A test checks the order in `--profile`, and that a `--show` that prints a
body and one that falls through count as two.

| 300 searches a build, 8 spinners | before | after |
|---|---|---|
| exact: first answer p50 / p90 / p99 / max | 3.4 / 7.2 / 14.6 / 22.4 ms | 2.5 / 4.3 / 6.9 / 8.8 |
| exact: wall p50 / p90 | 12.7 / 19.1 | 12.5 / 18.2 |
| fuzzy: first answer p50 / p90 | 11.4 / 28.3 | 9.7 / 26.3 |
| fuzzy: wall p50 / p90 | 21.8 / 37.3 | 21.2 / 37.7 |

Output and exit codes are identical on all 600 search pairs, and `--usage` totals match
the searches run (900 and 900): no count is dropped. On an unloaded machine the change is
the ~0.45 ms D7 measured.

**Wall time does not move**, and it's what an agent waits on: the shell returns at exit,
not at the last line. The win is for a person at a terminal and anything reading the
stream.

*Rejected:*
- **Try once, drop the count on `SQLITE_BUSY`.** There's no contention to skip, and it
  would lose counts exactly when some is real (a search during `--index`).
- **Handing the count to the warm child**, the only route off wall time. The child isn't
  spawned on a miss, outside git, or with detach off, so it needs a second path, for
  0.5–0.7 ms of a 12–22 ms wall at the median.
- **A writer thread alongside `signatures` and render.** A connection isn't `Sync`, a
  second one costs about what the write does, and the process still joins it before
  exit.

*Reverses if:* a search must read back something it wrote, or the usage write grows past
bookkeeping.

## D14 — Near misses compete with fuzzy matches, scored by the letters that agree

**Adopted**, 2026-09-26. Same harness and index as D12: 2,372 fuzzy queries, 417 of
them adjacent transpositions of real names, each with the name it came from.

*The trigger.* The typo pass ran only when every first-pass hit scored ≤ 0. So any
name that merely held the query's letters in order hid the typo reading:
`fethc_version` → `fetch_conversations`, and `iteraet!` → `register_range_type`.
It went unnoticed because the capped nets rarely held such a name; D12's complete
nets would have made it routine. A typo now competes with fuzzy matches but never
with literal ones. Without a positive exact, prefix, glob or constructor hit, near
misses join the ranking, provided the name evidence (the `typo` value) is at least
the best first-pass `fuzzy` value. That gate compares names before kind, extent
and recency, which every candidate carries: without it, `shft` → `Sheet`,
`updget` → `Update` and `cmlz` → `Cli` rode in on those. With nothing above zero,
every near miss joins, as before. A scope typo (`Widgit.new`) stays last-resort.

*The score.* The flat 120 − 40·edits became evidence. A near miss scores as the
fuzzy alignment of its longest common subsequence with the query, times the share
of those letters not undone by an edit, less the usual unmatched-tail charge.
`sleect` → `Select` is 89, against 48 for a scattered in-order `IsolatedExecutionState`.

*Tried first:*
- The name's self-alignment × (1 − edits / query length). It credits contiguity the
  user never typed: 21 queries lost their source from #1, 22 from the top 10.
- The common subsequence alone, with no per-edit charge. A 2-letter overlap scored
  like a 4-letter abbreviation, and `windows` (2 edits) tied `Window` (1): 30 lost
  #1.
- Per-edit discount without the evidence gate. Weak guesses won on side features: 4
  lost #1, 12 lost the top 10.

*Result against today.* Source at #1 is 916 → 916, top 10 1,347 → 1,358, found
1,568 → 1,588. 21 sources move up and 4 down, and top-10s change in 13 of 2,372
queries. Two lose #1. Both are genuinely ambiguous: `scorse_for`, where
`score_for` and `scores_for` are each one edit away (166 vs 163); and `twedele`,
where `tweedle_deedle` holds every letter in order and outranks the one-edit
`tweedle`, whereas the old retry used to discard first-pass hits wholesale. Cost:
query phase median 7.1 → 7.3 ms, since near misses are now scored on every fuzzy
query.

*Reverses if:* typo candidates start winning on queries whose in-order reading was
right. The gate is the lever: it compares evidence, not totals.

## D15 — Letters matched before a word start earn no credit

**Adopted**, 2026-09-26. Same harness as D12 and D14.

*The weakness.* `align` requires every word after the first to be entered at its
start, but the first matched letter may land anywhere. So the only scattered match
it allows is one that begins inside a word and picks letters there before reaching
a boundary: `testag` → `ac[t]iv[e]Storage`. Those letters earned the same base
credit as letters at word starts, and a large module's kind and extent then
carried it past names that actually read as the query.

*The rule.* The DP tracks whether the alignment has matched a word start yet.
Letters before that earn no base credit, and neither do their boundary bonuses;
gap and contiguity scoring are unchanged, so a contiguous run inside a word
(`cache` in `Precache`) still counts for something. The penalty is graded by the
alignment itself: one stray leading letter costs little, several cost more, and an
alignment that starts at a boundary is untouched. There is no threshold.

*Result, on top of D14, against today.* Source at #1 916 → 920, top 10 1,347 →
1,361, found 1,568 → 1,588. 83 sources move up and 5 down; no source leaves the top
10 and none loses #1 beyond D14's two ambiguous cases. The two-state table left
the query phase flat (7.0 → 7.1 ms median).

## D16 — A warm child's "nothing moved" spares the next hits the spawn

**Adopted**, 2026-09-26. Release build, rails (3.3k indexed files) and discourse
(14.4k), isolated DB. Each run is bursts of 20 back-to-back exact searches, with
the two builds interleaved burst by burst. Between bursts the harness waited for
every `--warm` child to exit. Other work kept the machine loaded throughout
(load average 20–45), so absolute walls are inflated and vary between runs. The
direction held in every run.

*The cost.* Since b7678d6 every hit on a complete repo spawns `rq --warm`, which runs
`git status` and usually exits having found nothing. The parent pays for the spawn.
A burst's searches also compete for CPU with the children's `git status`, which grows
with the worktree.

*What was built.* When the child finds nothing moved, it records the verdict in `meta`
with a stamp: checkout root, HEAD commit (which must still be the indexed one) and the
`.git/index` mtime in nanoseconds. A hit reads the stamp back and skips the spawn when
it matches and the verdict is under 10 s old (`RQ_WARM_RECHECK_MS`). The read is two
small file reads, a stat and a meta read, 0.1 ms in `--profile`. The window starts
*before* the child's `git status`. The stamp is read *after* it, because status can
rewrite `.git/index` itself. Unchanged:
- A miss still checks inline.
- The single-flight lock and the dirty-worktree rule (a101921) work as before.
- A sweep writes no verdict, so the hit after a real change still spawns a child to
  confirm.

| wall to exit, 20-search bursts | before p50 / p90 | after p50 / p90 |
|---|---|---|
| rails, clean (3 runs) | 9.6–12.8 / 16.7–46.6 ms | 9.3–12.5 / 15.7–42.7 |
| rails, dirty but indexed (2 runs) | 6.9–20.3 / 9.9–57.9 | 5.9–17.1 / 8.0–49.7 |
| discourse, clean (3 runs) | 12.0–18.5 / 29.2–37.1 | 8.5–12.1 / 14.8–21.1 |
| discourse, dirty but indexed (2 runs) | 22.0–24.1 / 39.8–46.7 | 14.7–16.0 / 24.4–33.2 |
| burst to quiet (last child exits), median | 30–76 ms | 21–41 ms |

On rails the paired median drops 0.3–0.5 ms clean and 1.0–3.2 ms dirty. Each
discourse child's `git status` is heavier, and there the median drops by about a
third and p90 by about half. Output and exit codes are identical.

*What the window allows.* Every git operation changes the stamp, so the next hit
spawns. Commit, checkout, reset and pull move HEAD. Merge, stash and `git add`
rewrite `.git/index`. An unstaged edit to a tracked file touches nothing in `.git`,
so the window is what catches it: the first hit more than 10 s after the verdict
spawns the child. Until then the edit can only hide a symbol in a file that isn't
among the hits, because a miss checks inline and a top hit's file is revalidated on
read. New untracked files were never seen by the child (`git status
--untracked-files=no`), so they are no worse off. The only usage evidence is a small
sample from local agent transcripts:
- 12 of 23 gaps between consecutive searches were under 10 s.
- 1 of 25 searches came within 10 s of an edit.

*Why this isn't D7's rejected pre-lock check.* D7 moved the child's `git status` ahead
of the lock, so children ran it concurrently. Here the parent reads a stored verdict and
forks nothing. Fewer children run, not more, and the lock still serializes the ones that
do.

*Rejected:*
- **Catching bare edits without a window.** Only a stat of every tracked file sees
  them. That is the work `git status` does, and it would move the child's cost into
  the parent.
- **Stamping before `git status`.** On a freshly written tree (the e2e fixture),
  status's racy-entry refresh rewrote `.git/index` after the stamp was taken, so the
  next hit spawned anyway.
- **Reusing the branch-files stamp.** It holds whole-second mtimes of `.git/HEAD` and
  `.git/index`. A commit updates the branch ref, not `.git/HEAD`. A whole second is
  also coarse next to a burst of searches.

*Reverses if:* staleness from an unstaged edit shows up in real use (shrink the
window first), or spawning gets cheap enough that the stamp isn't worth its window.

## D17 — Constants in Go, Python and TS/JS: what counts, per language

**Adopted**, 2026-09-26. Recall harness (D12) against main, release builds.

Ruby and Rust already emitted `constant`. Each of the other plugins now does too,
with the rule each language's own syntax supports:

- **Go:** package-level `const` (single, grouped, `iota`), one symbol per bound
  name at its spec's line. **Package-level `var` is out.** It is mutable state,
  and `constant` would mislabel it. The cost is the `var ErrFoo = errors.New(…)`
  sentinels, which are constant in all but name and are real jump targets.
- **Python:** an `UPPER_SNAKE` assignment at module or class level (annotated,
  tuple and chained forms too), parented by its class. Python has no `const`, so
  the naming convention is the only declaration of intent. Lowercase module
  variables are state. A single capital (`T = TypeVar("T")`) is not a constant.
- **TS/JS:** every module- or namespace-level `const` that is not a function
  (already emitted as one) and not a `require(…)` binding, **whatever its
  casing**. Plus a class's `static readonly` field. Out: `let`/`var` (mutable,
  as in Go), destructuring (binds names, defines nothing), anything below module
  level, and enum members (the enum is the target; Rust doesn't index variants
  either, and `Red`/`None`/`Default` would collide everywhere).

*Why all `const`s in JS, not just `UPPER_SNAKE`.* In JS the keyword is the
declaration. `export const router = createRouter()`, `const Button =
styled.button`, `export const store = configureStore(…)` are definitions people
jump to, and their casing says nothing about that. Measured against an
`UPPER_SNAKE`-only build of the same change:

| vs main | sources down | lost #1 | lost top 10 |
|---|---|---|---|
| all module-level `const` (adopted) | 45 | 2 | 2 |
| `UPPER_SNAKE` only | 39 | 1 | 1 |

Of 2,314 sources. Both runs gained nothing: every source was sampled from main's
index, so a new definition can only push one down. Every loss is in discourse's
JS and each is ambiguous rather than wrong: `boo*met` now finds
`BOOLEAN_METHODS` over `bookmark_metadata`, `logchannel` finds `LOG_CHANNEL`
over `LOGS_CHANNEL`, and the camelCase increment is `tes*pag` → `themeTestPages`
and `rnscrp` (`run_script` #10 → #11). Discourse gains 1,413 JS/TS constants
(1.9% of its symbols; 436 not upper-case). The camelCase rule costs 6 slips and
one ambiguous #1.

*Indexing throughput* is unchanged within noise (median of 5 fresh indexes each,
interleaved, load average about 20): django 1.08 → 1.09 s (+5% symbols), twirp's
vendored Go 0.12 → 0.13 s (+30%, mostly generated protobuf consts), discourse
4.39 → 4.20 s.

*Reverses if:* camelCase consts crowd real answers in use (drop to
`UPPER_SNAKE` for JS; the table is the price), or a `variable` kind joins the
model, which would be the honest home for Go's `var` sentinels.

## D18 — `--anchor`: rank from the position a query is asked from

**Adopted**, 2026-09-26. Recall harness (D12) plus a new anchored set, release builds,
against main.

A query now says where it is asked from: `--anchor FILE:LINE[:COL]`, an editor's cursor
or the file an agent is reading. Two additive features, both boosts, never filters:

- **`enclosing`**: the innermost definition whose `line..end_line` span holds the
  anchor line gives a scope chain (`Shop::Widget#persist` → `shop, widget, persist`).
  A candidate whose `parent` is a leading run of that chain earns 60 per shared level,
  capped at 180. A bare `save` inside `Widget` then prefers `Widget#save`.
- **`proximity`**: 90 in the anchor's own file. Otherwise 60 in its directory, halving
  per directory step between the two and dropped below 5. Anchor's repo only.

Both read only stored spans and parents, so no language logic reaches the core. The
anchor file comes from the index when its stored mtime matches, else a live parse,
because an editor's file is often dirty or new. That costs 0.1–0.3 ms from the index and
about 2.6 ms for a live parse of a 1,900-line file. rq records no inheritance, so a
method the enclosing class inherits earns nothing from `enclosing`.

*The set.* `script/recall/anchored.tsv`: 446 Ruby call sites (190 rails, 256 discourse).
Names were sampled from those defined at least twice. The truth is the one definition
`trekr --def` resolves the call to at confidence 0.9 or more, ranked by location.

| 446 call sites | #1 plain | #1 anchored | top 10 plain | top 10 anchored |
|---|---|---|---|---|
| all | 213 (47.8%) | 351 (78.7%) | 432 (96.9%) | 439 (98.4%) |
| truth in the anchor's file (276) | 115 (41.7%) | 258 (93.5%) | 269 | 276 |
| truth elsewhere (170) | 98 (57.6%) | 93 (54.7%) | 163 | 163 |
| bare/implicit call (303) | 131 (43.2%) | 273 (90.1%) | 296 | 303 |
| `Const.call` (82) | 51 | 50 | 80 | 80 |
| `local.call` (47) | 25 | 22 | 42 | 42 |

170 sources moved up and 9 down. Most of the gain is the 62% of call sites whose
definition sits in the anchor's own file, which is what implicit Ruby calls mostly are.
Where it doesn't, the anchor costs 5 #1s and no top 10s. The 9 losses are the two known
limits:
- **An inherited method.** `read_message` inside `MessageEncryptor` runs a prepended
  module's.
- **A call on another class, asked as a bare name, from a class that defines the same
  name.** `TopicSubtype.notify_moderators` from inside `Topic`. `rq TopicSubtype.notify_moderators`
  is the query for that.

*Unanchored ranking is unchanged.* The full recall run against main changed 0 of 2,372
top 10s (1,132 / 1,602 / 1,886 #1 / top 10 / found, both). `--json --limit 0 --explain`
over 300 rails queries on one shared index is byte-identical: 26,557 rows.

*Weights.*

| variant | #1 | top 10 | #1, truth elsewhere |
|---|---|---|---|
| adopted (60/180, 90/60) | 351 | 439 | 93 |
| `enclosing` only | 347 | 437 | 91 |
| `proximity` only | 344 | 436 | 93 |
| both at half weight | 352 | 438 | 95 |

Each signal alone gets most of the gain, because in Ruby the enclosing class and the
same file usually coincide. Both are kept: `enclosing` reaches a class reopened or
`impl`-ed in another file, and `proximity` serves top-level functions (Go, JS) with no
enclosing definition. Half weights measure the same, but the harness zeroes `recency`,
which spreads 0–120 in real use. The anchor is a deliberate signal and should not lose
to a file's mtime, so the weights sit at `recency`'s scale, below `branch`'s 180 per
file, and well under the gap between match tiers.

*Rejected:*
- **Filtering to the anchor's scope.** Navigation, not search: the definition you want
  is often the inherited or external one rq can't see.
- **Inferring the receiver from the source line.** That is a language's semantics, and
  trekr's job.

*Reverses if:* an inheritance model lands (then `enclosing` should walk ancestors), or
anchored use shows the same-file boost crowding out a better definition elsewhere
(shrink `proximity` first, since `enclosing` carries the scope case alone).

## D19 — Only word joiners are separators: `save!` is not an exact `save`

**Adopted**, 2026-09-26. Recall harness (D12) and the anchored set (D18), release builds,
against main.

The separator-insensitive exact match compared names with every non-alphanumeric
character dropped. So `save!`, `valid?` and `name=` were exact matches for `save`,
`valid` and `name`, 50 behind the real one (`separators -50`). In rails,
`rq 'ActiveRecord::Persistence#save'` scored `save` 1456 and `save!` 1401, confidence
0.61, and `--show` fell back to the list on a name typed exactly.

Now only the word joiners `_`, `-` and `.` may be left out. Any other character is part
of the name, so `save!` is a prefix match for `save` (699), and `save` scores 1456 at
confidence 1.00. `save!` still finds `save!` exactly. The rule is about characters, not
languages: `!`, `?` and `=` only turn up in the names of languages that allow them.

| 2,314 sourced queries | #1 | top 10 | found |
|---|---|---|---|
| main | 1,132 | 1,602 | 1,886 |
| adopted | 1,133 | 1,602 | 1,886 |
| also separator-insensitive prefix | 1,139 | 1,599 | 1,881 |

Adopted: 2 up, 1 down, top 10 changed in 8 of 2,372 queries. The one loss is `hasicon`
for `has_icon?`, #1 → #10, now fuzzy rather than exact-with-separators. It left out
both the joiner and the `?`, and main already reads a separator-less *prefix* as fuzzy
(`parsefil` for `parse_file`). Anchored call sites are unchanged: 351 #1, 439 top 10
of 446.

*Rejected: separators left out of a prefix too.* That keeps `hasicon` at #1, and gains 7
#1s overall. But it loses 3 top 10s and 5 found (`setview` for `setup_view` now finds
only `set_view_paths`). A separator-less prefix counts as a literal match, which
suppresses the fuzzy tail (D12) where main kept it. That's a change to what counts as
literal, with ambiguous queries on both sides, not a fix for sigils.

*Rejected: a `sigil` penalty on an exact match.* That needs a list of sigil characters in
the core, and a size to tune. Reading them as part of the name gets the same result from
the prefix tier.

*Reverses if:* separator-less prefixes are taken up on their own merits (the rejected row
is their price), or a language has names where a non-joiner punctuation character is
routinely left off when typing.
