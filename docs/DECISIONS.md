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

Recall: 0 sources moved; anchored unchanged (351 #1, 439 top 10).

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

*Rejected: halving the penalty in the anchor's file.* −200 still outweighs `proximity`
plus one level of `enclosing` (150), which is where a helper in the test's own class
sits, so it keeps most of the misses it was meant to fix.

*Reverses if:* anchored use from tests shows the in-file fakes winning over library calls
more often than the in-file helpers they were meant to find.
