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
  doing real work. *Adopted in D24* for fuzzy and typo matches, once side features stopped
  deciding among the compressed test matches.

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
right. The gate is the lever: it compares evidence, not totals. *Gate dropped in D24*:
once side features scale with match quality, totals compare evidence, and the gate only
hid near misses that ranked below the in-order reading anyway.

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
  either, and `Red`/`None`/`Default` would collide everywhere). *Reversed by
  D37:* members are now `variant`s, as Rust's are.

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

## D20 — Storage audit: trigram positions dropped; durability and page size left alone

**Decided**, 2026-09-26. Release builds, cold and incremental rails indexes plus searches
from inside rails, 5 reps interleaved build by build on a shared machine (load 10–15).
Sizes are from `dbstat`, on a rails + discourse index (126k symbols, 34.7 MB) and one of
nine repos (171k symbols, 43.2 MB).

*Where the bytes are* (rails + discourse, before):

| object | MB | share | rows | avg payload |
|---|---|---|---|---|
| `symbols` | 12.1 | 37% | 126k | 89 B |
| `symbols_fts_data` (trigram postings) | 6.3 | 19% | | |
| `idx_symbols_repo_name` | 3.9 | 12% | 126k | 25 B |
| `idx_symbols_name_lower` | 3.7 | 11% | 126k | 23 B |
| `files` + its `(repository_id, path)` key | 3.7 | 11% | 18k | 102 B |
| `idx_symbols_file` | 1.5 | 5% | | |
| `symbols_fts_docsize` | 1.2 | 4% | | |
| everything else (`usage_daily`, `meta`, …) | < 0.1 | | | |

*Adopted: `detail=none` on the trigram table.* Recall's `MATCH` is an OR of single
quoted trigrams, which never reads token positions, and positions were most of the
postings. Candidates come back in the same rowid order, so output can't move; checked
byte-identical on the benchmark queries and on the full recall set (`make recall`, no
source moved). The cost is a constraint, not a regression: a `MATCH` on anything longer
than one trigram is a phrase query, which `detail=none` refuses (one test used one).

| rails, cold index | before | after |
|---|---|---|
| database | 15.1 MB | 12.3 MB (−19%) |
| FTS sync (serial, after the parse) | 94 ms | 68 ms |
| cold index total | 658 ms | 631 ms (inside the noise) |
| one-file refresh, writes | 0.5 ms | 0.6 ms |
| fuzzy query phase (`conpool`, `actconn`, `sleect`) | 4.4 / 6.2 / 12.4 ms | 4.4 / 6.1 / 11.9 |

On the nine-repo index the postings go 7.7 → 3.0 MB. Schema v15 recreates the table and
rebuilds it from `symbols` (0.25 s at 171k symbols); nothing is re-parsed. SQLite
doesn't shrink the file, and later writes reuse the freed pages.

*Rejected, measured flat:*
- **`synchronous=OFF`.** In WAL with `NORMAL`, a commit doesn't `fsync`; only a
  checkpoint does. Cold total 638 vs 658 ms (control), one-file writes 0.5 ms both, the
  per-search usage write 0.5 vs 0.6 ms. Nothing to buy with the durability, even though
  every byte can be rebuilt. D13 already found the usage write's tail is CPU, not I/O.
- **8 KB or 16 KB pages.** Size within 0.5%, FTS sync −14 ms at 16 KB, queries within
  ±0.3 ms. Not worth a setting that only a fresh database can take.
- **`temp_store`, checkpoint policy.** Already `MEMORY` and the default auto-checkpoint;
  the last connection checkpoints and removes the WAL at close, and nothing in the
  profile waits on either.

*Rejected, measured and not worth it:*
- **Dropping `idx_symbols_name_lower`** (12% of the database, and 18% off the writer on a
  cold rails index: 231 → 189 ms, overlapped with parsing). Every name query joins
  `repositories`, so the planner already seeks `(repository_id, name_lower)` once per
  repo, with or without planner stats. The one exception is the unscoped first-letter
  net (`-a`, or a search outside any repo), whose inner query has no join. Without the
  index it needs a skip-scan plus a sort to keep its name order. Output stayed
  identical, but on the nine-repo index (7 interleaved reps) that net's recall got
  slower: `usr` +5 ms, `acn` +6, `sleect` +4 to +8, `tes*lim` +12 to +16. Those
  unscoped searches are already the slowest ones rq runs.
- **Interning `kind`, `language`, `visibility`** as small integers. They're ~17 of a
  symbol row's 89 bytes, so about 6% of the database. That would touch the write path,
  the candidate decode, the path layer's `kind IN (…)` and the `rq_keep` signature, all
  to shrink the database with no speed gain (D2: materializing a row costs ~1 µs of
  which a column is noise).
- **Dropping the `name_lower` column** for an index on `lower(name)` (~7%). SQLite's
  `lower()` folds ASCII only and the indexer uses Rust's full Unicode `to_lowercase`,
  so non-ASCII names would stop matching the same way. A custom collating function
  would make the database unreadable to anything that hasn't registered it.
- **`WITHOUT ROWID`.** The only key-only tables are `usage_daily` and `meta`, one page
  each.
- **Removing the trigram table.** After `detail=none` it's 3.6 of ~31 MB, and it's the
  only fuzzy net for a query longer than six characters: the first-letter net stops
  there, and D12 measured the scan alternatives at linear cost. D21 tried an fst
  instead.

*Reverses if:* a query needs a real substring or phrase `MATCH` (then `detail=column`
buys nothing either: it measured 6.1 MB against 6.2). The name index goes if unscoped
fuzzy search gets a net that doesn't need name order across repos.

## D21 — An fst over names as the fuzzy net: rejected, slower than FTS

**Rejected**, 2026-09-26. Spike on branch `storage-fst`: rails + discourse at the pinned
recall commits, release build, interleaved query by query against FTS (load 15–36, so
the ratios are the result, not the milliseconds).

*What was built.* One `fst::Map` per repo from `name_lower` to symbol ids (shared names
point into a postings array). It was stored as a blob, built from the
`(repository_id, name_lower)` index in name order, and walked with an automaton for
today's two broad nets together: holds any of the query's trigrams, or starts with its
first letter (≤ 6 characters). The ids then went through the same `rq_keep` filter and
cap as today. The nets have no window, so this is today's recall minus truncation.

