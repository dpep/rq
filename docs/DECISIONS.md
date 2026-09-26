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
list). That's imperceptible, and a toll on every future exit path.

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
