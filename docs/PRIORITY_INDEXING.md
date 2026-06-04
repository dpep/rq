# Priority-guided indexing (design sketch)

> **Status: design, not built.** The cheap half (path-guided warming) shipped in
> `index::run_index`; this captures the full best-first scheduler for when we
> have a large-repo workload to measure it against. Don't build it before there's
> a repo big enough that the budget can't index everything — that's the only
> regime where picking *what* to index next beats just indexing fast.

## Problem

On a cold or incomplete index of a **huge** repo, the warm budget (≈500 ms
inline + amortized deferred) can't reach every file. Indexing in walk order
wastes the budget on irrelevant files and misses what the user just searched
for. We want a **time-bounded, best-first traversal**: spend the budget on the
files most likely to satisfy the current (and next) search, letting cheap and
late-arriving signals re-order what's indexed next.

## Data structure

A **priority queue** — a binary max-heap (`std::collections::BinaryHeap`) keyed
by a priority score. ("Weighted heap" = priority queue; a tree is the wrong tool
— we want "repeatedly pop the most promising," not ordered lookup.) The frontier
is exactly a greedy/best-first search frontier (cf. Dijkstra/A*).

Two wrinkles:
- **No decrease-key.** `BinaryHeap` can't cheaply raise an existing entry's
  priority. Use **lazy deletion**: push a new `(higher_priority, path)` and keep
  a `done`/`in_flight` set; when a worker pops a path already handled, skip it.
- **Re-prioritization is push-only**, which fits producers that discover signals
  over time (content match, git recency) and just push higher-priority entries.

## Pipeline (parallel scan → shared queue → parallel parse → serialized write)

Parsing must stay parallel (it's the cost); only the SQLite write serializes
(WAL = one writer). So a single indexer thread is wrong — the heap is *shared*,
parse workers are *many*, and writes funnel to *one*:

```
            push (priority, path)
 ┌─────────────────────────────┐         ┌──────────────────────────────┐
 │ scanners (N threads)         │         │ parse workers (M threads)    │
 │  walk + read + cheap scan:   │ ──────▶ │  pop highest priority        │
 │   • path match (free)        │  shared │  (skip if already done)      │
 │   • content substring        │  Mutex< │  tree-sitter parse (CPU)     │
 │   • git recency (async)      │  Heap>  │  ── FileSymbols ──┐          │
 │   • neighbor expansion       │  +done  │                   │          │
 └─────────────────────────────┘  set    └───────────────────┼──────────┘
                                                              ▼  mpsc
                                              ┌──────────────────────────┐
                                              │ writer (1 thread)        │
                                              │  batch → one transaction │
                                              │  (WAL single writer)     │
                                              └──────────────────────────┘
```

- **Shared frontier:** `Mutex<BinaryHeap<Prioritized>>` + a `HashSet<PathKey>`
  for dedup-on-pop. Lock hold time is tiny (push/pop are O(log n)) next to the
  parse, so contention is low; a lock-free queue (crossbeam) is a later option if
  it ever shows up in a profile.
- **Parse workers** pop, check the `done` set, parse, and send `FileSymbols`
  over an `mpsc` channel. Parsing is fully parallel.
- **Writer** owns the SQLite connection and drains the channel into batched
  transactions (reuse `Store::replace_files`). One writer avoids `SQLITE_BUSY`
  churn; the alternative — many writers serialized by `busy_timeout` — works but
  contends. Prefer the single-writer channel.
- **Time boundary:** a shared `deadline`; workers stop popping once it passes.
  Whatever's indexed by then is the budget's best-effort best-first slice.

## Priority signals (highest → lowest)

1. **active / branch files** — what you're editing (already a signal today).
2. **path match** — path contains the query (`employee` → `employee.rb`,
   `…/employee/*`). Free, no read. **Shipped** in `run_index` today.
3. **content match** — file bytes contain the query (the live-scan pre-filter).
   Requires a read, so it's a *scanner* signal that arrives as files are read.
4. **git recency** — recently-committed files. `git log` is slow, so run it on a
   side thread and **slot files in as they materialize** (push with a recency
   boost); don't block the frontier waiting for git.
5. **neighbor expansion** — when a high-priority file is indexed, push its
   directory siblings with a small boost (locality: the definition you want is
   often near the file you found).

A file's priority is the sum of the signals known so far; late signals just push
a higher-priority duplicate (lazy deletion handles the rest).

## When to build it

Gate on **measurement**: a repo large enough that the budget genuinely can't
index everything, with a query set, comparing first-query hit-rate under
walk-order vs. best-first. Until then, path-guided warming + parallel parsing
already land most cold searches, and amortized warming reaches full coverage in
a few queries — so the scheduler's extra machinery (shared heap, channels,
async git, neighbor expansion) isn't yet proven to earn its complexity.