*Recall is nearly the same.* 8 sources up, 6 down, the top 10 changed in 12 of 2,372
queries. #1 held at 1,133, top 10 went 1,602 → 1,601, and found went 1,886 → 1,883.
The one lost top 10 is `tes*tra` (#9 → gone). Its complete net passes more than 8,000
rows through the filter, so the cap cuts in id order rather than the order the old
window read. Anchored call sites were unchanged (351 #1, 439 top 10).

*Latency is worse everywhere.* A trigram can sit anywhere in a name, so the automaton
can prune nothing, and the walk visits every key. For rails (32k distinct names),
streaming all of them costs as much as the whole FTS recall.

| recall, median ms | FTS | fst |
|---|---|---|
| rails `usr` / `conpool` / `sleect` / `twdl` | 4.7 / 10.5 / 16.0 / 27.0 | 20.6 / 23.9 / 30.0 / 37.2 |
| discourse `usr` / `conpool` / `sleect` / `twdl` | 10.1 / 11.0 / 29.8 / 10.4 | 47.5 / 44.4 / 50.8 / 41.4 |

Output was byte-identical on all 16 of those queries. The cost is linear in the repo's
distinct names, which gives up D3's property that recall doesn't grow with the corpus.
FTS postings grow with the matches.

*Size is a wash.* Rails' fst plus postings is 785 KB against 1.7 MB of trigram
table after D20. That's ~7% of the database, for a slower net.

*Freshness would add machinery.* An fst is immutable. Rebuilding one from the name index
cost 76 ms for rails, paid by every warm that changes a file, and until then the names
just edited, the likeliest query, are invisible to fuzzy recall. The workable design is
a base fst plus a delta of symbols above a watermark id. That needs `AUTOINCREMENT`,
since a re-indexed file otherwise reuses ids below the watermark, plus a background
rebuild threshold. That's a second recall path for no gain.

*Where an fst does win, and why it isn't enough.* A Levenshtein automaton prunes: at
distance 1 it walked rails in 0.1–0.5 ms. But typos are already rq's best row (94.5% #1,
97.6% top 10 through the trigram net), and prefix and exact are B-tree seeks that cost
0.2–0.4 ms.

*Reverses if:* a net that *can* prune (Levenshtein, or a prefix-anchored abbreviation
automaton) replaces a broad net rather than joining it. Or the corpus grows until FTS's
postings, not the row decode, dominate recall (D3's reversal), and a sharded or
per-repo fst beats per-repo FTS tables.

## D22 — Errors exit with sysexits codes, apart from every verdict

**Adopted**, 2026-09-27.

Before this, clap's usage errors exited `2`, the code rq gives `warming`, and rq's own
errors exited `1`, the code it gives a miss. A script branching on the number read a
typo'd flag as "ask again" and retried it forever, and read an unopenable index as "this
symbol does not exist" — the confident wrong answer the warming code exists to prevent.
The README told callers to branch on `kind` instead, which only a JSON caller can do.

Now each code means one thing:

| Exit | Meaning | `kind` |
|---|---|---|
| 0 | matched | |
| 1 | miss (`no_match`, `scope_not_found`) | |
| 2 | no answer yet (`warming`, `interrupted`) — ask again | |
| 64 `EX_USAGE` | the command line is wrong | `usage` |
| 66 `EX_NOINPUT` | a file the command names doesn't exist | `not_found` |
| 69 `EX_UNAVAILABLE` | no git host, editor or browser to hand off to | `no_remote`, `launch` |
| 70 `EX_SOFTWARE` | rq couldn't render its own output | `internal` |
| 74 `EX_IOERR` | the index can't be opened, read or written | `database`, `index` |

*Why sysexits.* It is the only convention for error codes a reader might already know,
and its range starts above anything a verdict will ever need. `64` was the user's call
and nothing argued against it: it sits clear of the shell's reserved `126`–`128+n`.

*Why a code per remedy, not per `kind`.* The number is for the caller that can't read
JSON, and what that caller needs is what to do next. `database` and `index` are both "rq's
storage failed; look at the disk or `RQ_DB`", and `no_remote` and `launch` are both "the
thing rq hands off to isn't there". `kind` still tells them apart for a JSON caller.

*Why not `2` for usage, as clap and most Unix tools do.* `2` already means ask again, and
the whole value of that code is that a caller can retry on it blindly. A retryable code
that is sometimes a typo is not retryable.

`Failure::exit_code` is the one mapping, and the JSON `code` is read from it, so the
object and the process can't disagree. The VS Code extension already treated anything
but 0–2 as an error. It now also reads an older rq's exit-2 JSON error as an error, not
as warming.

*Reverses if:* a caller shows up that needs two errors under one code told apart without
JSON — split that code, never reuse `1` or `2`.

## D23 — A name index for fuzzy recall: built, exact, and the default since D24

**Built behind `RQ_RECALL=scan`**, 2026-09-27; **the default** later that day, once D24
fixed the ranking below (final numbers at the end of D24). `RQ_RECALL=fts` forces the
old nets, which also remain the fallback while a repo's index is missing or rebuilding. Rails and discourse at the
recall pins, release builds, every timing interleaved against main on a shared machine
(load 5–20). How it works is in [NAME_INDEX.md](NAME_INDEX.md), along with the spike it came from.

*What.* Per repo, a 40-byte signature for every distinct symbol name and every file (signed
by its stem): which characters it holds, a Bloom filter of the character pairs a query can
step across under `align`'s rules, and a typo key. Recall screens every signature,
verifies the survivors with the scorer's own chain, and fetches rows only for what `score`
accepts. It replaces the first-letter net, the trigram net, the path `LIKE` net, `rq_keep`
and `NET_WINDOW`. This is D21's reversal clause: a net that prunes, replacing the broad
ones rather than joining them. The spike's pair postings were 2–6× faster again at these
sizes, but need a base, a delta and a background rebuild. The scan is append-only, and the
postings can be layered on the same `transition_pairs` later.

*Exact, and tested as exact.* A property test compares the index with rq's real `score` on
the harness's queries and names, plus derived and edge-case ones (non-ASCII, sigils,
acronyms, a name longer than the bit-parallel verifier takes). An ignored test runs every
harness query against every name in rails (32,987) and discourse (44,708): **0
disagreements over ~92M pairs.** D12's recall test now asserts that the index's candidates
are exactly what `score` takes from every row in its store. The format stamp,
`NAME_INDEX_FORMAT`, rebuilds an index written under other matching rules.

*Storage: SQLite, not a mapped file.* Measured in process with a fresh connection or
mapping per query, over all 2,372 harness queries:

| | median | p90 | p99 |
|---|---|---|---|
| in memory (floor) | 172 µs | 628 | 2,564 |
| flat file, mapped per query | 280 | 730 | 2,663 |
| SQLite chunks of 512, fresh connection | 540 | 1,008 | 3,044 |

The file is ~0.26 ms faster. SQLite gets appends in the symbols' own transaction, crash
safety, and `--drop` and a replaced `RQ_DB` for free. The file would need a cross-process
publish protocol, a way to reconcile names committed to SQLite but never appended, and
its own lifecycle. Chunk size (256, 1,024, 4,096) moved the scan by under 10%.

*The path net had to go into the index too.* With only the name index plus the old `LIKE`
net, D12's recall test failed: `widgetcontroller` lost `Widgets` in `widget_controller.rb`,
a primary definition named only by its file, which the trigram net used to carry in
through `rq_keep`'s stem branch. D12 counted 131 top-10 hits from such definitions. Files
are the second key kind, and every primary kind counts, where the `LIKE` net took only
classes and modules.

*Recall (`make recall`, main against scan).*

| | source #1 | top 10 | found |
|---|---|---|---|
| main | 1,133 (49.0%) | 1,602 (69.2%) | 1,886 (81.5%) |
| scan | 1,218 (52.6%) | 1,677 (72.5%) | 1,991 (86.0%) |

`abbr2` carries it (#1 119 → 212, found 272 → 378). `consonants` loses (#1 108 → 103, top
10 235 → 219), and so does `typo` (#1 394 → 391). 110 sources move up and 258 down. Anchored
call sites are identical in every cut (351 #1, 439 top 10 with `--anchor`): they are defined
names, answered by the exact and prefix layers. Two runs gave the same losses.

*Why it isn't the default: every loss is ranking, and read.* 14 sources lost #1 and 33 the
top 10 (46 queries). Every source is still recalled. They are weaknesses the capped nets
hid by never holding the competitor, as D12 found:
- **The test-path cliff, 22 (5 lost #1).** The source is in a test file, and complete
  recall almost always holds *some* non-test name with the letters in order, which the
  −400 penalty then ranks above it however weak its match. `coclfi` →
  `delete_action_cable_files_skipping_action_cable` (fuzzy 15) over `ConditionalClassFilter`
  (103); `tstwrt` → `Start`, a near miss that joined because every in-order hit was a test
  (score ≤ 0, so D14's bar fell to nothing). D12 accepted this for three queries and
  rejected a proportional penalty; at 22 it needs another look, probably a bar on name
  evidence like D14's.
- **Side features over name evidence, 22 (7 lost #1).** A scattered match wins on extent,
  kind, `path` and depth: `fipuno` → `BackfillPushNotificationLevel` (fuzzy 65 + extent 35
  + kind 15 + path 16) over `find_published_node` (109 + 11): a 44-point lead in name
  evidence loses to 55 points of side features. `lclztn` falls from #5 to #51 under 118
  hits.
- **D14's gate, 1.** `tets_br` loses `test_br` from its results: `test_sub_regions` holds the
  letters in order at 114 against the transposition's 102, so the near miss never joins.
- **The query's `_`, 1.** `_dshrz` now ranks the public `dasherize` over `_dasherize`;
  `align` ignores the query's separators.

*Latency.* 395 queries (every sixth, all types), 3 reps interleaved, load 8–10, ms:

| | main | scan | this build, FTS |
|---|---|---|---|
| recall median / p90 / p99 | 9.4 / 22.9 / 30.8 | 0.90 / 2.3 / 8.3 | 9.6 / 24.5 / 31.9 |
| first answer median / p90 / p99 | 12.3 / 27.9 / 40.1 | 1.9 / 4.2 / 14.5 | 12.4 / 29.0 / 39.1 |
| wall median | 32.7 | 31.8 | 32.7 |

Recall's median is 0.8–1.3 ms for every query type, against 6–14 ms. Wall doesn't move:
the git check after the answer dominates it (D13's point). The FTS column is this build
without the switch: keeping the index costs nothing at query time. `make recall --bench`
(58 hand queries, 5 reps) agrees: query phase 9.1 → 1.9 ms, first answer 10.7 → 3.3.

*Costs.* The database grows 29.7 → 36.9 MB (+24%) on rails and discourse, `name_sigs`
7.2 MB: signatures 3.8, keys 2.7, the rest page overhead. Dropping FTS gives back 3.7. A
cold index adds a serial rebuild about as long as the FTS sync: 55–71 ms on rails
(FTS 62–80), 89–124 on discourse, 5–10% of the pass; signing is half of it (~0.8 µs a
key, allocating). A one-file write of 20 symbols, five of them new names: 480 → 551 µs
median (p90 602 → 686). Appends first re-signed the whole last chunk, which cost +200 µs;
they now keep its signatures.

*Rejected:*
- **The mapped file**, above.
- **Keeping the `LIKE` path net**, above: it loses path-only hits the nets had.
- **Pair postings now.** 2–6× faster in the spike, needing machinery the scan doesn't.
  Revisit at a million names, where the scan's median is ~5 ms.

*Reverses if (becomes the default):* the test-path and side-feature losses are fixed as
ranking changes of their own, measured on this harness with `RQ_RECALL=scan`, and the
losses left are ambiguous or deliberate. Then the FTS table and its triggers go in a
migration, which has to build every repo's index first. *Reverses entirely if:* a way to
match can't be expressed as transitions between a name's characters, or a matching rule
changes in a way the property test can't keep the pairs in step with.

## D24 — Ranking for complete recall: name evidence first

**Adopted**, 2026-09-27. D23's name index recalls every name a query can match, and its
46 losses were all ranking: weaknesses the capped nets hid by never holding the
competitor. Each fix below is its own commit, measured with `make recall` against the
commit before it, under both `RQ_RECALL=scan` and the FTS nets, with `--anchored`. An
offline re-ranker over `--explain` output (every hit, unrounded) reproduced the harness
exactly and screened the variants; the chosen one was then measured for real.

### 1. Definition shape scales with match quality

*The weakness.* `kind`, `extent`, `path`, `depth` and `private` were sized as
tiebreakers between exact matches (1000), where they are small. On a fuzzy match, whose
evidence runs from about 30 to 200, they decide: a class carries extent, kind and a file
named after it (~65), a method about 11. `fipuno` answered `BackfillPushNotificationLevel`
(fuzzy 65) over `find_published_node` (109), and `lclztn` sank `localizations` to #51 under
Localization* classes.

*The rule.* Each of these is multiplied by `match_quality` (exact 1.0, prefix 0.9, glob
0.7, fuzzy 0.30–0.65, path-only 0.25). They keep full weight between exact matches, and on
a fuzzy match they shrink to about a third: still enough to order names that read as the
query about equally well. A path-only hit's `path` is its evidence and stays whole.

| 2,314 sourced | scan #1 | top 10 | found | FTS #1 | top 10 | found |
|---|---|---|---|---|---|---|
| before | 1,218 | 1,677 | 1,991 | 1,133 | 1,602 | 1,886 |
| scaled | 1,280 | 1,716 | 1,991 | 1,190 | 1,624 | 1,886 |

Scan: 373 up, 73 down. Anchored call sites unchanged (351 #1, 439 top 10 with
`--anchor`), in both modes: they are exact matches, where the scale is 1. Of the 13 lost
#1s, 12 are ambiguous by name — the new #1 reads as the query at least as well
(`fity`: `file_type` 96 over `field_type` 92; `docbaspac`: `docker_base_packages` 186 over
`dockerfile_base_packages` 170; `chngls`, `fndprv`, `mgrtps` and the rest), and the
side features that used to carry the source are now too small to. The 13th, `subtes`, is
the test-path cliff (part 2). Of the 5 lost top 10s, `chatest` is the cliff, `tstlft` and
`tstdlg` are ambiguous by name, and `tstwhr` and `dscvry` are within 5 points of name
evidence, which side features still decide. FTS loses the same queries plus `deauco`
(ambiguous: `DEFAULT_*` constants read as `deauco` better than `DECRYPTED_AUTH_COOKIE`).

*Rejected, screened offline under scan:*
- **Weighting the name instead** (fuzzy × 2: 1,259 #1, 15 lost; × 3: 1,279, 20 lost). A
  constant is the same scale without the grading, and it moves the fuzzy value that
  `match_quality` and confidence read.
- **Capping side features at a share of the fuzzy value** (0.25–0.75): 1,229–1,259 #1,
  and a cap is a kink where the scale is a line.
- **Dropping `path` on a name match** (it restates a class's name): 1,228 #1, 37 lost
  top 10s. The file still helps.

### 2. An approximate match in a test gives up a share of its evidence

*The weakness.* A definition under a test path took a flat −400, sized to clear the gap
between an exact and a prefix match. Complete recall almost always holds some non-test
name with the query's letters in order, however weak, so the cliff put it first:
`coclfi` answered `delete_action_cable_files_skipping_action_cable` (fuzzy 15) over
`ConditionalClassFilter` (103), `tefofo` `protect_from_forgery` (58) over `test_form_for`
(130). D23 counted 22 such losses.

*The rule.* A fuzzy or typo match in a test loses 0.4 × its name evidence, capped at 400;
a literal match (exact, prefix, glob, constructor) and a path-only hit keep the cliff. To
outrank a match outside tests, a test definition must read as the query 1.67× better.

This is D12's rejected formula, and it now works because of part 1. D12 found it flooded
fuzzy results with test definitions: scaling a test match's evidence by 0.6 compresses the
name differences among tests, and full-weight side features then decided between them.
Screened offline, without part 1 the share gives 1,232 #1 and 35 lost top 10s; with it,
1,303 and 7. A smaller flat penalty for fuzzy matches (40–100) scored about the same, but
isn't graded: it treats a test match that reads as the query perfectly and one that barely
does alike.

| 2,314 sourced | scan #1 | top 10 | found | FTS #1 | top 10 | found |
|---|---|---|---|---|---|---|
| before (part 1) | 1,280 | 1,716 | 1,991 | 1,190 | 1,624 | 1,886 |
| share | 1,303 | 1,741 | 1,990 | 1,207 | 1,640 | 1,885 |

Scan: 134 up, 33 down; one lost #1 and two lost top 10s. `ststmico` and `tstcrr` are
near-ties among test definitions (168 against 164, 97 against 96), where the share's
compression lets extent decide. `twedele` loses `tweedle` from its results: every
first-pass hit used to score ≤ 0 under the cliff, which let every near miss join (D14's
fallback); now `tweedle_deedle` scores above zero and sets the bar. Part 3 is that gate.
FTS: no lost #1, the same two top 10s. Anchored unchanged: their truths are exact matches.

### 3. Every near miss joins, and ranks on its score

*The weakness.* D14 let a near miss join only if its name evidence was at least the best
in-order match's, because kind, extent and recency otherwise carried weak guesses in
(`shft` → `Sheet`). With nothing above zero, every near miss joined. Complete recall
nearly always holds a name with the letters in order, so the bar rarely fell: `tets_br`
dropped `test_br` (typo 102) because `test_sub_regions` holds the letters at 114, and
after part 2 `twedele` dropped `tweedle` the same way. The old fallback had been doing
the work only because the test cliff pushed every first-pass hit below zero.

*The rule.* Without a literal hit, every near miss joins and ranks on its total. Part 1
made that total compare name evidence first, which is what the gate was for; a near miss
that reads worse than an in-order name now ranks below it rather than vanishing.

| 2,314 sourced | scan #1 | top 10 | found | FTS #1 | top 10 | found |
|---|---|---|---|---|---|---|
| before (part 2) | 1,303 | 1,741 | 1,990 | 1,207 | 1,640 | 1,885 |
| no gate | 1,303 | 1,750 | 2,000 | 1,207 | 1,648 | 1,894 |

No source lost #1 or the top 10 in either mode, and no hand-picked query's #1 changed
(D14's `shft`, `updget`, `cmlz` among them). 10 sources move down, by one to eight
places as near misses join the list: two stay in the top 10 (`updget` #3 → #8, `fedete`
#5 → #6), and the rest were already past it. `tets_br` and `twedele` find their sources again. Anchored
unchanged.

### 4. A leading sigil is read as typed

*The weakness.* `align` reads only a query's letters and digits, so `_dshrz` is `dshrz`,
and the public `dasherize` (fuzzy 83) outranked `_dasherize` (62): its `d` sits at index
0 and earns the start bonus, where `_dasherize`'s sits at 1. Typing the underscore is a
deliberate request for the internal name.

*The rule.* When the query and a name begin with the same run of non-alphanumeric
characters, the fuzzy value aligns both without it and adds 10 per character, what `align`
credits one matched letter. `_dasherize` scores 92 against `dasherize`'s 83. It changes the
value only, never what matches, so the name index is untouched: the exhaustive
index-against-`score` test still agrees on every harness query and name.

*Result.* Scan: `_dshrz` back to #1 (1,303 → 1,304), nothing else moved. FTS: no change,
as its capped net never held `dasherize` for this query. Anchored unchanged.

*Rejected:* a penalty on names *without* the sigil. It says the same thing about the
query as a bonus does, charged to every other candidate, and needs a size of its own.

### The default flips to the name index

With parts 1–4 in, `make recall BASE=main --anchored`, main's default (FTS) against the
new default (the name index):

| 2,314 sourced | source #1 | top 10 | found |
|---|---|---|---|
| main, FTS | 1,133 (49.0%) | 1,602 (69.2%) | 1,886 (81.5%) |
| D23's scan, before D24 | 1,218 (52.6%) | 1,677 (72.5%) | 1,991 (86.0%) |
| default now | 1,304 (56.4%) | 1,750 (75.6%) | 2,000 (86.4%) |

481 up, 119 down. Anchored call sites unchanged (351 #1, 439 top 10 with `--anchor`).
`--bench 5` on the hand-picked queries, load 9–25: query phase median 7.1 → 1.9 ms, first
answer 9.1 → 3.2 ms; wall is the git check after the answer, as in D23. The FTS path gains
too (1,207 #1, 1,648 top 10), so the escape hatch isn't a regression.

The bar was no lost #1 or top 10 against main unless explained. 14 lose #1 and 6 the top
10, every one read:
- **Ambiguous by name, 15.** The new #1 reads as the query at least as well as the source;
  the capped nets never held it. `fity`: `file_type` 96, `field_type` 92. `docbaspac` and
  `dobapa`: `docker_base_packages` over `dockerfile_base_packages`. `cmpths`: `compute_has_more`.
  `chngls`: `changeLightScheme` 116, `changeListener` 113. Likewise `tstlzy`, `rcrsvl`,
  `chse`, `fltrty`, `ensy`, `mgrtps`, `fndprv`, `chatest`, `tstdlg`, and `tets_br`, where
  `test_sub_regions` holds every letter in order (114) against the transposition's 102 —
  D14's `twedele` case; `test_br` is #2 rather than gone.
- **Near ties decided by side features, 3.** `tstwhr` (94 against 95), `dscvry` (78
  against 83, the source private) and `deauco` (105 against 107, among several `DEFAULT_*`
  names that read as well). Deciding these is what the scaled features are for.
- **The test rule doing its job, 1.** `hscstm`: `has_custom_context?` (95) outranks the
  page-object helper `has_custom_label?` (97) under `spec/`.
- **A residual weakness, 1.** `has_icno?`: the test helper `has_icon?` (typo 128) keeps
  0.6 of its evidence, 77, and `AdminPluginsShowHouseAdsIndexController` (fuzzy 74) passes
  it on kind and extent. A class holding a query's letters that scattered shouldn't score 74;
  that is `align`'s value, not a ranking feature, and is left for its own change.

*Not yet done:* the FTS table and triggers stay. D25 closed the gaps that fell back to them:
recall rebuilds a missing or stale index before reading it, and verifies a suspended one's
rows directly.

*Reverses if:* test definitions start displacing library ones on real use (the share is
the lever: raise it toward the cliff), or the harness's sampled sources stop resembling
what people navigate to — it samples test and library names alike, which favours
anything that softens the test penalty.

## D25 — The name index always answers; its storage audited

**Decided**, 2026-09-27. Release builds, rails and discourse at the recall pins, every
timing interleaved against main (1ad2e79) on a shared machine (load 5–17): 396 harness
queries (every sixth), 3–5 reps. Sizes from `dbstat` on a fresh rails + discourse index.

*What.* Recall no longer falls back to the FTS nets when a repo's index isn't current:
- **Missing or from another format: rebuilt, then read.** One query per search finds the
  repos in scope that are behind; each is rebuilt before the scan. That is once per repo
  after an upgrade or a format change: 62 ms for rails, 103–110 for discourse, after which
  recall is back to ~1 ms. `--all-repos` rebuilds only the repos that need it, where it used
  to fall back for all of them if any one did.
- **Suspended by a cold pass: verified from its rows.** Suspension is now a marker in
  `name_index` (and drops the repo's chunks) rather than a missing row, so it isn't
  mistaken for an upgrade and rebuilt mid-pass. Recall reads the repo's distinct names and
  file paths from `symbols` and `files`, signs and verifies each: complete over what's
  committed, where the FTS fallback saw none of the pass's rows (their FTS sync is deferred
  too). It costs 55–59 ms at full rails size and 81–120 at discourse, only while the pass
  runs; the warming poll mostly waits for an exact or prefix match or the pass's end anyway.
  *Amended in 0.55.1:* "only while the pass runs" wasn't so for a pass killed before its
  end: the marker stayed, and every `-a` search paid the row read (252 ms at discourse,
  against ~2) until a pass inside the repo rebuilt it. The marker now records the pass's
  pid, and recall rebuilds an index whose pass is gone, as it does a missing one. The warm
  lock couldn't say this: an explicit `--index` doesn't take it.

The FTS nets now run only if the rebuild fails (a writer held the lock past the busy
timeout), or under `RQ_RECALL=fts`.

| 396 queries, ms | main | this |
|---|---|---|
| recall median / p90 / p99 | 0.88 / 2.25 / 8.51 | 0.89 / 2.25 / 8.08 |
| first answer median / p90 / p99 | 1.85 / 4.16 / 14.34 | 1.85 / 4.16 / 14.69 |

`make recall BASE=main --anchored`: 0 sources moved, the top 10 changed in 0 of 2,372 queries, anchored unchanged (351 #1, 439 top 10). Database 36.62 → 36.61 MB. Unscoped (`-a`) on a three-repo index (rails, discourse, rq), 198 queries: recall 2.75 /
5.28 ms median / p90 against 2.76 / 5.32, top 10s identical.

*Rejected: appending through a cold pass instead of suspending.* The obvious fix: a
cold pass that appends like any other write is always current, with no second path. It
costs the pass nothing (rails cold index, 7 reps: 790 ms median against 781), but recall's
median went 0.88 → 1.03 ms on every rerun. Crossing binaries and databases put all of it
on the database: `name_sigs` sat in 135 runs of contiguous pages against main's 10, because
each append rewrites the last chunk between batches of symbol pages, and the rebuild at the
pass's end reuses those scattered pages. Skipping that rebuild left 127 runs and 1.00 ms.
A rebuild over a contiguous index stays contiguous (10 runs after one), so main's layout
survives normal use. A first version of this change also cost 0.03 ms of median in extra
per-search queries (a join against `name_index` in the scan, a separate lookup for
suspended repos); one query in `ensure_name_index` took it back to flat.

*Where the bytes are* (rails + discourse, 36.6 MB, before this change and unchanged by it):

| object | MB | |
|---|---|---|
| `symbols` | 12.4 | |
| `name_sigs` | 7.1 | signatures 3.8 (names 3.1, files 0.7), keys 3.1 (names 2.0 with 0.3 of offsets, file paths 1.1), page overhead 0.2 |
| `idx_symbols_repo_name` | 4.0 | |
| `idx_symbols_name_lower` | 3.8 | read only by the FTS fallback's unscoped first-letter net |
| `symbols_fts_*` | 3.7 | postings 2.5, docsize 1.3 |
| `files` + its key | 3.9 | |
| `idx_symbols_file` | 1.6 | |

*Ready, not done: dropping FTS and `idx_symbols_name_lower`.* With this change nothing
needs them but `RQ_RECALL=fts` and a rebuild that failed on a lock. Together they are 7.5
MB, 20% of the database and more than the name index costs. Vacuumed for a like-for-like
size, the rails + discourse index goes 35.2 → 31.2 MB without FTS and → 27.8 without the
index too (a migration's drop frees pages for reuse rather than shrinking the file).
Without the first-letter net nothing reads that index: every other name query joins
`repositories` and seeks `idx_symbols_repo_name` per repo, and the plans are identical with
and without it. Removed in D26.

*Rejected, measured:*
- **A 128-bit pair Bloom** (24-byte signatures). −1.5 MB (−4.3%), identical answers, but the
  screen passes 55% more names (1.29M → 2.00M over the harness) and the tail pays for it:
  recall p90 2.02–2.29 → 2.27–2.88 ms and p99 +0.4–0.9 over two runs, median flat. 64 bits:
  −2.3 MB, 2.9× the survivors, twice the in-memory scan.
- **Compressing chunks.** zlib takes the keys from 3.1 to 0.9 MB, but 46% of chunks hold a
  survivor for the average harness query, ~36 inflates a query against a ~0.9 ms recall.
  Signatures compress only to 59% and every scan reads all of them.
- **Front-coding names** (sorted within a chunk): 1.7 → 1.1 MB of name bytes, for a
  sequential decode of every chunk with a survivor and a chunk order that appends break.
- **Indexing only files with a primary definition**: 5,908 of 17,720 files hold none and
  fetch nothing, ~0.6 MB, but a file that gains a class later needs its own append
  bookkeeping. Storing a file's stem and id instead of its path saves ~0.75 MB and has to
  guard against ids SQLite reuses.

*Reverses if:* a cold pass's writes stop interleaving with the index's pages (the chunks
move to their own file or database), which would make appending through it free; or the
row verify's cost during a cold pass starts to show on real repos, where a rebuild on
demand before the pass ends is the next step.

## D26 — FTS removed: the name index is the only fuzzy recall

**Adopted**, 2026-09-27. Release builds, rails and discourse at the recall pins.

*What.* Schema v18 drops `symbols_fts`, its three sync triggers, and
`idx_symbols_name_lower`; the code that read and maintained them goes with it: the
first-letter, trigram and path `LIKE` nets, the in-SQLite `rq_keep` filter and
`score::could_match` behind it, the indexer's deferred FTS sync, and `RQ_RECALL`. The
migration drops the triggers first (one left behind would fail every symbol write) and
builds nothing: recall rebuilds each repo's name index on first need (D25). Every name query
left is scoped by repository and seeks `idx_symbols_repo_name`; unscoped (`-a`) exact and
prefix queries scan `repositories` and seek it per repo, the same plan as with the dropped
index.

*A rebuild that can't take the lock reads the repo from its rows.* D25 kept the FTS nets
for one case: another writer holding the lock past the 3 s busy timeout when recall needs
to rebuild. Three ways to answer that without them:
- **Propagate the error** (exit 74). A spurious failure: the answer is in the database,
  and a caller told `database` has nothing to fix.
- **Report warming** (exit 2). Honest that a retry will do better, but it withholds an
  answer rq can give, and `warming` means the index is incomplete, which it isn't.
- **Verify the repo's names from its rows**, as recall already does for a repo whose cold
  pass suspended its index. Complete over what's committed, 55–120 ms at rails and
  discourse size, and the rebuild is left to the next search.

The third, since it's the same answer by an existing path. Only a busy or locked error
takes it; any other error from the rebuild propagates and exits 74.

| rails + discourse, fresh index | file | vacuumed |
|---|---|---|
| 0.54.1 (FTS, no name index) | 29.7 MB | 28.0 MB |
| main (FTS and the name index) | 36.6 MB | 35.2 MB |
| this (the name index alone) | 29.1 MB | 27.8 MB |

A database migrated in place keeps its file size. From main the drop frees 1,836 pages
(7.5 MB) for reuse; from 0.54.1, whose users skip v17, the name index is built into those
pages and the file stays at 29.7 MB with 135 pages free. An older rq on a migrated
database fails any query that reaches fuzzy recall (`no such table: symbols_fts`, exit
74) and its first index of a repo; exact and prefix matches still answer.

`make recall BASE=main --anchored`: 0 sources moved, the top 10 changed in 0 of 2,372
queries, anchored unchanged (351 #1, 439 top 10 with `--anchor`).

| 396 queries × 5 reps, interleaved, load 14–27, ms | main | this |
|---|---|---|
| recall median / p90 / p99 | 1.55 / 4.18 / 14.08 | 1.52 / 4.18 / 13.91 |
| first answer median / p90 / p99 | 3.51 / 8.75 / 24.93 | 3.43 / 8.53 / 25.51 |

Flat, as expected: the nets never ran by default, so removing them changes no query's
work. Output was byte-identical on every query.

*Reverses if:* the name index has to be rebuilt somewhere recall can't afford to wait for
it, or a query shape appears that it can't screen, and a fallback earns its storage back.

## D27 — A scope no parent records is read off the file's path

**Adopted**, 2026-09-27. Language testers on hugo, gin, django and tokio; recall harness
(D12) against main.

*The weakness.* A qualifier matched only a recorded `parent`. Ruby and Python classes
record theirs, but no language records its packages or modules as one, so every scope
spelled as a package failed with `scope_not_found`: Go's `hugolib.HugoSites`,
`gin.Context` and `page.Site`, Python's `django.db.models.QuerySet` and `models.QuerySet`,
Rust's `mpsc::Sender`. `found_in` then named the bare name's #1, which for `mpsc::Sender`
was oneshot's `Sender`, and for a leaf that only matched fuzzily, some other name.

*The rule.* Where the parent doesn't answer the whole scope, the innermost segments it
does hold count as before (`parent`), and the rest must appear in order in the file's
path: the repo's name, its directories, the file's stem (`path_scope`). In order, not
contiguous, so `tokio::sync::mpsc` skips `src`; joiners are ignored, so `tokio_util` finds
`tokio-util/`. It is 30 per segment, half an owner segment, and halves per directory
between the scope's innermost segment and the file. The scope gate then keeps only the
best-scoped results, where it used to keep those with any `parent`: a parent over a
path, and a scope's own directory over its subdirectories. That decides `gin.Default`:
the repo name puts every file in gin inside `gin`, but only `gin.go` is directly in it,
where `binding.Default` (extent 33 against 18) used to win. `found_in` reports only an
exact definition of the leaf. Nothing here is per language: it reads the stored path and
parent, as `--anchor`'s `proximity` does.

| per-language set, 31 queries | #1 before | #1 after |
|---|---|---|
| all | 13 | 27 |
| qualified (17) | 1 | 17 |

`make recall BASE=main --anchored`: 0 sources moved, the top 10 changed in 1 of 2,372
queries, anchored unchanged (351 #1, 439 top 10). The one is a hand-picked query,
`ActionDispatch::Routing.draw`, which found nothing and now finds `RouteSet#draw` in
`action_dispatch/routing/route_set.rb`. Dogfooding trekr's source turned up the same miss for a Rust
file stem: `files::key` failed with `found_in` naming a Ruby stub's `Hash#key`, and now
finds `key` in `src/tree/files.rs` at confidence 1.00.

*Rejected:*
- **The innermost scope must be the file's own directory or stem.** Exact for Go, where a
  package is its directory, but Rust and Python re-export: `tokio::net::TcpStream` lives
  in `net/tcp/stream.rs`. Graded distance ranks the direct one first without losing the
  re-export.
- **Contiguous segments.** `tokio::sync::mpsc` would need `src` spelled out.

*Limits.* A crate or package whose name isn't a directory (`grep_searcher` in
`crates/searcher/`) isn't found by it. A language that records modules as parents would
answer through `parent` and never reach this.

*Reverses if:* the path puts wrong definitions inside a scope in real use (a directory
that happens to share a scope's name); then require the innermost segment to be close,
not merely present.

## D28 — Generated code takes the test penalty

**Adopted**, 2026-09-27. Language testers' corpora; recall harness (D12) against main.

*The weakness.* In hugo, `rq String` returned ten stringer files (`kind_string.go`,
`// Code generated by "stringer"; DO NOT EDIT.`) before any of the 102 hand-written
`String` methods: every one is an exact match, and the filename adds `path` 25. Generated
code is a definition like any other, and sometimes the only one (a protobuf message), but
beside a hand-written one of the same name it is rarely the one meant.

*The rule.* The indexer reads a file's first 20 lines for the tools' own marker: a comment
holding `@generated`, or both "generated" and "do not edit" (Go's standard line, protoc's,
`// GENERATED CODE -- DO NOT EDIT!`). A line starting with a letter or a quote is code, so a
generator printing the marker isn't marked. The flag is `files.generated` (schema v19), and
a generated definition takes the test-path penalty under its own name, `generated`: −400
on a literal match, 0.4 of the name evidence on a fuzzy one. One concept, secondary code,
with one size; a generated file under a test path takes it once.

Marked: hugo 11 files (all 11 carrying Go's marker, no others), gin 1 (a `.pb.go` under
`testdata/`), excalidraw 2 (base64 wasm modules), django, tokio and ripgrep none.

| | per-language set #1 (31) | recall #1 / top 10 / found |
|---|---|---|
| before | 27 | 1,304 / 1,750 / 2,000 |
| after | 28 (`String`) | 1,304 / 1,750 / 2,000 |

Recall: 0 sources moved; anchored unchanged (351 #1, 439 top 10). Reading the header costs
nothing measurable: a cold index of rails with one worker took 3.3 s of user CPU on both
builds, four interleaved runs each (wall time was unusable at load 33).

*The upgrade re-reads every file.* v19 forgets every file's stat and hash and demotes
complete repos to warming, as v14 did for three languages. The header can't be read from
the database, and a lazy backfill (the `end_line` and `visibility` precedent) would leave
the flag unset on an existing index until each file changed, which for generated files is
rarely.

*Rejected:*
- **Filename conventions** (`_string.go`, `.pb.go`, `_pb2.py`, `.generated.ts`). Per
  language and per tool, and in the core; the header marker is what the tools agree on.
- **A penalty of its own size.** Nothing distinguishes how secondary generated code is from
  how secondary test code is; a second constant would be tuned against nothing.
- **No `path` bonus for a member** (a method or function with a parent), on the theory
  that a file is named after a type, not its methods: `String` in `kind_string.go`
  belongs to `Kind`. Against this change: #1 unchanged, top 10 1,750 → 1,748, 21 sources
  up and 11 down, 3 lost top 10s, plain anchored top 10 432 → 431. In Ruby a file is
  named after the module that owns a method, and shares its words:
  `content_security_policy?` in `content_security_policy.rb`, `polymorphic_mapping` in
  `polymorphic_routes.rb`. The generated penalty already settles the stringer case.

*Reverses if:* a hand-written file carries the marker in its header (then require the
marker's line to be the whole comment, Go's regexp), or generated code turns out to be the
target people navigate to while a hand-written namesake exists.

## D29 — Example, demo and docs apps take the test penalty

**Adopted**, 2026-09-27. Language testers' corpora; recall harness (D12) against D28.

*The weakness.* In excalidraw, `rq Excalidraw` ranked the library's
`export const Excalidraw = React.memo(…)` (`packages/excalidraw/index.tsx`) third, behind a
Next.js example's local `dynamic()` wrapper (`examples/with-nextjs/…`, `path` 45) and a docs
site's scaffold (`dev-docs/…`, extent 22). Both are private, but `private` is −15, a
tiebreaker. An example is code that shows the library rather than being it, which is what
the test penalty already says about tests.

*The rule.* A definition under a directory segment `examples`, `example`, `_examples` (Go's
ignored-directory convention), `demo`, `demos`, `docs` or `dev-docs` takes the test-path
penalty as `example_path`, after `test_path` and `generated` (one penalty at most). Not
`doc`: ripgrep's `crates/core/flags/doc/` and tokio's `src/doc/` are library code.

In the testers' corpora that is 20 files in tokio's `examples/`, 12 in excalidraw's
`examples/` and 10 in `dev-docs/`, ripgrep's three `crates/*/examples/`, django's sphinx
extensions under `docs/`, rails' two `activerecord/examples/` scripts and eight
discourse files (`docs/developer-guides/`, and spec helpers under `spec/**/examples/`,
already test code).

| | per-language set #1 (31) | recall #1 / top 10 / found |
|---|---|---|
| before (D28) | 28 | 1,304 / 1,750 / 2,000 |
| after | 29 (`Excalidraw`) | 1,304 / 1,750 / 2,000 |

Recall: 1 source moved down within the top 10, top 10 changed in 2 of 2,372 queries;
anchored unchanged (351 #1, 439 top 10).

*Rejected: a heavier `private` (−15 → −30).* Asked for alongside, since both wrappers are
private and the library's component is exported. Against this change: #1 1,304 → 1,292,
top 10 1,750 → 1,746, 14 lost #1 and 9 lost top 10s. Visibility starts deciding between
names that read as the query about equally well: `polmap` finds `polymorphic_mappings`
over the private `polymorphic_mapping`, `quopar` `QuoteParamsPattern` over `quota_params`.
A tiebreaker is what `private` measures well; the example directory is the evidence here.

*Reverses if:* a library keeps its real code under one of these names (a `docs` package
that is the product), or example code turns out to be what people navigate to beside the
library's own definition.

## D30 — Upper-snake abbreviations: diagnosed, not changed

**Rejected**, 2026-09-27. Recall harness (D12) against D29.

*The report.* In excalidraw, `fontfam` never finds `FONT_FAMILY`, where `EVT` finds `EVENT`.
It read as `align` not crediting `_` boundaries the way it credits camel humps.

*Two causes, neither that one.* `_` does start a word (`boundaries` marks the letter after
any separator), so `FONT_FAMILY` and `FontFamily` have the same word starts.
- **`fontfam` is a literal prefix** of `FontFamilyHeadingIcon` and six other camelCase
  names, and not of `font_family`, since a prefix keeps its separators (D19). A prefix hit
  suppresses the fuzzy tail, and `FONT_FAMILY` is only fuzzy. That is D19's rejected
  "separators left out of a prefix too", whose numbers stand: it trades found queries
  (`setview` then finds only `set_view_paths`, never `setup_view`), and here it would also
  have to run fuzzy recall under every prefix hit, since no store range holds `font_family`
  for `fontfam`.
- **Across `_`, the step is a gap.** For `fntfam`, `T→F` is contiguous in `FontFamily`
  (+10) and a one-character gap in `FONT_FAMILY` (−3), 13 points, so `FONT_FAMILY` (fuzzy
  129) ranks fifth behind `FontFamilyKeys` (139).

*Measured: a separator is not a gap.* `align` scoring a step across separators alone as
contiguous: #1 1,304 → 1,299, top 10 1,750 → 1,748, 48 up and 52 down, 11 lost #1.
The gap is doing work: `forlin` → `bullet_for_li_node` over `format_line`, `stpo` →
`set_pool` over `StatementPool`, `addmmb` → `add_members` over `addMembers`. Crediting
a run that continues into the next word rewards reading a query across a word break the
user didn't type. Scoring only, so the name index (D23) would have been untouched.

*Reverses if:* a corpus where upper-snake constants are the usual target shows the gap
costing them first place (then try it for all-caps names alone, which needs its own
harness); or D19's prefix reversal is taken up on its own merits.

## D31 — The anchor's own file isn't secondary to itself

**Adopted**, 2026-09-27. Anchored call sites (D18) and the recall harness, against D29.

*The weakness.* With `--anchor tests/helpers/api.ts:120`, the method defined on that line
ranked #3. It took the test penalty (−400), and the anchor can add at most 270 (`proximity`
90, `enclosing` 180), so a library definition of the same name always won. Asked from
inside a test, that test file's definitions are the context, not a distraction from it.

*The rule.* A candidate in the anchor's own file takes no secondary penalty (`test_path`,
`generated`, `example_path`). Only that file: the rest of the test tree keeps its penalty,
and `proximity` already grades how near it is.

| 446 call sites, with `--anchor` | #1 | top 10 | #1, truth in anchor file | #1, truth elsewhere |
|---|---|---|---|---|
| before (D29) | 351 | 439 | 258 | 93 |
| waived in the anchor's file | 359 | 439 | 270 | 89 |

172 up, 14 down. The four new losses are all Ruby tests that define a same-named fake in
another class of the file and call the library's version (`unknown` in
`broadcast_logger_test.rb`, `build_from_database` twice in `attribute_set_test.rb`,
`decode_credentials`): D18's known limit, a call on another class asked as a bare name
from a file that defines the name. Unanchored recall is untouched (0 sources moved).

*Rejected: halving the penalty in the anchor's file.* 356 #1 (265 in the anchor's file, 91
elsewhere): half the losses and under two thirds of the gain. −200 still outweighs
`proximity` plus one level of `enclosing` (150), which is where a helper in the test's own
class sits.

*Reverses if:* anchored use from tests shows the in-file fakes winning over library calls
more often than the in-file helpers they were meant to find.

## D32 — A literal match no longer hides the other case convention

**Adopted**, 2026-09-27. Recall's fast path skips the name index once an exact or
prefix match exists, since the relevance gate would drop every fuzzy candidate anyway.
It also dropped the one non-literal candidate that scores as exact: the same identifier
in the other case convention. `joiners_eq` scores `AbortHandle` as `exact` −
`separators` for `abort_handle`, but only if something fetches it, and `name_lower`
has no row `abort_handle`. So on tokio `abort_handle`, `join_handle` and `async_read`
never showed the type, and in rq `active_files` never showed `ActiveFiles`. The fast
path now also scans the name index for names `joiners_eq` accepts and fetches only
those rows.

`make recall` against the commit before it: 31 sources up and 2 down. Rust goes
81.0 → 81.1% #1, 88.7 → 89.4% top 10 and 89.3 → 90.0% found. The `case` type goes
88.4 → 90.1% #1 and 88.9 → 95.1% top 10. Ruby's derived queries didn't move
(56.4 / 75.6 / 86.4).

The anchored set did move, and it exposed the price. Ruby's `schema_creation` went #1
→ #5 plain, because five `SchemaCreation` classes each sit in a `schema_creation.rb`.
The path bonus (50) plus extent (up to 50) and kind (15) outweighed `separators`
(−50). The same happened on tokio, where `chunks_timeout` and `buf_writer` lost #1 to
`ChunksTimeout` and `BufWriter`. Anchored #1 went 213 → 208 plain and 351 → 348
with `--anchor`. So `separators` is now −150, more than those three can add
together. The spelling the query typed wins whenever it exists, and the other
convention ranks right behind it. Measured with the test-scope change (D33, which
moves no Ruby query) applied: anchored is back to main's 213 / 351 / 432, with 438
top 10 against main's 439 (`url_for` in `routing_test.rb`, #8 → #11 with `--anchor`, behind `UrlFor`
modules outside tests). Derived queries moved 1 up and 1 down, and none lost #1 or the
top 10. The unit test that pins `parsefile` → `parse_file` over `parse_files` still
holds: 850 against a prefix's 699.

*Considered and dropped:* doubling the penalty only when the query typed a separator
the name lacks. It is a condition where a size will do, and Ruby's `schema_creation`
needs the larger gap either way.

Latency: the fast path now pays one name-index scan. Literal queries on rails
(`save`, `find_by`, `render`, `ActiveRecord`, single runs) went from 0.3–2.2 ms to
0.7–1.9 ms in recall. The hand-picked bench (5 reps) shows a query-phase median of
2.0 ms for both, with p90 10.6 → 12.5 ms.

*Reverses if:* the scan shows up in first-answer latency on a large repo. A
`name_joined` column indexed like `name_lower` would then make this a seek.

## D33 — Tests beside the code take the test penalty

**Adopted**, 2026-09-27. The test penalty (D12, sized in D24, widened by D28 and D29)
reads the path and a generated header, so Rust's unit tests never got it: they live in the file they test, inside `mod tests`.
In the dogfood set, `braboost` found `branch_boost_adds_to_the_score` ahead of
`BRANCH_DIR_BOOST`. `rq clock` answered with `tests · clock`, the test module a path
match surfaced as clock.rs's primary definition. A definition now counts as test code
when a segment of its parent scope is `tests`, `test` or ends in `_tests`, or when it
is a module of that name. It is one more arm of the secondary-penalty chain, under the
feature `test_scope`: at most one secondary penalty applies, and D31's anchor-file
waiver covers it. The rule is about scope names, not Rust. It applies to
any plugin that records a lowercase test scope as a parent. Only lowercase segments
count, so Ruby's `Minitest::Test` and `ActiveSupport::Testing` are untouched. In the
Rust corpora the in-file test modules are named `tests` (107), `test` (2) and
`*_tests` (7, in trekr).

`make recall` against D32: 21 sources up and 7 down. rq goes 90.4 → 90.8% #1, trekr
87.3 → 87.4% and tokio 73.3 → 73.0%. Ruby didn't move, and neither did the anchored
set. All five queries that lost #1 and both that lost the top 10 have a source that is
itself test code outside a `#[cfg(test)]`-gated file tail. tokio's `run_test` and
`test_slot_for` live in `#[cfg(all(test, …))] mod tests`, which the query derivation
counts as source. These are the losses D12 accepts. The full dogfood set (7,152
queries from the last 300 commits of rq and trekr) went 85.9 → 86.2% #1 (39 up, 6
down) with no loss of #1. `glob` gained most, 83.2 → 85.2%.

*Reverses if:* a language routinely names non-test scopes `test` or `*_tests` in
lowercase.

## D34 — A Rust module file as a module symbol: measured, not adopted

**Rejected**, 2026-09-27. `mod store;` is a pointer, and Rust extraction rightly skips
it. But nothing takes its place, so a module has no symbol. `rq store` finds only
`Store`, and before D33 `rq clock` found `tests · clock`, a path match surfacing the
file's test module. The trial emitted one `module` symbol per module file: `store`
for `src/store/mod.rs` or `src/store.rs`, spanning the file, with no parent, and
skipping `main.rs`, `lib.rs`, `build.rs` and comment-only files.

`make recall` against D33: 209 sources up and 91 down. Rust went 81.2 → 84.2% #1,
89.3 → 93.2% top 10 and 90.0 → 93.9% found, and `exact` went 90.7 → 96.9%. Ruby, the
anchored set and the regress cases didn't move. Much of that gain is ground truth
that counts `mod x;` as a definition of `x`. The 41 that lost #1 are one shape: a
type and the module file named for it (`frame.rs` holding `Frame`), where the
module's whole-file extent carries it.

It also broke three of `ranking_aspirations`: `store` → the module over `Store`,
`search` → the module over `fn search`, and `budgeted` → the `budgeted_index` module
by prefix over `index_budgeted`. A one-line span (no extent) fixes the first two. But
every file stem then becomes a name that exact and prefix matches can reach, so
`budgeted` still lands on the module. Keeping it would take a rule that a file's
module ranks below the definitions in it. That is an exception, not a signal, and the
aspirations say the definitions are what people want.

*Reverses if:* modules get a container notion in the model (a symbol that other
symbols in the file belong to), so "the definition over its container" is a real
signal rather than a special case. Or daily use shows module names being searched for
and missed (DOGFOOD.md).

## D35 — `pub(crate)` takes no visibility penalty: measured, left out

**Rejected**, 2026-09-27. The Rust tester found tokio's internal `pub(crate)
block_on`s ranked above the public `Runtime::block_on` and `Handle::block_on` (#7 and
#8 of 12). Only Rust emits `crate` visibility, and today it carries no penalty.
private and protected get −15. Three settings, measured against D33 with the same
binary through an environment switch:

| crate / private | Rust #1 | Rust top 10 | Ruby #1 | up / down | `block_on` |
|---|---|---|---|---|---|
| 0 / 15 (kept) | 81.2% | 89.3% | 56.4% | | #9 |
| 8 / 15 | 81.2% (+3) | 89.4% | 56.4% | 36 / 28 | #7 |
| 15 / 15 | 81.1% (−5) | 89.4% | 56.4% | 56 / 52 | #7 |
| 30 / 40 | 80.6% | 89.3% | 55.5% | 133 / 172 | #4 |

None fixes `block_on`. `future/block_on.rs`'s `pub(crate) fn block_on` earns a path
bonus (33) and more extent, which is a bigger gap than any penalty a tie-breaker
should carry. The mild setting is +3 queries of 4,507, with 13 losing #1: churn, not
signal. Raising private too costs Ruby.

*Reverses if:* a signal lifts public API in general (re-exports at a crate root, say),
and `crate` becomes one input to it rather than a penalty of its own.

## D36 — Python's nested defs are indexed, as `local`

**Adopted**, 2026-09-27. Python tester on django; recall harness (D12) against main.

*The weakness.* A `def` body was never walked, so a closure had no symbol:
`_wrapper` in `_multi_decorate` (`django/utils/decorators.py`), the `_view_wrapper`s
of every view decorator, `decorator` inside `user_passes_test`. `rq _view_wrapper`
answered `no_match`.

*The rule.* A `def` inside a `def` is a `function` whose parent is the enclosing def's
qualified name (`_multi_decorate`, `Signal.send`, `user_passes_test.decorator`), at any
depth. A class or assignment inside a def is a local and stays out, as does everything
inside a local class. Its visibility is a new value, `local`: nothing outside the
enclosing body can reach it, which is narrower than `private`. `local` scores −150
(`local` in `--explain`) where `private` scores −15, sized as `separators` was in D32:
past what `path`, extent and kind can add together, so a nested def ranks below every
same-named definition outside a function body, above a prefix match (a literal name
still wins), and above test code (−400). An `--anchor` inside the enclosing def adds
back up to 270, so asked from there it still wins. The value is language-neutral; only
Python emits it today.

Recall is in D37, measured with it. django: 41,194 → 42,279 symbols (+1,085, of which
809 under `tests/`). 24 names are both a nested def outside `tests/` and some other
definition outside it. With the nested defs marked `private`, 4 of them ranked a nested
def above its namesake: `_save` (`LayerMapping.save`'s over `FileSystemStorage._save`),
`deconstruct` (`deconstructible.decorator`'s, carried by `path`, over
`Field.deconstruct`), `_compile` and `asend` (#2 and #3 ahead of methods). With `local`,
none do.

*Rejected:*
- **`private`.** The four above: −15 is a tiebreaker, and a closure's larger body or a
  matching filename outweighs it.
- **Local classes too.** django defines models inside test methods by the hundred; a
  local class is no more reachable than a local def, and nothing asked for it.

*Reverses if:* closures turn out to be navigated to in preference to a same-named
module-level definition (then drop `local` back to the `private` size).

## D37 — Go, Python and TypeScript adopt the shared kinds

**Adopted**, 2026-09-27. Language testers on hugo, gin, django and excalidraw; recall
harness (D12) against main.

*The weakness.* 0.56.0 gave the model `type` and `variant` for Rust, and the other
plugins still spoke the old vocabulary. Go dropped every named type that wasn't a
struct or interface: gin's `HandlerFunc` (`rq HandlerFunc` → `no_match`) and
`HandlersChain`, hugo's `type GitInfo = gitmap.GitInfo`. TypeScript labelled type
aliases `struct` and skipped enum members (`rq EVENT.MOUSE_MOVE` → `no_match`).
Python's enums were classes of constants, and lowercase members were missed.

*The rules.* All syntactic, each in its plugin:
- **Go:** any `type_spec` that isn't a struct or interface, and every `type X = Y`
  alias, is a `type`.
- **TypeScript:** `type X = …` is a `type`. Each enum member is a `variant` of its
  enum, with the enum's visibility; a quoted member loses its quotes.
- **TypeScript/JavaScript:** a binding whose value is a call whose first argument is
  (through nested calls) a function literal is a `function`: `memo((p) => …)`,
  `React.forwardRef(function F() …)`, `memo(forwardRef(…))`. The callee's name isn't
  read. `memo(BadgeBase)`, which wraps a name, stays a `constant`: the function it
  wraps is indexed under its own name.
- **Python:** a class with a base whose last dotted segment ends in `Enum`, `Flag` or
  `Choices` is an `enum`, and each name its body assigns (with a value, no leading
  underscore) is a `variant` of it.

| corpus | symbols before | after | what changed |
|---|---|---|---|
| gin | 1,639 | 1,674 | 35 `type` |
| hugo | 10,875 | 11,060 | 185 `type` |
| excalidraw | 4,818 | 4,887 | 559 `struct` → `type`, 40 `variant`, 65 `constant` → `function` |
| django (with D36) | 41,194 | 42,302 | 41 `enum` (15 outside `tests/`, all real enums), 129 `variant` |

`HandlerFunc`, `HandlersChain`, `goBinaryStatus`, `EVENT.MOUSE_MOVE` and `_view_wrapper`
now answer #1 (regress cases); hugo's `GitInfo` finds the alias first. Ranking barely
moves: `variant`, `function` and `constant` carry no kind weight, and `type` weighs 12,
so only a TS alias, 15 as a `struct`, loses 3. `make recall BASE=main` over this branch
(D36 included): 0 sources up, 1 down, the top 10 changed in 13 of 6,879 queries, none
lost #1 or the top 10. Ruby (56.4 / 75.6 / 86.4%) and Rust (81.2 / 89.3 / 90.0%) didn't
move, nor did the regress set (33 of 36 on both; four cases added here take it to 34 of
40 on main and 37 of 40 on this branch, `deconstruct` guarding D36) or the anchored set
(213 / 359 #1, 432 / 439 top 10). The recall corpora hold TS and Python too: the one
down is `res*fn`, discourse's TS alias `ResolveBlockFn`, #2 → #3, since `type` weighs 12
where `struct` weighed 15; the other changes are further down a top 10, mostly the
`script/*.py` nested helpers of rq and trekr (`rank`, `cut`, `dist`) appearing below the
Rust definition of the same name.

*D17's enum-member exclusion is reversed.* It rested on Rust not indexing variants
(it does since 0.56.0) and on collisions. In excalidraw, 8 of the 40 members share a
name with another definition; each ranks first only for its own spelling (`rq
UPDATE` finds `WS_SUBTYPES.UPDATE`, `rq update` the methods, per D32).

*Kept as they are:*
- **An interface prints as `trait`.** A per-language display name (`interface` in Go
  and TS) was weighed and turned down. `kind` is one vocabulary across text, JSON and
  `-k`; a mixed-language result list would print one kind two ways; a script
  grouping JSON by `kind` would have to know `interface` and `trait` are one; and a
  display table keyed by language is a special case per language at the edge. The
  input side already accepts `-k interface` and `rq interface X`, and the VS Code
  extension maps `trait` to its Interface icon. Aliasing text alone would make text
  and JSON disagree about the same hit. The skill and `--help` say `interface` =
  `trait`.
- **`@property`, `@classmethod` and `@staticmethod` stay `method`**, with no new field.
  A property is navigated to like any method, and `-k method` should find it; a kind
  per decorator would split one target by how it's called. A `decorators` or `detail`
  field would need a column, a migration and an output field that no filter or
  ranking signal reads. The decorator is on the line above the one rq returns.
- **Go's iota constants aren't tied to their type.** `parent` is lexical nesting, and
  a const isn't nested in its type: Go spells it `pkg.StateIdle`, never
  `State.StateIdle`. Calling them `variant` would turn a convention into an enum.
- **TypeScript overload signatures stay folded into the implementation**, which is
  the one navigation target; emitting each signature would multiply results for
  every overloaded function. A known gap, left open: a `declare function` with no
  implementation (a `.d.ts`) is a `function_signature` too, and isn't indexed.
  (Closed by D38: signatures are indexed as stubs and fold at search time.)
- **No list of wrapper names** (`memo`, `forwardRef`, `observer`, …) for the
  wrapped-function rule. It would catch `memo(BadgeBase)`, but it's a per-framework
  list to maintain. The literal-argument rule also calls `computed(() => …)` a
  function; kind carries no ranking weight between `function` and `constant`, so the
  mislabel costs only `-k`.

*Reverses if:* testers read `trait` for an interface as wrong in practice (then alias
text output only, and record why text and JSON differ); or an agent needs to tell a
property from a method without reading source (then a `detail` output field).

## D38 — Declarations defined elsewhere are indexed, as stubs

**Adopted**, 2026-09-27. TypeScript tester on excalidraw, `@types/node`; recall harness
(D12) against main.

*The weakness.* A `.d.ts` declaration with no implementation in the repo had no symbol:
`declare function`, the members of `declare module "png-chunk-text" { … }` (excalidraw's
`extract`, `decode`), `declare global` additions, a declared `var`. D37 left this open. An
overload signature was dropped too, which was harmless only because the implementation
answered for it.

*The rules.*
- **TypeScript extracts ambient declarations** like the definitions they describe:
  `declare` anything, with or without `export`, and everything in a `.d.ts`/`.d.mts`/
  `.d.cts`. `declare module "fs"` is a module named `fs` holding its members (a wildcard
  or bodiless one declares nothing); `declare global`, and the `global { … }` nested in a
  module that the grammar recovers as an error, add to the top level; a declared
  `let`/`var` is a constant, a global's definition; a namespace merged into a same-named
  function (`export namespace readFile`) adds members to it and isn't a result of its
  own. Ambient declarations read public: they describe API that exists.
- **A stub is a declaration whose body lives elsewhere**, a language-neutral flag
  (`symbols.stub`, schema v21): an ambient value and an overload signature. It takes
  `stub`, −150 scaled by match quality, the size of `local` (D36) for the same reason:
  past what path, extent, kind and visibility add together, so the implementation wins
  wherever it's indexed, and below the exact-to-prefix gap, so a declaration that is the
  only exact match still beats a longer name.
- **A declared interface or type alias is no stub.** A type has nothing elsewhere to be a
  declaration of; declared, it is the definition. The first cut marked them, and
  discourse's `declare global { interface Window }` lost `wnidow` (#1 → #2) and `wndw`
  (#2 → #32) to `windows`.
- **An unqualified name folds within one file.** `collapse_declarations` folded only
  qualified names, since two top-level `Widget`s in two files are unrelated in Rust. In
  one file they are one definition declared more than once, so overload signatures fold
  into their implementation (the stub penalty makes it the survivor), as class-method
  overloads already did through their parent. excalidraw's `useOnAppStateChange` is one
  result at line 140 with `declarations: 4`.

The vendored-library shape is what needs the flag: a CommonJS `index.js` reads private
(ESM's convention), and its `.d.ts` declares the class public, as long, and in a file
named for it, so path, extent and visibility all favoured the declaration.

| | recall #1 / top 10 / found | regress | anchored #1 / top 10 |
|---|---|---|---|
| main | 4,962 / 5,777 / 6,055 | 39 of 44 | 213 / 432 plain, 359 / 439 anchored |
| fold alone | 4,962 / 5,777 / 6,055 (10 up, 0 down) | 37 of 40 | unchanged |
| this change | 4,963 / 5,777 / 6,055 (11 up, 0 down) | 41 of 44 | unchanged |

No source lost #1 or the top 10. The regress gains are `extract` and
`png-chunk-text.decode`; `encode` (the implementation over two ambient `encode`s) and
`useOnAppStateChange` are guards. `@types/node` indexes 6,525 symbols, where it had
interfaces and namespaces only.

*Rejected:*
- **The test penalty (−400) for stubs.** A declaration that is the only exact match would
  lose to a prefix match of another name.
- **Spanning the overload group as one symbol** (first signature to the implementation's
  end), which would put the doc comment above the first signature directly above `line`.
  It lands a jump on a signature rather than the body, and class-method overloads
  already fold; one mechanism for both. The comment stays outside the span, as a leading
  comment does in every language, and `also_in` names the first signature below it.
- **Stubs for all bodyless signatures** (interface and trait methods, abstract methods).
  Those are the contract's own definition, often the one meant.

*Reverses if:* a declaration turns out to be the navigation target beside an indexed
implementation (a library whose `.d.ts` carries the docs people jump to); then the
penalty shrinks toward a tiebreaker.

## D39 — Package names from manifests: weighed, not done

**Rejected**, 2026-09-27. D27's limit: a Rust crate named other than its directory
(`grep_searcher::Searcher`, in ripgrep's `crates/searcher/`) answers `scope_not_found`.
The fix considered: read each file's nearest manifest at index time (`Cargo.toml`
`[package] name`, `go.mod` module, `package.json` name, a Python package) and let
`path_scope` match a segment against it. It needs four manifest formats in the indexer,
a column, and a re-read of every file, for a gain in the testers' corpora confined to
ripgrep's six `grep-*` crates. Go's module path ends in the repo name, which the scope
already reads; npm and Python packages are almost always their directory. The miss is
already graceful: `found_in` names `crates/searcher/src/searcher/mod.rs`, the right
definition.

*Reverses if:* crate-qualified queries into workspaces with abbreviated crate directories
show up in daily use (DOGFOOD.md), or another feature needs the package name anyway.

## D40 — A capital typed into an approximate match counts

**Adopted**, 2026-09-27. Recall harness (6,879 queries, regress cases), the anchored set
and the dogfood set (7,152 queries from the last 300 commits of rq and trekr), against
main at 9b74bed.

*The weakness.* A typed capital is a deliberate signal on an exact match (`case`, +150:
`Symbol` means the type). Nothing read it anywhere else, so a near miss that carried case
ranked the two spellings on kind and extent: `COUNTESR` answered `counters` over
`COUNTERS`, `Plaec` `place` over `Place`, `Geenrated` `generated`. 72 of the dogfood
set's 985 misses were the source's other-case twin, most of them typos.

*The rule.* When the query carries case, a prefix, fuzzy or near-miss match earns `case`
of 150 × *the share of the query's cased letters whose case the aligned letter shares* ×
match quality. The share is graded evidence (`JAVASCRPIT` agrees with `JavaScript` on 2
of 10 letters, with `JAVASCRIPT` on all), and the quality scale is the one D24 gives every
feature an approximate match earns, so it decides between readings of the same letters
without outweighing the letters. A near miss aligns its common subsequence with the name,
as its score does. An all-lowercase query stays casual, as on an exact match: `fraem`
keeps ranking `Frame` over `frame` on kind, the same answer `frame` gets.

| | #1 | top 10 | found |
|---|---|---|---|
| harness, main | 4,963 (72.8%) | 5,777 (84.7%) | 6,055 (88.8%) |
| harness, this | 4,976 (73.0%) | 5,777 | 6,055 |
| dogfood, main | 6,167 (86.2%) | 6,653 (93.0%) | 6,714 (93.9%) |
| dogfood, this | 6,198 (86.7%) | 6,653 | 6,714 |

13 up and 0 down on the harness, 31 and 0 on the dogfood set, all in the `typo` row
(90.4 → 91.7%, 90.1 → 92.9%). Ruby, the anchored set and the regress cases don't move:
their queries are lowercase or exact.

*Considered:* the near-miss tier alone. It scored the same on both sets, which carry
case only in derived typos; the rule is the exact tier's, so it applies to every tier
that approximates a name.

*Reverses if:* people type capitals casually into approximate queries (a sentence-case
habit), which would show up as the other-case twin losing in DOGFOOD.md.

## D41 — A word may be entered past a dropped vowel

**Adopted**, 2026-09-27. Same sets as D40, against D40.

*The weakness.* `align` enters every word after the first at its start. A consonant
skeleton drops vowels, including one that starts a word, so `prsnch` never found
`parse_anchor`: its `n` sits one letter into `anchor`. `hdspdl` (`HEADS_UP_DELAY`),
`mxncls` (`MAX_ENCLOSING`) and `lvrpd` (`LIVE_REPO_ID`) failed the same way. 162 of the
dogfood set's `consonants` misses were not found at all; 123 of them only for this.

*The rule.* A word whose first letter is a vowel (`a e i o u`) may be entered at its
second letter, from anywhere in the word before, as its start may. That is
`MAX_NONBOUNDARY_GAP`'s own reason (a dropped vowel) applied at a word start. A word
starting with a consonant is still entered at its start: dropping a consonant is not how
anyone abbreviates, and allowing it read coincidence into names. Measured as a screen on
the dogfood set, any first letter found 20 more #1s, all in `first+last` queries it read
by accident (`teslimits` stepping through f[l]oat into l[i]mits), and lost 7 #1s to 2.

The name index encodes it: `transition_pairs` adds the pairs from the word before to
the second letter, and `aligns` a shifted word-start mask over the vowels.
`PAIRS_VERSION` 1 → 2 rebuilds every repo's index once, on first need (D25). `make fuzz`
(20,000 names, three seeds) and the exhaustive rails + discourse test (2,372 queries,
77k names) show no disagreement.

| | #1 | top 10 | found |
|---|---|---|---|
| harness, D40 | 4,976 (73.0%) | 5,777 (84.7%) | 6,055 (88.8%) |
| harness, this | 5,052 (74.1%) | 5,915 (86.7%) | 6,213 (91.1%) |
| dogfood, D40 | 6,198 (86.7%) | 6,653 (93.0%) | 6,714 (93.9%) |
| dogfood, this | 6,301 (88.1%) | 6,774 (94.7%) | 6,835 (95.6%) |

`consonants` goes 48.3 → 56.0% #1 and 71.4 → 84.4% top 10 on the harness, 67.9 → 77.5% and
84.1 → 95.2% on the dogfood set. Every language gains: Ruby 56.4 → 57.3% #1, Rust 81.5 →
82.7%. Harness 159 up, 73 down; no source leaves the top 10. Anchored and regress
unchanged, plus a new regress case (`prsnch`).

The 10 lost #1s (2 on the dogfood set, both among them):
- **Another name reads as the skeleton, 6.** `tstkpn` → `test_keep_and_…` (k, p, [a]n),
  `tstmxn` → `test_mixed_encoding`, `tstgrd`, `nwthrd`, `addlcl` → `addUlClasses`,
  `accenc` → `AcceptedAnswerCache`. The competitor holds the query's consonants in order
  now that its vowel-initial words count; the source is #2 or #3.
- **An in-order reading beats a near miss, 4.** `wriet` → `written_at` over `write`,
  `liens` → `line_anchor_start`, `runnign?` → `running_in_rack?`, `Fraem` →
  `FramedImpl`. D14's competition: a name that holds every letter in order outranks a
  transposition whose letters agree less. The source stays #2.

*Reverses if:* skeleton matches through a vowel-initial word start show up as noise in
real use.

## D42 — A query may skip whole words, at a price

**Adopted**, 2026-09-27. Same sets as D40, against D41; latency interleaved on the recall
harness's 58 hand-picked queries.

*The weakness.* `align` never skipped a word, so a query naming a name by its first and
last words found nothing: `braboost` for `BRANCH_DIR_BOOST`, `maxbonus` for
`MAX_BODY_BONUS`, `headelay` for `HEADS_UP_DELAY`. `first+last` was the harness's weakest
row (46.3% #1, 51.2% top 10), and 172 of the dogfood set's 189 `first+last` misses were
not found at all. NAME_INDEX.md had costed the fix at design time.

*The rule.* A word start (or a word's second letter, past a dropped vowel, D41) may follow
any earlier matched letter. Each whole word stepped over costs `WORD_SKIP_PENALTY` (40),
on top of the gap its letters already cost: more than a matched word start earns (25), so
a reading that enters every word outranks one that leaves one out. The name index follows:
`transition_pairs` pairs every earlier character with each word start, and `aligns`
reaches every word start after the earliest position it holds, which is simpler than the
Kogge-Stone fill it replaces. `PAIRS_VERSION` 2 → 3. `make fuzz` (20,000 names, four
seeds) and the exhaustive rails + discourse test show no disagreement.

*Sizing the charge*, each against D41:

| per skipped word | harness #1 | down | lost #1 | lost top 10 | dogfood #1 |
|---|---|---|---|---|---|
| 25 | 5,166 | 49 | 3 | 2 | 6,383 |
| **40** | **5,158** | **25** | **2** | **0** | **6,380** |
| 60 | 5,146 | 7 | 1 | 0 | 6,374 |

With no charge beyond the gap (screened on the dogfood set), `vistno` and `visnode` lost
`visit_statements_node` from the top 10 to `visit_def_node`, which skips the shorter word.
25 lets a skip beat an adjacent reading on side features (`togsec` → `toggle_section`
#5 → #15); 60 gives back 12 #1s to save one. 40 is the smallest charge that loses no
top 10.

| | #1 | top 10 | found |
|---|---|---|---|
| harness, D41 | 5,052 (74.1%) | 5,915 (86.7%) | 6,213 (91.1%) |
| harness, this | 5,158 (75.6%) | 6,132 (89.9%) | 6,563 (96.2%) |
| dogfood, D41 | 6,301 (88.1%) | 6,774 (94.7%) | 6,835 (95.6%) |
| dogfood, this | 6,380 (89.2%) | 6,876 (96.1%) | 6,986 (97.7%) |

`first+last` goes 46.7 → 59.9% #1 and 51.9 → 78.3% top 10. Ruby's found rate goes 89.9 →
99.4%, Rust's 91.7 → 94.6%. Harness 356 up and 25 down, dogfood 151 up and none down.
Anchored (213 / 359 #1, 432 / 439 top 10, plain / anchored) and the regress cases
unchanged, plus a new one (`braboost`). The two lost #1s:
- `tesfilfiewitdiruplwheraidiruplurlisnotdef` (a 41-letter `abbr3`): both it and the
  name with an extra `tag` word saturate the alignment's 600 cap, so the skip's charge
  never lands and side features decide. The cap's limit, not the skip's.
- `admdembers` (a typo of `addMembers`): `AddJoinModeToChannelMemberships` now holds its
  letters in order through a skip. D14's in-order-versus-near-miss competition, as in D41.

*Latency.* More names match a short query, so recall fetches and scores more rows:
`rsp` 858 → 2,213 candidates (query phase 7.2 → 16.7 ms), `hwia` 112 → 663, `apc` 562 →
1,106. 7 interleaved reps × 58 queries, median / p90 of per-query medians, ms:

| | query phase | first answer | wall |
|---|---|---|---|
| D41 | 2.2 / 14.6 | 3.3 / 16.1 | 16.7 / 34.1 |
| this | 2.6 / 17.5 | 3.9 / 18.7 | 16.9 / 34.0 |
| main (before D40) | 2.0 / 11.0 | 3.3 / 12.7 | 16.2 / 33.6 |
| D40–D42 | 2.6 / 17.2 | 3.9 / 18.3 | 16.4 / 33.4 |

Inside the 50 ms first-answer budget, and wall doesn't move (the git check after the
answer dominates it, D23). Accepted for the recall it buys.

*Not revisited: D30.* `fntfam` still ranks `FONT_FAMILY` fifth: its cost is the `_`
counted as a one-character gap on a step that stays contiguous in letters, which neither
the vowel nor the skip rule touches. D30's measurement stands.

*Reverses if:* short queries' candidate counts show up in first-answer latency on a large
repo. The simpler lever is then a bound on how many words one step may skip, which the
pairs can encode; the rejected alternative is a skip allowed only into the last word,
which is `first+last`'s recipe rather than a reading rule.

## D43 — A top-level type outranks its nested namesakes

**Adopted**, 2026-09-28. User report; recall harness, anchored set and dogfood set against
main (0.58.1), plus a screen of every name defined both at the top level and nested.

*The report.* In a Rails app, `rq Account` ranked `class Account < ApplicationRecord` #2,
behind a `Billing::Providers::Account`, at confidence 0.50 each. `depth` charges nothing
up to two levels (`FREE_DEPTH`), so the model and its nested namesakes tie on every
feature but `extent`, which decides by whichever class is longer.

*The rule.* For an unqualified query, a type (`class`, `module`, `struct`, `enum`,
`trait`) with no parent earns `top_level`, 10 × match quality, when a type of the same
name among the results has one. It is relative to the results, applied after scoring
beside `constructor_owner`, because a per-candidate bonus is a bonus for *languages*: Rust
and Go record no module as a parent, so every type there is top-level. The first cut gave
any parentless type 10 and lost 30 #1s, all Rust: `copy` → the `Copy` trait over `fn copy`,
`tick` → `Tick`, `linnum` → `LineNumber` over `line_number`. That is `FREE_DEPTH`'s
`.esm.js` lesson again. Types only: a method's parent is its owner, not a namespace, so a
free function is no more canonical than a method (and `block_on`'s crate-internal free
function would have gained). A named scope or an anchor says which one is meant:
`Admin::Account` never carries it, and `--anchor`'s `enclosing` (60 a level) outweighs it.

*Sizing.* 568 names are defined both ways in rails, discourse, tokio and django. Where
their #1 lands, by bonus:

| bonus | #1 top-level | changed |
|---|---|---|
| 0 (main) | 375 | |
| 5 | 419 | 44 |
| **10** | **434** | **59** |
| 15 | 451 | 76 |
| 25 | 466 | 91 |

At 10 the changes read as intended: `UsersController`, `GroupsController` and
`EmailController` over their `Admin::` twins, the `MutedUser` and `UserFieldOption` models
over the importer's copies, `lib/search.rb`'s `Search` over `Tags::Search`, rails'
`ActiveModel` module over a generator's class, tokio's `FastRand` over a private copy in
`tokio-stream`. Ten are cross-language (a JS model over a namespaced Ruby one, rails'
actioncable `Connection` over `ActiveRecord::TypeCaster::Connection`); neither side is
more canonical, and main's pick was the longer body. 15 adds reopened builtins (core
extensions' `class String`, `Date`, `Numeric`) over `ActiveModel::Type::String` and the
like; 25 adds `Digest`, `File` and `Method`. The rest of both steps are test classes.

| | #1 | top 10 | found |
|---|---|---|---|
| harness, main | 5,158 (75.6%) | 6,132 (89.9%) | 6,563 (96.2%) |
| harness, this | 5,158 | 6,131 | 6,563 |
| dogfood, main and this | 6,380 (89.2%) | 6,876 (96.1%) | 6,986 (97.7%) |

The harness ranks by name, so it can't see an order among namesakes: 2 up, 14 down, no
lost #1. The one lost top 10, `dscrsp` (`DiscoursePluginRegistry` #7 → #12), is the
feature on a fuzzy match: `DiscourseRssPolling`, a namespace module also nested as
`Jobs::DiscourseRssPolling`, gains 4 points in a near tie. The dogfood set changes 14 top
10s and no source. Anchored call sites are unchanged (213 / 359 #1, 432 / 439 top 10,
plain / anchored). Regress 44 of 47 before, plus three new cases that main fails
(`UsersController`, `MutedUser`, `ActiveModel`).

*Rejected:*
- **A bonus for any definition with no parent**, the report's own wording. Free functions
  in every language but Ruby would outrank methods, which `FREE_DEPTH` exists to prevent.
- **Charging per level from zero.** The same language penalty, per level. Measured in D49,
  with a relative form: none beats this rule.

*Reverses if:* a language starts recording modules as parents (then every type in it is
nested, and the rule reads that language's namespaces as it reads Ruby's), or top-level
declarations that only reopen a class (Ruby core extensions) show up winning in real use.

## D44 — A tie reads as 0.50 already: no `tied` flag, no N-way confidence

**Rejected**, 2026-09-28. The same report as D43 asked that an exact tie lower the reported
confidence or say so (a `tied` field), since both halves of a tie showed 0.50.

*Measured.* On the anchored set (446 call sites, truth by location, so a namesake at #1 is
wrong), today's confidence for #1 is already calibrated where a tie lands: asked plain,
the 323 #1s reported at 0.45–0.55 are right 44% of the time. 0.50 is a coin flip, and
says so.

*Rejected:*
- **An N-way share.** Pairwise, a lead `p` over one rival; across rivals,
  `1 / (1 + Σ (1 − p) / p)`, which equals today's value with one close rival and reads a
  four-way tie as 0.25. Brier score of #1's confidence against being right:

  | | today | N-way |
  |---|---|---|
  | plain | 0.250 | 0.232 |
  | anchored | 0.121 | 0.131 |

  Better plain, worse anchored: the anchor's boosts are evidence, and discounting every
  close rival under-reads them (its 0.30–0.45 bucket is right 61% of the time). A change
  to what every caller reads, and `--show`'s gate, needs to win on both.
- **`tied: true`.** A binary on a graded judgement: 1452 against 1452 and 1443 against
  1442 are the same coin flip, and a flag would call one a tie and the other not.
  Confidence already grades it.

What changed is the wording: README and the skill say that about 0.5 means level with
the next result.

*Reverses if:* callers are seen taking a 0.5 result as an answer (then the scale needs a
word in the output, not only in the docs), or an N-way share that holds up anchored.

## D45 — `block_on` stays at #9: no signal in reach picks `Runtime::block_on`

**Left**, 2026-09-28. Revisits D35 with the scores on main (tokio, pinned):

| # | definition | visibility | score |
|---|---|---|---|
| 1 | `future/block_on.rs` free fn | `pub(crate)` | 1255 (path 33, extent 22) |
| 2–4, 7, 10 | `CurrentThread`, `CoreGuard`, `CachedParkThread`, `BlockingRegionGuard`, `MultiThread` | crate/private | 1221–1237 |
| 5–6 | tokio-test's `block_on`, `LocalRuntime::block_on` | public | 1223 |
| 8–9 | `Handle::block_on`, `Runtime::block_on` | public | 1221 |

*Why nothing here fixes it.* The #1 leads by 34, from its file being named for it, so
any visibility signal that clears it is D35's 30/40 setting, which cost Ruby 0.9 points
of #1. The brief's narrower shape, public over `pub(crate)` only among exact ties, has
no tie to act on: #1 isn't tied with anything. And the public definitions alone still
don't order right: `LocalRuntime::block_on` and tokio-test's out-extent `Runtime`'s, and
`LocalRuntime` is re-exported from `tokio::runtime` exactly as `Runtime` is, so even a
crate-root export signal (D35's reverses-if) would tie them. What makes `Runtime` the
answer is that it's the stable, documented entry point, which rq doesn't index.
`Runtime::block_on` is #1 today, and that is the query to type.

*Reverses if:* extraction records re-exports or stability (`cfg(tokio_unstable)`), and the
two together separate `Runtime` from `LocalRuntime`.

## D46 — `declare var process` is held back by `stub`, not by kind

**Left**, 2026-09-28. DOGFOOD.md read the `@types/node` miss (`process` #3) as kind weight:
`interface Process` has `kind` 15 and a declared `var` 0. On 0.58.1 (`@types/node`
26.6.3) the gap is 189: `interface Process` 1403, `declare module "process"` 1214, the
module's `var process` 1188, the global one in `globals.d.ts` 1155. Both values and the
module take `stub` (−150, D38); the interface doesn't, since a declared type is its own
definition. So a kind rule favouring values for a lowercase query would move the var 15
points of 189, and would read intent into case on an all-lowercase query, which the
`case` rule (and D40) decline to do.

The miss is D38's sizing meeting a namesake that isn't its implementation: `stub` is sized
to lose to the implementation of the same thing, and here it loses to a type whose name
differs only in case. The fix, if one is wanted, is for `stub` to apply only when a non-stub
of the same name is among the results, as D43 does for `top_level`. Not taken here: it's
a change to D38's rule and wants its own measurement on TypeScript, where the harness
has only excalidraw's regress cases.

*Reverses if:* ambient globals show up as misses in real use more than once.

## D47 — Words a fuzzy match never enters: measured, not charged

**Rejected**, 2026-09-28. Revisits D30's `fntfam` → `FONT_FAMILY` (#5) with a rule shaped
differently from its rejected one, which scored a step across `_` as contiguous.

*The idea.* `FONT_FAMILY` is the only candidate `fntfam` reads completely: `FontFamilyKeys`
leaves `Keys` untouched and `FontFamilyHeadingIcon` two words. Between matched letters a
skipped word already costs 40 (D42); before the first and after the last it costs only
its letters, through the unmatched-tail charge. So: charge each whole word the alignment
never enters, in `fuzzy_value`. It changes the value only, never what matches, so the name
index (D23) is untouched.

*Measured*, the same binary through an environment switch, against the charge at 0:

| per word | harness #1 | top 10 | lost #1 | lost top 10 | dogfood #1 | `fntfam` |
|---|---|---|---|---|---|---|
| 0 | 5,158 | 6,132 | | | 6,380 | #5 |
| 5 | | | | | | #2 |
| 10, leading and trailing | 5,210 | 6,136 | 11 | 18 | 6,404 (1 lost) | #1 |
| 10, trailing only | 5,198 | 6,128 | 12 | 21 | 6,400 (4 lost) | #1 |

The gain is where the recipe covers every word by construction: `abbr2` 73.8 → 76.5%,
`abbr3` 87.4 → 88.1%. The losses are where a query stops before the name does, which is
also how people type: `consonants` keeps six letters, and its top 10 falls 84.4 → 83.3%.
`ismtch` loses `is_match_at` #2 → #14 to `is_match`, `dblctn` `db_location_from` to
`db_location`, `updget` `update_digest_and_get` #9 → #96. Both effects are the sampler's
shape more than the rule's merit, so the net +52 isn't evidence either way, and 18 lost
top 10s fails D42's bar. 5 per word is too little to move `fntfam`.

*Reverses if:* a query set drawn from real use (not recipes) shows complete readings
losing to longer names; then the charge sized against it.

## D48 — Fields are indexed, as a `field` kind ranked below its namesakes

**Adopted**, 2026-09-28. Recall harness (D12) with the anchored set and regress cases,
against main at 2467823; symbol counts and index time on the ten pinned corpora.

*The weakness.* No plugin emitted a field, so a struct field, an interface property or a
model field had no symbol: `rq also_in` (rq's `Hit.also_in`, the source of a JSON field)
answered `no_match`, as did `AppState.zenModeEnabled`, `Permission.codename` and
`CommonDirs.PublishDir`. Earlier plugin tests asserted the gap on purpose ("a plain data
field isn't a definition worth navigating to"), so this reverses a stance, not an
oversight.

*The rules.* A new language-neutral `core::Kind`, `field`: a named slot a type declares,
parented by the type's qualified name. All syntactic, each in its plugin:
- **Rust:** each named field of a struct or union, with its own `pub`. Not a tuple
  struct's positions, nor a struct variant's fields (the variant is the target).
- **Go:** each field of a named struct type; an embedded field goes by its type's name
  (`*pkg.List[T]` → `List`), as Go names it. A nested anonymous struct's fields stay out,
  reached through the field that holds them.
- **TypeScript/JavaScript:** a class's property declarations, an interface's property
  signatures, and the properties of the object type a type alias *is* (through unions,
  intersections, parentheses). Not an object literal's properties (values, as D17 kept
  them out), a parameter's or type argument's object type, a property's own nested type,
  a computed name. An arrow-valued property stays a method and `static readonly` a
  constant.
- **Python:** any name a class body binds or annotates, unless `UPPER_SNAKE` (a constant)
  or in an enum (a variant). Not a dunder, not `self.x = …`, nothing inside a def.
- **Ruby:** nothing new. `attr_*` and the schema DSLs already emit methods, which is what
  Ruby navigates to. `Struct.new(:major)`'s members stay unindexed (the constant naming it
  is found); a field kind would mislabel what Ruby exposes as accessor methods.

*Ranking.* A field takes a kind weight of −150 × match quality, the size of `local` (D36)
and `stub` (D38): past what path, extent and kind add together, so it ranks below any
same-named definition that isn't a field; above test code (−400); and, as the only literal
match of its name, above another name's prefix (the exact-to-prefix gap is ~300).
`Type.field`, `Type::field` and `Type#field` scope to the parent as for methods.

*The flood.* Symbols per corpus, main → fields (index file size):

| corpus | symbols | added | DB |
|---|---|---|---|
| rails | 50,993 → 50,993 | 0 | 11.6 → 11.6 MB |
| discourse | 74,812 → 79,114 | +4,302 (+6%) | 17.4 → 18.0 MB |
| django | 44,675 → 54,253 | +9,578 (+21%) | 9.2 → 10.4 MB |
| hugo | 11,060 → 15,083 | +4,023 (+36%) | 2.0 → 2.5 MB |
| excalidraw | 4,919 → 7,136 | +2,217 (+45%) | 1.2 → 1.5 MB |
| tokio | 8,319 → 9,701 | +1,382 (+17%) | 1.4 → 1.5 MB |
| ripgrep | 3,463 → 4,150 | +687 (+20%) | 0.6 → 0.6 MB |
| gin | 1,674 → 1,990 | +316 (+19%) | 0.4 → 0.4 MB |
| rq, trekr (pinned) | 4,429 → 5,220 | +791 (+18%) | 0.9 → 1.0 MB |

The most repeated names: discourse's Ember injections (`@service router;` 219 times,
`currentUser` 124, 2,770 `@service` lines in all), django's `name` (599), `ordering` (174,
mostly `Meta`), `operations` and `dependencies` (migrations), tokio's `inner` (110).

*Burial, measured directly.* For every name outside test paths that is both a field and
some other definition (208 in django, 531 hugo, 559 discourse, 193 tokio, 1,907 over nine
corpora), `rq <name>` ranked a field first on 17. Each is a field over a namesake that
takes a penalty of its own: a test-scoped or test-path definition (ripgrep's `prev` in
`mod tests`, discourse's `hide_profile` in a page object), or another field.

*Recall.* Against main: #1 5,158 → 5,160, top 10 6,132 → 6,149, found 6,563 → 6,583; 22
sources up, 14 down, Ruby unchanged. The gains are not evidence for fields: ground truth is
items only, and rank is by name, so a field that shares a source's name (tokio's `uring`,
rq's `lang`) now counts as finding it. The losses are the real signal:
- `binimplicit` (ripgrep): `binary_implicit`, a field, over `binary_detection_implicit`
  #1 → #2. It reads as the query better; ambiguous.
- `src` (rq): `Source` #2 → out of the top 10, behind the fields literally named `src`.
- Anchored: plain top 10 432 → 431; `modal` (discourse) #10 → #62 plain, behind 52
  `@service modal;` injections, since the Ruby truth sits in a spec path (−400). Anchored
  it is unchanged, as is every anchored number (213 / 359 #1, 439 top 10).
- Regress: 44 → 51 of 54, the seven new field cases (`also_in`, `Hit.also_in`,
  `CommonDirs.PublishDir`, `RootConfig.BaseURL`, `AppState.zenModeEnabled`,
  `Permission.codename`, `AbstractUser.email`); none lost.

*Cost.* Index time unchanged within noise (median of 5 interleaved, load ~50: django 1.56
→ 1.49 s, discourse 2.43 → 2.54 s, hugo 0.51 → 0.49 s). Query latency on the 58 hand-picked
queries, 5 reps: first answer 4.7 / 24.3 → 4.7 / 25.6 ms (median / p90).

*Rejected:*
- **Not indexing fields.** The measurement above is the case against the flood: a field
  takes #1 from a real namesake almost never, and the one plain top 10 lost is to test
  code. Without them, `rg` is the only way to a JSON field's or a model column's source.
- **A −400 field weight** (the test-path size, so an exact field loses to another name's
  prefix). Against −150: recall 1 fewer #1 and 6 fewer top 10 (all name-coincidence
  sources, as above), and on real queries it answers the wrong thing: django's
  `verbose_name` gives `verbose_name_raw`, a method the query only begins, where the field
  is what was typed. rq's `recency` is the same shape; the aspiration test now asks for
  the field there and keeps the helper under `recency_b`.
- **Dropping bare decorated class fields** (`@service modal;`), which would cut most of
  discourse's 4,302. It needs a rule that also drops `@tracked count;` state, or a list of
  decorator names, a per-framework list as D37 turned down.
- **Instance attributes (`self.x = …`, `this.x = …`).** An assignment in a method body is
  a use as often as a declaration, and each one would be another symbol per method.

*Reverses if:* field noise shows up in daily use where a namesake exists (DOGFOOD.md; then
the weight moves toward −400, with `verbose_name` as the price), or Ember-style injections
dominate a JS codebase's results (then the bare-decorated rule).

## D49 — Nesting charged per level: measured, and D43 stays

**Rejected**, 2026-09-28. Asked after D43: rather than a bonus for one case, why not a small
charge for every level of nesting beyond the one queried? D43 had measured only "+10 for
any parentless type" and rejected per-level charging by argument. These are the numbers.

*Method.* One index per screen, built once; each variant a throwaway build reading the
constants from the environment, so every row ranks the same rows. Against main (0.59.0,
D43 on): the recall harness (sourced queries, Ruby and Rust) with regress and the anchored
set, the dogfood set (rq and trekr, Rust), and D43's 568 names defined both top-level and
nested in rails, discourse, tokio and django.

*Variants.* D43 off in all of them, since each is meant to subsume it.
- **A, absolute:** `depth` charged from level 0 or 1 (`FREE_DEPTH`) at 5, 10 or 15 a level.
  The literal reading: an unqualified query asks for depth 0.
- **B, relative:** each result charged per level deeper than the shallowest same-named
  result. A qualified query's results all sit under its scope, so the shallowest is the
  queried depth.
- **C:** B for types only (`Ct`); B replacing `depth` (`Cr`); `Ct` capped at one or two
  steps.

| variant | #1 | Ruby / Rust #1 | top 10 | lost / won #1 | regress | anchored #1 plain / anchored | dogfood #1 (lost) | names top-level (changed) | discourse JS #1 |
|---|---|---|---|---|---|---|---|---|---|
| **main (D43)** | **5,160** | **1,360 / 3,800** | **6,148** | | **54 / 57** | **213 / 359** | **6,380** | **434** | **274** |
| D43 off | 5,160 | 1,360 / 3,800 | 6,149 | 0 / 0 | 51 | 213 / 359 | 6,380 (0) | 375 (59) | 275 |
| A, from 0, 5 | 5,149 | 1,356 / 3,793 | 6,149 | 30 / 19 | 53 | 216 / 359 | 6,388 (8) | 417 (20) | 280 |
| A, from 0, 10 | 5,147 | 1,351 / 3,796 | 6,143 | 34 / 21 | 54 | 214 / 359 | 6,387 (13) | 443 (11) | 287 |
| A, from 0, 15 | 5,128 | 1,339 / 3,789 | 6,135 | 57 / 25 | 54 | 217 / 360 | 6,390 (18) | 459 (26) | 301 |
| A, from 1, 5 | 5,159 | 1,359 / 3,800 | 6,149 | 11 / 10 | 51 | 216 / 359 | 6,380 (0) | 374 (63) | 275 |
| A, from 1, 10 | 5,155 | 1,355 / 3,800 | 6,144 | 9 / 4 | 53 | 214 / 359 | 6,380 (0) | 392 (45) | 280 |
| A, from 1, 15 | 5,151 | 1,351 / 3,800 | 6,141 | 14 / 5 | 53 | 217 / 360 | 6,380 (0) | 399 (49) | 282 |
| B, 5 | 5,158 | 1,359 / 3,799 | 6,148 | 6 / 4 | 54 | 217 / 358 | 6,377 (3) | 426 (11) | 277 |
| B, 10 | 5,149 | 1,356 / 3,793 | 6,147 | 15 / 4 | 54 | 217 / 359 | 6,377 (3) | 443 (10) | 278 |
| B, 15 | 5,148 | 1,355 / 3,793 | 6,146 | 17 / 5 | 54 | 218 / 360 | 6,377 (3) | 460 (28) | 278 |
| Cr (B, no `depth`), 5 | 5,157 | 1,358 / 3,799 | 6,156 | 16 / 13 | 53 | 216 / 359 | 6,377 (3) | 417 (20) | 270 |
| Cr, 10 | 5,149 | 1,356 / 3,793 | 6,156 | 24 / 13 | 54 | 215 / 359 | 6,377 (3) | 443 (11) | 271 |
| Cr, 15 | 5,148 | 1,355 / 3,793 | 6,154 | 26 / 14 | 54 | 217 / 360 | 6,377 (3) | 459 (26) | 271 |
| Ct (B, types), 5 | 5,161 | 1,360 / 3,801 | 6,149 | 1 / 2 | 54 | 213 / 359 | 6,380 (0) | 426 (11) | 276 |
| Ct, 10 | 5,161 | 1,360 / 3,801 | 6,149 | 1 / 2 | 54 | 213 / 359 | 6,380 (0) | 443 (10) | 277 |
| Ct, 15 | 5,161 | 1,360 / 3,801 | 6,149 | 1 / 2 | 54 | 213 / 359 | 6,380 (0) | 460 (27) | 277 |
| Ct, 5 capped at 10 | 5,161 | 1,360 / 3,801 | 6,149 | 1 / 2 | 54 | 213 / 359 | 6,380 (0) | 425 (10) | 276 |
| Ct, 10 capped at 10 | 5,161 | 1,360 / 3,801 | 6,149 | 1 / 2 | 54 | 213 / 359 | 6,380 (0) | 434 (0) | 277 |

Found is unchanged everywhere (6,583 harness, 7,008 dogfood); lost / won is against main.
More top-level namesakes is not better past D43's 434: that is where reopened builtins
start to win (D43's sizing).

*A, from level 0,* is `FREE_DEPTH`'s language penalty back again. Every method has an
owner, so it loses to a free function or a type in a language that leaves those at the
top: `tick` → the `Tick` struct, `linnum` → `LineNumber`, `member` → `Member`, trekr's
`place` → `Place`, and in discourse a Ruby method → a JS helper (`to_array` → `toArray`,
`flrtyp` → `FilterTypeValueSuggester`). JS takes discourse's #1 on 6 to 27 more queries.
Ruby loses #1 at every setting. The dogfood set gains (+7 to +10) because rq's and trekr's
own free functions win there, but it also loses 8 to 18.
*A, from level 1,* leaves Rust alone (a method sits at level 1) but can't see D43's case:
`Admin::UsersController` is at level 1 too. It fails one to three regress cases D43 passes,
puts 374 to 399 namesakes at the top level against D43's 434, and still loses Ruby #1s.

*B* is the same penalty in relative form. Where a free function and methods share a name,
every method is one level deeper: tokio's `schedule` loses to the `Schedule` trait, and
ripgrep's `shortest_match` methods to `shortest_match_at`, charged for a test's free
`shortest_match`. D43's "a function's parent is its owner, not a namespace", measured.
Dropping `depth` for it (Cr) is worse.

*Ct,* types only, is clean on every screen but the namesakes. Its one lost #1 is a typo's
near tie: `sleect` → the `select` method over `ActionView::Helpers::Tags::Select`, charged
for being deeper than a test's `ReservedWordTest::Select`. Per level, it is D43 at another
size: at 5 it misses the `Admin::` controllers D43 fixed; at 10 or 15 it charges
`ActiveModel::Type::String` two levels and ranks the core extensions' `class String`,
`Date`, `Numeric` and `Method` above the real classes, the step D43 sized to stop short of.
The second level buys two right answers D43 misses, where the nested namesake's longer
body outweighs 10 (`AboutController` over `Admin::Config::AboutController`, the
`AiToolAction` model over `DiscourseAi::Automation`'s), against four builtins and three
test classes flipped the other way. Capped at one step, Ct is D43 restated as a penalty:
the same 434 namesakes, +1 net #1 on the harness (one lost, two won, fuzzy near ties).

*So:* no per-level charge beats D43. The one that matches it is a flat step, the same size,
charged to every type deeper than its shallowest namesake instead of credited to the top
level. That is a rewording with one lost source, not a simpler rule, so D43 stays.

*Reverses if:* a language records its modules as parents (D43's own reverses-if). Then
nothing is top-level there, and the capped relative form (a type one or more levels deeper
than its shallowest namesake: −10 × match quality) is the drop-in, measured here as equal.

## D50 — The checkout is the index unit; a file version is stored once

**Adopted**, 2026-09-28. The reproduction is `tests/checkouts.rs`; numbers at the end.

*The bug.* Identity is the remote (`github.com/org/repo`), so every worktree, clone and
detached checkout of one project shared one set of rows, and `files` was keyed by
`(repository_id, path)`: the last checkout to index a path won. Two worktrees where
branch A adds `Widget#alpha` and branch B renames `old_name`: index A, then B, and a
search for `alpha` from A found nothing. It said `warming` (exit 2) because A's HEAD
differed from the one recorded, which was B's, and a background warm then re-indexed A
over B; with the warm in-process it was a definitive miss (exit 1). B's next hit re-parsed
its own file back. Answers flipped with every switch, each paid for by re-indexing.
Coverage, the indexed HEAD, the edited set, the commit-times HEAD, the warm lock and
verdict and the branch-file cache were per repo too, so one checkout's state decided the
other's warm.

*What.* Two ideas, each a key change rather than a special case:
- **The checkout is the index unit.** A `checkouts` row (a root path) owns coverage and
  every per-tree cache. A search is scoped to it, and its rows carry their `root`. The
  repository stays the logical project: JSON `repo`, the grouping `-a` spans, and the
  name index's unit.
- **A file version is stored once per repo.** A `files` row is now `(repository_id, path,
  content_hash)`, and symbols hang off it as before. `checkout_files` maps each checkout's
  paths to versions, with that checkout's `mtime` and `git_ts` (commit times follow a
  branch's history, not the bytes). A second worktree reads and hashes its files, finds the
  versions, and parses only what differs; a pass loads the repo's `(path, hash)` set up
  front so a worker skips a known version's parse, not just its write.

*Why path and bytes, not bytes alone (trekr's blob).* trekr's facts are "a pure function of
a blob's bytes". rq's extraction isn't: the plugin is chosen by extension, `.d.ts`
declarations are stubs, and the TS grammar differs from TSX's. The extractor's whole input
is `(path, bytes)`, so that is the key; the only sharing it gives up is identical content
at two paths, which a rename across branches produces and which costs one parse.

*No `--gc`: a version nobody maps is deleted when the last map row lets go of it*, in the
same transaction (a path rewritten, a file forgotten, a checkout pruned or dropped). An
orphan can't exist, so nothing needs collecting. Rejected: keeping versions until a `--gc`
(trekr), so switching a checkout back to a branch parses nothing. Every saved edit mints a
version, so the store grows with editing rather than with code, and it needs a collector
with a retention policy. Switching back re-parses only the files the branches differ in,
as it did before; a sibling worktree on that branch keeps them alive anyway.

*The name index stays per repository*, over every version's names and paths. It is a
screen: a name only another checkout defines passes it and fetches no rows, because the
fetch joins the checkout's map. Candidates are still exactly what `score` accepts from this
checkout's rows (D23's property). It was already a superset over time, holding names
deleted since its last rebuild. Rejected: an index per checkout (a rebuild and a copy of
the name bytes per worktree, for keys nearly all shared) and a shared base with a
per-checkout delta (D23's declined layering). A cold pass suspends the index only when the
*repo* holds no versions: a new worktree's first pass appends the few names it adds.

*`-a`* searches every checkout. Rows of one version fold to one hit, the current
checkout's when it maps it, else the newest checkout's; so do rows of different versions
that define the same name, kind and parent at the same path, whatever line each is on,
which is what two branches that differ elsewhere in the file look like. The first cut
also required the same line, and an edit above a definition defeated it: with five
rails worktrees `rq -a find_by` listed `core.rb`'s `find_by` three times (lines 255,
259, 260), 40% of `-a` top 10s changed, and other repos' answers were pushed down —
search, not navigation. Rejected: folding only when the lines are near, a threshold
with nothing to derive it from; the path, name, kind and parent already say which
definition it is, and the one kept is the checkout you're in or the newest. The first fold happens in SQL, one
row per version, before the candidate cap: folding after it let k checkouts of one repo
cut the cap to 8,000/k definitions, which dropped 8,771 of them across five rails
worktrees and crowded out every other repo. Checkouts far enough apart still hold one
definition in several versions, so the name index's fetches count definitions against
the cap when unscoped; a fuzzy `teco` over five rails worktrees 300–2,000 commits apart
recalled 3,201 definitions against 8,000 versioned rows, and now 4,812, the union of
what each worktree finds alone. Scoped, a row is a definition and the count is rows,
as before. The current-repo boost follows
the scope: it goes to the checkout you're in (the feature keeps its name, `current_repo`).

*Surface.* `--status` is one row per checkout, with `root` beside `repo`. Hits and
outlines already named the checkout by `root`, so the field keeps that name rather than
adding a `checkout` synonym. `--drop` inside a checkout drops that checkout; given a
repo identity, every checkout of it.

*Migration (v23).* `files` is rebuilt as versions, keeping ids so symbols stay put. Each
repo's rows are mapped to one checkout still on disk, the one a search last verified
(the newest `warm_verified:` stamp) or else the newest registered, which keeps its
coverage; other checkouts start unindexed and warm on their next search, parsing only
what differs from the versions already there. The first cut took the newest registered
checkout whether or not it existed: usually a short-lived agent worktree, often gone, so
`-a` answered from a deleted tree and the live ones started empty. Per-tree caches keyed
by repo are dropped rather than guessed at: the next sweep records them again. A repo
with no checkout is unreachable and goes, name index too.

The upgrade holds the write lock throughout, 11.4 s on a store of 1.5M symbols, and
seven of eight rq processes opening it meanwhile failed "database is locked" (exit 74)
at the 3 s busy timeout. An opener that finds the schema behind now waits up to five
minutes for the lock, and the one that upgrades says so on stderr.

*Dead checkouts.* A checkout whose root is gone from disk is forgotten before `-a`,
`--status` or an index pass reads the others: a stat per checkout, a write only when one
is gone. Only pruning on a sibling's index pass left `-a` answering from deleted trees
(and preferring them, being newest) and `--status` calling them `complete`. When a
repo's last checkout goes, the repo goes with it.

*Measured.* Release builds against main (0.59.0), on a shared machine whose load is
given per table. Two rails worktrees 300 commits apart (486 files differ) and a third
on the same commit as the first.

`make recall BASE=main --anchored`: 0 sources moved, the top 10 changed in 0 of 6,879
queries, regress 54 of 57 on both, anchored unchanged (213 / 359 #1, 431 / 439 top 10,
D49's main row). A single checkout ranks exactly as before.

| rails store, vacuumed | MB |
|---|---|
| main, one checkout | 11.2 |
| this, one checkout | 11.7 (+4.5%: the map, with its path) |
| this, two worktrees | 14.3 |
| two full copies | 23.4 |

| `rq --index` of a worktree, 5 reps, load 8, ms | default jobs | one job |
|---|---|---|
| cold, main | 848 | 2,927 |
| cold, this | 847 | 3,472 |
| after a sibling on the same commit | 170 | 214 |
| after a sibling 300 commits away (12,609 new symbols) | 321 | 841 |

An earlier run at load 5 agreed: cold 619 / 620, same commit 152, 300 commits away 259
(one job: cold 1,967 / 1,963, 178, 659). What a sibling pays for is reading and hashing
every file, parsing what differs, and its own `git log` for commit times (56 ms of the
same-commit pass, since commit times follow the checkout's branch).

| 582 rails queries × 5 reps, interleaved, load 8–13, ms (median / p90 / p99) | recall | query phase | first answer |
|---|---|---|---|
| main, worktree A alone | 1.45 / 6.01 / 14.88 | 1.71 / 8.72 / 25.52 | 32.42 / 42.13 / 60.38 |
| this, A alone | 1.53 / 6.31 / 18.29 | 1.79 / 9.80 / 28.92 | 3.22 / 11.66 / 31.36 |
| this, A beside B | 1.60 / 6.69 / 18.48 | 1.84 / 10.09 / 28.05 | 3.23 / 12.24 / 30.55 |
| this, B beside A | 1.54 / 6.71 / 18.82 | 1.81 / 10.43 / 31.60 | 3.48 / 12.63 / 33.98 |

The price is one seek into `checkout_files` per candidate row: recall +5% at the median
and p90, +23% at p99, where queries fetch thousands of rows. Making its `file_id` index
cover `mtime` and `git_ts` saved a second seek (recall p90 5.37 → 5.26 in an earlier
run); dropping the `checkouts` join for the root measured within noise and stayed. A
sibling's versions cost the scan little: the screen passes its names, the fetch finds no
rows. Plain clones (`make recall --bench`, 58 hand queries × 5, load 12–15): query phase
2.8 / 18.7 → 3.1 / 22.1 ms, first answer 4.1 / 20.1 → 4.2 / 23.1.

First answer in a worktree fell tenfold for a reason next door: every fast path in the
git layer assumed `.git` is a directory, so a linked worktree forked git for HEAD and,
with no stamp for its branch-file cache, ran `git diff` on every query. They now follow
the `.git` file to the worktree's own dir and its `commondir`. Main paid the same, which
is why its first answer here is 32 ms against the 2–4 ms a clone gets.

*`-a` over five worktrees*, after the fixes above (rails clone at `main`, and
worktrees 300, 600, 1,000 and 2,000 commits back, all indexed; 1,163 rails queries,
`--limit 0`, from the 300 worktree). Against the union of what each worktree finds
alone, counting a declaration folded into `also_in` as found: D50's first cut (main)
missed 22,134 of 243,452 definition sites in 26 queries and returned 169,968 duplicate
rows in 1,006; now none missing and no duplicates. Latency, `--profile` phases, 3 reps
interleaved, load 7–23, ms (median / p90 / p99):

| | recall | first answer |
|---|---|---|
| main, one checkout | 1.53 / 6.17 / 18.27 | 3.33 / 11.79 / 31.70 |
| this, one checkout | 1.54 / 6.19 / 17.68 | 3.13 / 11.55 / 28.14 |
| main, `-a`, one checkout | 2.28 / 7.45 / 19.36 | 3.83 / 12.58 / 29.98 |
| this, `-a`, one checkout | 2.33 / 7.66 / 22.28 | 3.86 / 12.72 / 31.19 |
| main, `-a`, five | 2.72 / 15.12 / 54.01 | 4.84 / 22.88 / 70.09 |
| this, `-a`, five | 2.72 / 13.07 / 39.77 | 4.38 / 18.12 / 47.83 |

Five checkouts still cost `-a` about twice one at p90 and p99: the name index holds
every version's names, and each surviving row pays the per-version checkout pick.
Both of D50's levers were already in (the covering `checkout_files` index; the
`checkouts` join measured within noise), so nothing further was tried.

*Correct from each worktree.* All 1,163 rails harness queries, top 10 from each worktree
with its sibling indexed against the same worktree indexed alone: identical, 1,163 of
1,163 from both, and no row from the other checkout. `tests/checkouts.rs` pins the
reproduction (worktrees, clones of one remote, a detached checkout) and the sharing,
`-a`, `--status` and `--drop` behaviors. rq's own main and `checkouts` worktrees on one
database: main's first index stored 950 of its 1,511 symbols, sharing the rest.

*Migration.* v23 on a rails database built by 0.59.0: 20–27 ms including the `--status`
that opened it; foreign-key and integrity checks clean. Tables are rebuilt SQLite's way
with foreign keys off for the upgrade transaction; enforcement is back on for every write
after it. An rq older than 0.54.1 lowered the version of a newer database it opened, and
re-running pre-v23 steps on the new shape would fail, so a database that already has
`checkout_files` resumes the ladder at v23.

*Rejected: identity by path, no sharing* (each checkout its own repo row, files and name
index). No join, so search would match main exactly, and no versions table or release.
But a second worktree would cost a cold pass (847 ms at rails against 170, and minutes on
a monorepo where a first query blocks on it) and a full copy (+11.7 MB against +2.6),
paid by every worktree an agent makes for a task. The sharing is about 80 lines (the
versions table, `release`, the parse skip); the rest — checkout-scoped coverage, caches,
scope and output — either shape needs.

*Reverses if:* the per-row seek shows up against the 50 ms budget at scale, where
denormalizing the checkout onto `symbols` (a row per checkout per symbol) trades the
sharing back for a seekable `(checkout, name_lower)` index; or extraction stops reading
the path, when bytes alone can key a version and renamed files share too.

## D51 — A database rq can't use is set aside and rebuilt, or kept for a newer rq

**Adopted**, 2026-09-29. `src/store/recover.rs`; tests there and in `tests/recovery.rs`.

*The problem.* The index is a cache, but a database rq couldn't use left every command
failing until someone deleted it by hand: a file that isn't a database, a truncated one, or
an upgrade step that errors all came back as exit 74 on every run. Meanwhile an older rq
that opened a newer rq's database queried it as its own and failed on SQL
(`no such column: fi.mtime`, 0.59 against v23).

*What.* `Store::open` sorts every way an open can fail into one of three:
- **Broken:** SQLite says the file is corrupt or not a database (at any statement of the
  open, including the schema read that ends it), an upgrade step fails for a reason other
  than the environment, the version is negative, or a table the schema creates is missing.
  The file and its WAL move to `<name>.broken-<unix time>`, the shared-memory file is
  deleted, and a fresh database is created. The previous set-aside copy is deleted first, so
  at most one is kept. One line on stderr says what happened and where the old file is;
  stdout and the exit code are the command's own, so `--json` sees `not_indexed` or
  `warming` like any first run, and the usual rebuild-on-need takes it from there.
- **Newer:** the version is above this rq's. The file is never written. This rq opens
  `<stem>.v<its version>.<ext>` beside it (`rq.v23.db`) instead, says so once when it
  creates it, and works normally from it.
- **Failed:** busy, locked, disk full, I/O, permissions, read-only, out of memory. A new
  file would meet the same failure, so the error is reported as before (74).

The upgrade runs in one transaction, as it did, so a step that fails midway rolls back and
the copy set aside is the database as the older rq left it: evidence for the bug report.

*Stamps.* `user_version` stays the schema version. `meta` gains `created_by` (the rq that
laid down a fresh schema) and `schema_by` (the rq that brought it to its current schema),
written inside the schema transaction, so they cost no extra write. `schema_by` is what the
side-store message quotes ("written by a newer rq (0.61.0)"). Rejected: a last-opener
stamp. It would take the write lock on the read path whenever the binary changed, and
nothing needs it: the schema version decides compatibility, and the rq that owns that
schema is the one worth naming.

*Concurrency.* An advisory lock on `<db>.lock` is held shared while opening and exclusive
while setting a file aside. An opener that finds the file broken takes the exclusive lock,
then compares the file's (device, inode) with what it saw before opening: if it changed,
another process already set it aside, and this one opens the new file. So exactly one
process moves it and says so, and none can move a fresh database another has started
writing to (the eight-thread and six-process tests check both). The shared lock keeps an
opener off the old file while its WAL and main file move; the WAL moves first so a new file
never adopts the old WAL. A process that already had the old file open finds out on its
next write, which SQLite refuses as `SQLITE_READONLY_DBMOVED`.

*Side stores, not a refusal or a rebuild.* Two installed versions (a brew release and a dev
build, or an agent's pinned copy) are normal. An older rq that rebuilt a newer database
would ping-pong, each version wiping the other's index on every alternation; one that
refused would fail every command until upgraded. The side store costs a second index on
disk and a cold first search, and both versions keep working. When a newer rq lays down
its schema at the main path, it deletes its own version's side store (the main path is
its own again) and any older version's unused for 30 days; side stores are found by
probing their names, not by listing the directory, because `RQ_DB` can sit in a directory
of hundreds of thousands of files (listing a 445,000-entry `$TMPDIR` on each schema write
took the burst-of-openers test from 1 s to 50 s).
Rejected: a newer rq adopting an older rq's side store. It is a cache of the same repos at
an older schema, and adopting it would mean migrating a file some older rq may still be
writing.

*Not done.* No `PRAGMA quick_check`: it reads every page, 20–30 ms on a 12 MB rails index
against a 50 ms first-answer budget, and WAL gives no "unclean shutdown" signal to reserve
it for. It isn't needed for truncation either: SQLite compares the header's page count
with the file's size on the first read, which the open's schema read triggers (the
truncation tests cut a database in half). Damage deeper in a file than the open reads
still fails the command that reaches it. The open costs 0.5–0.6 ms against 0.5 ms before
(rails, `--profile` "store open").

*Reach.* Only rq from this release on recovers or keeps a side store. rq 0.60.0 and older
opening a later schema still fail on SQL; the first schema bump after this one is the first
an older rq steps around.

*Reverses if:* a real index turns out to be worth more than a rebuild (it's a cache today),
or side stores pile up in practice; then the older rq refusing with a clear message is the
simpler shape.

## D52 — An answer from a half-built index says so, and a guess waits

**Adopted**, 2026-09-30. `settled`, `disclose_warming` and the poll in `src/cli/mod.rs`;
`begin_pass`/`passes` in `src/store/mod.rs`; e2e tests `a_prefix_match_from_a_partial_index_*`,
`an_exact_match_from_a_partial_index_*`, `a_search_waits_on_another_process_*`.

*The problem.* A user on a ~93k-file Ruby monorepo, after an upgrade rebuild or `--drop`:
`rq User` answered `admin_approval_tool.rb` and `rq Order` a model named for something
else, both `source: index` at normal confidence, while `class User` wasn't indexed yet.
Reproduced on a 100k-file Ruby corpus with a fresh index: `User` →
`UserFieldsController` (prefix, 0.9), `Order` → a method `ordered_variable_defaults`
(prefix, 0.9), `Account` → a method `account` (0.84). Two causes:
- The warming poll accepted any **prefix** match as the answer. The demand tier (D11)
  parses files containing the name in path order, so on a large repo hundreds of
  `user_*` names commit before `user.rb` does.
- The poll stopped when **this process's** warm ended, while another process (the
  detached warm, an upgrade rebuild, an `rq --index` elsewhere) was still filling the
  index. And no hit said the index was partial.

*What.*
- **Settled.** On a checkout not yet `complete`, the top match answers only when no unread
  file can beat it on its name: an exact match carrying `case` (the capitals typed agree),
  or any exact, prefix or constructor match once this search's warm has
  read every file containing the name — the demand tier's walk completing, which the warm
  now reports. Anything else keeps the poll waiting.
- **The wait follows whoever is indexing.** A pass over an incomplete checkout marks it in
  `meta` (`pass:<root>:<pid>`, cleared at its end, a dead pid's mark cleared by the next
  pass) and records how many source files the tree spans (`span:<root>`, from the
  `git ls-files` the pass enumerates with). The poll waits while its own warm runs, or
  another live pid holds a mark or the warm lock (which a `--warm` child holds across
  its passes), within the wait budget as before.
- **Not settled when the wait ends** (`--no-wait`, `--wait`, the budget, Ctrl-C, or
  nobody left indexing): `{"status": "warming", "query", "warming", "provisional": [hits]}`,
  exit 2. Text prints the hits and one stderr line. `--show`/`--open`/`--web` don't act
  on it.
- **Every index hit from an incomplete checkout** carries `warming: {read, of,
  interrupted, hint}`: files held, files the tree spans (omitted before any pass counted
  them), whether nothing is indexing it (no live mark, and this rq leaves no warm
  behind), and what to run. `confidence` is scaled by `read / of`, floored to two
  places. Text adds one stderr line. No schema change: `meta` holds the marks.

*Shape.* trekr answered the same report first (its DEC-320) with `warming: {read, of,
interrupted, hint}` on every answer, confidence scaled by the share read, and exit 2 for
"no answer yet". rq already says `warming` for that state and exits 2 for it, so the
same object fits without new vocabulary. Exit 2 stays one shape — the status object a
miss already gets — with `provisional` added, rather than a result array that exits 2:
a caller that branches on the exit code reads the same object it always did.

*Measured.* The 100k corpus (97k Ruby files; full index 19.6 s, release, 8 cores, load
3–12 from other work), a fresh store per sample, a rebuild driven by another process
(`rq --warm`, as an upgrade or `--drop` leaves), the query issued 1 s (~6k files read)
or 4 s (~25k) in, `--json -l 2` with default settings. 26 capitalized class names; a hit
is correct when it is one of the complete index's top-scoring results (ties included).

| | base, 1 s | new, 1 s | base, 4 s | new, 4 s |
|---|---|---|---|---|
| correct top hit | 10/26 | 15/26 | 17/26 | 23/26 |
| wrong, undisclosed | 16 (conf up to 1.0) | 0 | 9 (up to 1.0) | 0 |
| wrong, disclosed `warming` | — | 11 (conf ≤ 0.11) | — | 3 (≤ 0.18) |
| top is a definition of the name | 19/26 | 25/26 | 22/26 | 26/26 |
| exit 2 | 0 | 0 | 0 | 0 |
| latency p50 / p90 | 286 / 524 ms | 309 / 1121 ms | 1412 / 1836 | 1427 / 2497 |

Every remaining wrong answer is a class of the right name in another place — another
`Logger`, another `Request` — the case the disclosure is for. A complete index answers
byte-for-byte as before (26 names plus 7 more, `-l 5`), warm latency within noise
(median 133 vs 134 ms over three rounds), and `make recall BASE=main` is unchanged:
0 up, 0 down, top 10 changed in 0 of 6,879 queries.

*The cost: queries without a capital.* `user`, `account`, `order`, `topic`,
`connection_pool`, `perform` and the prefix query `ConnectionPo`, 1 s into the same
rebuild: base 3 of 7 right in ~0.3 s; new 7 of 7 right in 4–25 s, since only the
demand walk's end settles them (it reads all 97k files and parses every one holding the
name while the rebuild competes for the CPU, and the cold pass's suspended name index
makes each poll's recall ~1 s). It is paid once per cold rebuild, within the wait
budget, under the block-until-answered rule that correctness beats a first query's
latency.

*Rejected.*
- **Any exact match waits for the demand walk too** (every answer settled the same way).
  Measured as a third arm: 19/26 and 17/26 correct against 15/26 and 23/26, at p50 10.4–
  10.7 s and p90 27–40 s. Reading every file holding the name settles the *name*, not
  the ranking among a hundred exact `Client`s, which still moves until the whole tree is
  in.
- **Any exact match answers early, capital or not.** `Order` answered the `ORDER`
  constant in the 1 s run, and `account` a method `account` where the class ranks
  first; `case` is what says the capital was meant.
- **The live scan for the exact name instead of waiting.** The demand walk already is
  that scan, persisted; a second, unpersisted one would race it for the same files.
- **Looping passes in the search's own warm** so a fuzzy query on a repo above the
  per-pass cap (50k files) waits for the whole index. A capped pass stopping is what
  `warm_progress` pins; such a query now reports `warming` with its provisional hits
  rather than answering, and the detached warm carries on.

*Reverses if:* lowercase or prefix queries during a rebuild draw complaints about the
wait (then accepting any exact match early, which would have got 4 of the 6 lowercase
queries above right, is the cheaper trade), or the poll's recall on a suspended name index is fixed and the
demand walk gets cheap enough that waiting for it costs nothing.

### D52 addendum — what a pre-release hunt changed

- **`--wait` is a deadline at a terminal too.** Following other processes' passes made an
  interactive wait end only when the last of them did: `--wait 3` under a pty waited 20 s
  behind an `rq --warm` rebuild of the 100k corpus. An explicit `--wait` now bounds every
  caller; without one, a terminal still waits until answered (the progress line shows,
  Ctrl-C escapes). A poll that, at the last poll's cost, would end past the deadline is not
  started — on a cold pass each costs about a second.
  The rest of the overshoot was after the answer: the search's own pass, cut short, rebuilt
  the suspended name index and read commit times (~1.2 s) that the still-running pass
  would redo at its end. A pass cut short while another live pass fills the checkout now
  leaves both to that one. Piped `--wait 3`, 1 s into the rebuild: 4.0–5.3 s → 2.8–3.2 s.
- **Settled means no file a retry would read can beat it.** Under `-a`, the demand walk
  read only this checkout, but its flag settled a prefix match from any checkout; and a
  prefix match from a checkout nothing was indexing stayed provisional forever, asking for
  a retry that reads no more of it. A literal match now settles once neither this checkout
  (demand walk done, or nothing left to warm) nor another checkout someone is indexing can
  hold the name unread; a fuzzy one once neither has anything unread. `continuing` (and so
  `interrupted: false`) belongs only to the checkout this search leaves a warm behind for,
  and another checkout's hint names its root (`rq --index <root>`). Outside `-a` nothing
  changes: this checkout is the only one read.
  A second hunt found the rule applied only to a top match from an incomplete checkout:
  from a complete one, `-a` answered `widget_maker` (prefix, 0.9, exit 0) while another
  checkout being indexed held `class Widget` unread, and never waited, since only its own
  warm made it poll. The rule now holds whatever checkout the match came from, the
  search waits on another checkout's indexer as on its own warm, and a provisional
  answer's `warming` and hint describe the checkout holding it back rather than the top
  match's.
- **`read` and `of` count one population.** `read` counts every file the index holds;
  `of` counted what `git ls-files` lists, so the untracked files an explicit index reads
  pushed `read` past `of`, and the clamp hid the unread tracked files ("6 of 6 files read",
  confidence 1.0, with two tracked files unread). `of` is now git's tracked files plus
  those the index holds that git doesn't, recounted at a pass's end, dropped once the
  checkout is complete, and counted on demand for a partial index an older rq left without
  one. Outside git nothing counts the tree short of walking it, so `of` is omitted (never
  `null`) and `confidence` is 0: the share read is what backs it, and an unknown share
  backs none. Scaling is in whole hundredths (29 of 100 at 1.0 is 0.29, not 0.28).
- **"Still indexing" is a promise a process keeps.** `interrupted: false` told a caller
  rq was still indexing, but the warm a search left behind bowed out to an `rq --warm`
  already holding the lock, and that child stopped at its 20 s budget: in a second hunt,
  10 of 16 searches 1 s into an `rq --warm` rebuild of the 100k corpus left coverage at
  38–85k of 97k with nothing indexing it until the next search. The budget now bounds a
  pass, not the sweep: a warm child keeps sweeping while its passes make progress, until
  the checkout completes, within half the warm lock's TTL (5 min), so its lock never
  reads as a crashed warmer's. Rejected: respawning a successor at the budget, which is
  the same sweep with a process boundary and a race for the lock in the middle; and
  claiming `continuing` only when some process will finish, which a search can't know
  of a child it didn't start. A repo that needs more than 5 minutes still stops part-way,
  as a crashed warm does. Measured on that harness, 8 runs per build alternating, load
  18–26 from other work: once every warm had exited, 1 of 8 base runs had a complete
  index (the rest 31–72k of 97k), against 8 of 8 (all done 49–64 s after the rebuild
  began); exit 2 went from 2 of 8 to 0 of 8.
- **`--status` reads a checkout a live pass holds as `warming`**, with the same `of`. A
  cold pass reads every file for a search's name before it writes any, so for seconds it
  showed `unindexed 0 files` mid-rebuild.
- **`--show` gates on the ranking, not the share read.** Its 0.85 bar asks whether the top
  match is the definition meant among the candidates, which is what `--open` acts on
  without a bar. Gated on the scaled `confidence`, `--show` refused a settled answer
  until ~85% of the tree was read and told a one-candidate list to "narrow the query".
  Now a settled hit shows when its unscaled confidence clears the bar, with the same
  stderr line a listed hit gets; the `confidence` field stays scaled. A provisional
  answer still isn't shown, and says only that.
- **Text says what JSON says.** With nothing continuing the index (`interrupted: true`),
  the stderr line reads "indexing stopped part-way" and names the `rq --index` that
  finishes it, rather than "still indexing".
- **Paper cuts.** A `warming` miss carries the same `warming` object a provisional answer
  does (and text its "N of M files read"); `provisional` lists its hits in a result's key
  order rather than sorted, inside the same alphabetical status object a miss is; "1 file
  read". A warm lock past its TTL no longer counts as a live indexer (`cmd_warm` already
  took it over at that age), so a reused pid can't hold a scripted search for its whole
  budget. A pass mark gets the same bound: an explicit `rq --index` may run for as long as the
  repo takes, so a pass renews its mark as it writes (each minute, well inside the
  10-minute TTL), and a mark unrenewed past the TTL is a crashed pass's whose pid may be
  reused — not counted, and cleared by the next pass. A pid that answers EPERM still
  counts as alive: an `rq` run under `sudo` writing the same database is one, and reading
  it dead would clear a live pass's mark and call its index `interrupted`; the TTL and
  the stall bound below cover what a reused one costs.
- **Second-hunt paper cuts.** A pass mark is written under the write lock taken up front:
  read then written in a deferred transaction, a busy writer failed the upgrade at once,
  busy timeout or not, so an `rq --index` during an `rq --warm` rebuild exited 74
  "database is locked" (4 of 12 runs) and a search's own warm died silently (now logged
  at `-v`). That left a second, older cause of the same exit: a cold pass's end rebuilds
  the name index in one transaction, which on the 100k corpus under load outlasted the 3 s
  busy timeout of an `rq --index` writing alongside. A pass nobody waits on — `rq --index`
  and a warm child — now waits out another writer for 30 s; a search keeps 3 s, since
  its in-process warm is joined before the process exits. `rq --index` 1–12 s into an
  `rq --warm` rebuild of the 100k corpus: 66c08a2 failed 1 of 12, the pass-mark fix
  alone 4 of 16 (the rebuild cause, now hit at delays of 5–8 s), both fixes 0 of 14. A miss's `warming` lists its keys in a result's order, as a provisional
  answer's does, so one `-J` stream never mixes two. `of` for a partial index an older rq
  left without one is counted once and kept — a best-effort write that never waits on a
  busy writer, whose pass records its own — rather than recounted by every query that
  shows the checkout (`rq Account -a` from a small repo beside a partial 97k-file one:
  first answer 144–321 ms on every query before; after, 828 ms once — 634 ms of it the
  count, under load — then 7–12 ms), and
  `--status` reports that same `of`. A sparse checkout's files outside its cone
  (skip-worktree and not on disk) are neither enumerated nor counted, so 2 files of a
  10-file tree read "2 of 2", not "2 of 10"; a skip-worktree file still on disk (hiding
  local edits) stays in. `--open` prints the stderr line `--show` does for a settled
  answer from a partial index.

- **Following another checkout's indexer, at a terminal, looks like waiting on one's own.**
  The second hunt's `-a` follow drew no progress line and installed no Ctrl-C handler from
  a complete checkout (both hung off the search's own block), so it sat silent for the
  programmatic 60 s budget: 7.4 s behind a live `rq --warm` of a 30k-file checkout, the
  full 60 s behind a stopped one or a stale mark, then exit 2. Now the progress line names
  the checkout being waited on, Ctrl-C prints the provisional answer, and a terminal waits
  until answered as it does on its own warm. **The stall bound:** a search stops following
  other processes once nobody has committed to the index for 5 s (`PRAGMA data_version`,
  which moves on any other connection's commit) and answers provisionally, naming the
  `rq --index` that finishes the checkout. A 97k-file `rq --warm` rebuild, sampled every
  50 ms, never went 1 s without a commit (longest gap 0.9 s, the closing name index
  rebuild, load 6); a stopped process or a crashed pass's mark never commits again.
  Rejected: counting the followed checkout's files instead, which stands still through a
  cold pass's name index rebuild and a demand scan that finds nothing to write; and
  treating an EPERM pid as dead (above).

- **`rq --index` behind another writer waits once, and says so.** With the 30 s wait, a
  write lock held 45 s failed `rq --index` at 44.6 s and one held 120 s at 64.5 s (0.60.1:
  6.8 s), all of it in `index: setup` on a complete checkout. Not a stale snapshot: the
  first write (the repository upsert) timed out after its 30 s, and the failure path then
  resolved the checkout's identity for output it never printed — a write that waited 30 s
  more. Now the identity is resolved only on success; the setup's upserts share one
  transaction that takes the write lock up front, so a busy writer costs one wait, not
  one per statement; and the pass's waits use rq's own busy handler (the bound per wait,
  20 ms polls) so that past a second an interactive `rq --index` prints once that it is
  waiting for another rq writing the index. Each wait is still bounded on its own, not
  summed over the pass: a long `rq --index` beside a warm interleaves many short waits,
  and a pass-wide total would fail it for making progress. The audit of the index path
  found no other read-then-write in a deferred transaction: the rest take the lock up
  front or write first (`forget_file`, `set_file_git_ts`).

- **Known gaps, left open at 0.60.2.** A sparse checkout's cone widening isn't noticed
  until `rq --index` (older than 0.60.1). `--drop` while an `rq --index` runs can fail
  that pass on a foreign key (older). The representative a duplicate collapse keeps
  depends on file insertion order, which the parallel parse makes vary run to run: two
  `rq --index` builds of the 100k corpus by one binary listed the same 5,116
  `initialize` results with one representative swapped, so the hunt's 5,116 vs 5,117
  across builds is that, not a change in order. `--wait 3` at a pty 1 s into a rebuild
  took 4.8 s in the hunt; not reproduced at load 6–8 (`user` 3.1–3.5 s, `User`
  0.4–0.7 s, base and new alike), so `poll_fits` is left as it is.

- **A live candidate is scaled as an indexed one (after 0.60.2).** A user's
  `--no-wait` queries during a rebuild got provisional lists mixing index hits
  scaled by `read / of` (a wrong one at 0.03) with `source: live` hits at their
  raw 0.9 and 0.47, and no `warming` on the live ones. The live scan runs when no
  pass has finished for the checkout (after `--drop`, or before a first index
  writes coverage) and the index holds no exact or prefix match. Reproduced on the
  100k corpus 0.6 s after `--drop`: `rq User --no-wait` answered exit 0, `live`,
  confidence 0.67, no `warming` — the complete index ties several `User`s at 0.50.
  What backs a live candidate is what this answer has read of the tree: the
  index's files and the scan's. The scan stops at its 250 ms budget having parsed
  57–1,448 of 97,146 files (`User`, `Order`, `Logger`, `usr`, on an empty index),
  so it adds at most 0.015 to the share the index already backs — nothing at
  whole hundredths once the index holds anything. So every hit from a checkout
  rq is indexing, live or not, carries that checkout's `warming` and is scaled by
  its `read / of`: one scale per answer, checkable against the `warming` it
  carries. Before a pass registers the checkout, `read` is 0 and `of` is git's
  tracked count (not kept: the pass records its own). Now: `User` 0.6 s after
  `--drop` answers `live`, `warming: {read: 0, of: 97146}`, confidence 0.00.
  A dir outside git that rq doesn't track is unchanged: nothing is indexing it,
  and the scan — unbounded by an index's share — is the answer.
  Rejected: **`max(index read, files the scan parsed) / of`**, the union's floor,
  which equals the index share at whole hundredths in every sample above and
  needs a second count in the output to stay checkable; and **a
  `confidence_basis: live|index` marker**, which would leave two scales in one
  answer and only label the mismatch — `source` already says which is which.
- **Warm latency: no regression to remove.** The same user's warm benchmark read
  23–91 ms on 0.60.1 and 25–106 ms on 0.60.2, the top end a bare class name with
  ~7.6k candidates. Measured on one complete index of the 100k corpus (948k
  symbols), 0.60.1, 0.60.2 and this change interleaved over 12 rounds after a
  warm-up, `--json -l 5`, load 3–5: `Test` (8,000 candidates recalled) p50 58.2 /
  59.2 / 59.1 ms, p90 60.0 / 60.0 / 61.1; `initialize` 53.1 / 53.4 / 54.1; `User`
  36.5 / 36.2 / 37.1; `Error`, `Logger`, `Mail::Message`, `ConnectionPool` within
  0.5 ms of each other. `--profile` attributes the same time to recall, score
  and sort in both releases (21–25, 21, 8 ms for `Test`), and no warming span
  runs: on a complete checkout the warming checks are a coverage read per root,
  under the profile's 0.1 ms resolution. Rejected: **skipping the warming
  machinery up front on a complete checkout**, which would save nothing
  measurable and add a second path to keep in step. The user's 91 → 106 ms
  didn't reproduce; it is a single number per release from a machine we can't
  sample, and this harness's own single runs of `Test` spanned 57–61 ms.

## D53 — Untracked files are in a checkout's index, and a warm keeps them

**Adopted**, 2026-09-30. The candidate list in `run_index` (`src/index/mod.rs`); e2e test
`an_untracked_file_an_explicit_index_read_survives_a_warm_completing_the_checkout`.

*The problem.* The two passes disagreed about untracked files. An explicit `rq --index`
walks the disk (honouring `.gitignore`) and reads them; a warm enumerates `git ls-files
--cached`, which lists only tracked files, and a warm that completes the checkout
reconciles away every held file it didn't see. So a file `rq --index --path a` read was
forgotten when a search's warm later finished the checkout — `rq Untracked1` exited 1
while the file sat on disk — and a complete index lost them the next time an edit sent a
warm over it. Older than D52 (0.60.1 does it too); D52's `of` counted them meanwhile.

*What.* In. A new file not yet `git add`ed is exactly what someone navigating their own
work looks for, and an explicit index already reads them. A warm's candidates are git's
tracked files plus the files the index holds that git doesn't list, when they're still
on disk: re-read like any other (an mtime match skips them), and reconciled away once
deleted. `of` already counts that population (D52 addendum).

*Rejected.* **`git ls-files --others --exclude-standard`** in the warm, so it discovers
untracked files too: on the 97k-file corpus it takes 2.0–2.6 s against 0.04–0.06 s for
`--cached`, a tree walk on every pass. A warm doesn't find an untracked file no pass has
read; an explicit index does. **Out** — the explicit index skipping them in a git repo —
would make a brand-new file unfindable until it's added.

*Known gaps.* Renaming an untracked file drops it until `rq --index` (the warm forgets
the old path and never discovers the new one), and a file ignored after it was indexed
stays until `rq --index`.

*Reverses if:* git's untracked cache (or fsmonitor) makes `--others` cheap enough to run
per pass.

## D54 — A pass says what it is doing: `warming.phase`

**Adopted**, 2026-10-01. `set_pass_phase`/`pass_phase` in `src/store/mod.rs`, the
two phase writes in `run_index` (`src/index/mod.rs`), `warming_state` and
`finishing_note` in `src/cli/mod.rs`; e2e test `a_pass_past_its_reads_says_it_is_finishing`.

*The problem.* A user polling `--no-wait` once a second through a rebuild of a
~109k-file monorepo saw `warming.read` sit at 50,000 for about 10 s, then jump. A
caller deciding from `read`/`of` whether to keep waiting reads that as a hang.

*What happens there.* 50,000 is a pass's file cap. When a pass's reads end it
rebuilds the name index (one transaction), reads commit times (`git log`, then an
update per path), records coverage and recounts the span; a warm child then starts
its next pass, which sets up, enumerates, and stats the files it already holds
before it reads a new one. None of that adds a file, so `read` stands still.
Measured on the 100k corpus (97k Ruby files, 948k symbols; release, 8 cores), a
cold rebuild by `rq --warm` sampled every 50 ms from another connection: `read`
stood at 49,994 for 1.3 s (load 4–8), 1.4 s with 1/s `--no-wait` queries running,
and 1.41 s under eight CPU hogs. `--profile`: name index 0.48–0.56 s, commit times
0.41–0.52 s, the next pass's setup and enumerate 0.09–0.19 s, then its first batch.
The ~10 s the user saw didn't reproduce here. What scales with their repo and not
this corpus: commit times (this corpus has one commit; `git log -n1000
--name-only` takes 0.08–0.44 s on rails, ruby and discourse) and a warm child's
`nice 10` and throttled I/O under foreground load.

*What.* `warming` carries `phase` — `reading`, or `finishing` once a pass's reads
end — and `phase_secs`, whole seconds in it, wherever `warming` appears (results,
a `provisional` answer, a `warming` miss) and on `--status` rows for a checkout
not complete. Both are omitted, never `null`, when no live pass records a phase:
nothing is indexing (`interrupted: true`), or the pass is an older rq's. Several
passes: `reading` wins, since `read` then moves. Text adds ", finishing a pass
(N s)" to its "N of M files read". Storage is `phase:<root>:<pid>` in `meta` —
`<phase>:<unix seconds>` — written at the pass's start (before it enumerates, so a
warm child's next pass reads `reading` from its setup), moved to `finishing` when
the reads end, and deleted with the pass mark; it counts only beside a live mark
or warm lock, and the next pass clears a dead pid's. A separate key, not a suffix
on the pass mark, which an older rq would parse as a crashed pass's and clear. No
schema change; two small writes a pass. Observed in the same rebuild, `--status`
every 0.25 s: `reading 8` (s) to `finishing 0` at 49,994, to `reading 0` within a
second.

*The stall bound, checked.* D52's 5 s bound on following another process's
indexing counts commits, and the steps above commit only at their ends. The
longest stretch without one, in the runs above: 0.74 s (load 4–8) and 1.32 s
(eight hogs), both the closing name index rebuild of 948k symbols; the cap
boundary's own was 0.46–0.55 s. The bound holds here with room. Left unchanged:
exempting a `finishing` pass from it would make it a rule with an exception, and
nothing measured needs one. `finishing` is now itself a commit, so the clock
restarts where the long steps begin.

*Rejected.*
- **Counting `read` more finely.** `read` is the files the index holds; nothing
  in that window adds one, so a finer count stands just as still.
- **A last-progress timestamp.** The name index rebuild is one transaction with
  no commit to stamp, so the timestamp would stall exactly where `read` does.
- **`committing` as the name.** The commits are the cheap part; the window is the
  name index and commit times.

*Reverses if:* a profile (`RQ_PROFILE=1 rq --warm`, or `phase_secs` on a
`finishing` pass) shows a step without a commit past 5 s on a real repo — then the
stall bound needs the phase, and that exception gets weighed on its numbers.
