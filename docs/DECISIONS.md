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
