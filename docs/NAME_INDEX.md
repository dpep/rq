# The name index: fuzzy recall that holds exactly what the scorer accepts

Fuzzy recall used to be three broad nets (first letter, any shared trigram, a file-path
`LIKE`) filtered by the scorer's necessary condition and cut at a cap (D12). The name index
replaces all three. For each repo it keeps one fixed-size **signature** per distinct symbol
name and per file. A query screens every signature with a few AND instructions, verifies
the survivors exactly, and fetches rows only for the names and files the scorer will
accept. The decision and its numbers are D23 in [DECISIONS.md](DECISIONS.md); this is how it
works, and the design spike it came from.

It is the default recall since D24: complete recall finds 105 more sources on the harness,
and exposed ranking weaknesses the capped nets hid (D23), which D24 fixed. Since D26 it is
the only fuzzy recall: the trigram table and the nets are gone.

## Why an index is possible

D21 failed because the net it replaced ("shares any trigram") can't prune. But the scorer
never asked for a plain subsequence. `align` accepts only this shape: the first query
letter anywhere; each later letter either stays in the current word within a gap of 2
(`MAX_NONBOUNDARY_GAP`), or lands on the *first letter of a later word* (or its second,
when the first is a vowel a skeleton dropped, D41). Words may be skipped, at a price in
the score (D42), but a later word is never entered further in. So every consecutive query
pair `(a, b)` must be a **transition pair** of the name:

- `(name[j], name[i])`, `i` mid-word, `j` in the same word, `i - j <= 3`
- `(name[j], name[i])`, `i` a word start, `j` anywhere before it
- `(name[j], name[i])`, `i` the second letter of a word starting with a vowel, `j`
  anywhere before that word
- plus alphanumerics adjacent across separators, which is how an exact, prefix,
  separator-free (`joiners_eq`) or glob match steps

With 37 codes (26 letters, 10 digits, "any other alphanumeric") there are 1,369 possible
pairs. Before D42 a name held ~37–51 of them; skipping words roughly doubles that, and a
7-letter query's 6 pairs stay selective.
That is a prunable necessary condition, which D21's reversal clause asked for.

`score::transition_pairs` generates the pairs, next to `align`, whose rules it encodes.
`score::NAME_INDEX_FORMAT` is derived from the pair function's version and `align`'s gap
constant and stamped on every repo's index, so a change to the matcher's rules rebuilds
the index instead of reading a stale one.

## The signature (`search/names.rs`)

40 bytes per name:

| bits | what |
|---|---|
| 37 | which codes the name holds |
| 8 + 6 + 6 | the typo key: the lowercased name's length, and its first two characters' codes |
| 256 | a Bloom filter of the name's transition pairs (one hash) |

For a non-ASCII name the Unicode-lowercased form's adjacent pairs and codes are added too,
since exact and prefix compare `name_lower`, which can differ from the name letter by
letter (`İ` lowercases to two characters).

A query compiles to a `Probe`: the codes and pairs it needs (a glob's `*` and `?` break
adjacency), and, for a non-glob query of four or more characters, the typo key. A name
survives the **screen** if it holds every needed code and pair, or if it could be a near
miss: a length within two, the first character right or swapped with the second, and at
most two of the query's codes absent. Survivors are then **verified** by the scorer's own
chain: prefix (which covers exact), `joiners_eq`, alignment, glob, or `near_miss_distance`.
The alignment check is `score::aligns`, a bit-parallel reachability over the same
transitions (u128 masks; a word start is reachable from any earlier position); a name longer
than 128 bytes or not ASCII goes through `align` itself.

The scorer folds a name's letters one at a time (`score::fold`) to meet a query lowercased
whole, so `σασprs` finds `ΣΑΣParser`. A letter whose lowercase is ASCII (`İ`, the Kelvin
sign) keeps its case: `pair_code` codes it as non-ASCII, and folding it to `i` or `k` would
let the scorer accept a name the screen had dropped.

The result is the exact set of names `score` accepts on the name alone. A property test
(`search::names::tests`) checks it against rq's real `score` over the harness's queries and
names, plus derived and edge-case ones, and an ignored test runs every harness query
against every name in rails and discourse.

## Storage: chunks in SQLite

```sql
name_sigs (repository_id, kind, chunk, n, sigs BLOB, keys BLOB)  -- PRIMARY KEY (repository_id, kind, chunk)
name_index (repository_id PRIMARY KEY, format, built)
```

Two kinds of key: distinct symbol names (kind 0), and repo-relative file paths (kind 1),
signed by their stem, because `score` lets a file named like the query surface its
primary definitions. Keys are stored in append order, in chunks of up to 512. `sigs` holds
a chunk's signatures back to back; `keys` holds each key's end offset (u32), then the keys'
bytes. The scan reads a chunk's `keys` only when one of its signatures survives.

- **Appends ride the symbols' transaction.** `replace_files` asks, before it rewrites a
  file, whether each of its names is already in the repo, and whether the file is new;
  the new ones are appended to the last chunk in the same transaction. A reader sees
  names and symbols together.
- **Deletes are free.** A name or file with no rows left still screens and verifies,
  then fetches nothing. It stays until the next rebuild.
- **Rebuild** writes the repo's chunks from `SELECT DISTINCT name` and its file paths,
  and stamps `name_index`. It runs at the end of any index pass whose repo's index is
  missing or stamped with another format, at the end of a cold pass, and as compaction
  once the keys appended since the last rebuild pass a quarter of what it wrote (at
  least 1,000).
- **A cold pass suspends the index.** It marks the repo's `name_index` suspended, drops
  its chunks and skips appends, then rebuilds at its end. The
  rebuild writes every chunk into contiguous pages; appending between the pass's batches
  scatters them among the symbols' pages, which costs every later scan (D25). Meanwhile
  recall reads the suspended repo's distinct names and file paths from its rows and
  signs and verifies each: complete over what's committed, 55–120 ms at rails and
  discourse size, and only while the pass runs.
- **Missing or stale means rebuild, then read.** Before recall reads the index it
  rebuilds every repo searched whose `name_index` is missing or holds another format:
  once per repo after an upgrade, 62 ms for rails and 103–110 for discourse. A new repo
  starts with an empty, current index. If another writer holds the lock past the busy
  timeout, the rebuild is left to a later search and recall reads that repo from its rows,
  as it does a suspended one (D26).

A flat file next to the database, mapped per query, measured faster (below), but it
needs its own cross-process publish protocol, crash reconciliation with SQLite, and a
lifecycle for `--drop` and a replaced `RQ_DB`; SQLite gives all three for free.

| 2,372 queries, in process, fresh map or connection each | median | p90 | p99 |
|---|---|---|---|
| in memory (floor) | 172 µs | 628 | 2,564 |
| flat file, mapped per query | 280 | 730 | 2,663 |
| SQLite chunks, fresh connection | 540 | 1,008 | 3,044 |

## Recall

`Store::search_candidates` keeps the exact and prefix layers and their fast path. After
them, the name index replaced the first-letter net, the trigram net, the path `LIKE` net,
`rq_keep` and `NET_WINDOW`: scan the repo's name signatures, verify survivors, fetch rows by
`(repository_id, name_lower)`; scan its file signatures, verify stems, fetch the primary
definitions of the accepted files. The cap still bounds the rows. When the accepted names
hold more rows than the cap, they are fetched best first by the value `score` gives the
name (glob or alignment), not in whatever order a net met them. `score()` still sees whole
rows and is unchanged, so this isn't D1's two-phase split.

Schema v18 dropped the FTS table, its triggers and the `name_lower`-only index its
first-letter net read (D26). It builds no index: recall rebuilds each repo's on first need.

## The spike

2026-09-27, against rq 0.53.0, in a standalone bench crate: rails (dfd1e95, 32,987
distinct names) and discourse (d55f1dc, 44,708) indexed by `rq --index`, the 2,372 queries
of `script/recall/queries.tsv`, and synthetic 300k and 2M name sets (real name shapes
refilled with same-length real words), with a 2M-symbol SQLite database in rq's schema to
run the old SQL at that scale. Machine load 5–22 on 8 cores throughout, so every method was
timed per query, interleaved, median of 3–5 reps: read the ratios.

Ground truth was the scorer's functions ported verbatim, run over every name. All
structures below produced exactly that set, with **0 violations on all 2,372 real queries
and on the 300k and 2M runs**.

| structure | what it is | per query |
|---|---|---|
| A. pair postings | 1,369 posting lists over name ids (dense ones as bitmaps, the rest delta-varint), smallest-first galloping intersect, then verify | the smallest list's probes + verify |
| B. signature scan (built) | presence + 256-bit pair Bloom per name, scanned flat, then verify | O(n) at ~2 ns a name + verify |
| C. presence scan | the 64-bit presence mask alone | O(n) + far more survivors |
| D. no index | verify every name | O(n) at ~100 ns a name |
| E. `could_match` scan | D12's gate over every name, in memory | O(n) at ~80 ns a name, unverified |
| old nets | first letter (≤ 6 chars) + trigram OR + `rq_keep` + 4× window, cap 8,000 | FTS walk + decode |

Recall latency, µs (median / p90 / p99), in memory:

| | rails 33k | discourse 45k | synth 300k | synth 2M |
|---|---|---|---|---|
| A. pair postings, verified | 33 / 229 / 683 | 13 / 49 / 173 | 178 / 887 / 3,246 | 921 / 5,093 / 14,405 |
| B. signature scan, verified | 78 / 377 / 1,119 | 52 / 122 / 315 | 715 / 2,120 / 5,216 | 4,796 / 12,578 / 49,427 |
| C. presence scan, verified | 679 / 1,611 / 2,591 | 249 / 831 / 1,867 | 5,530 / 12,894 / 20,879 | 38,335 / 83,676 / 177,357 |
| D. verify every name | 3,307 / 4,004 / 6,103 | 3,578 / 4,608 / 8,317 | 35,451 / 43,510 / 49,901 | 222,103 / 324,811 / 461,657 |
| E. `could_match` scan | 2,312 / 3,186 / 4,853 | 2,645 / 3,621 / 6,348 | 34,277 / 40,409 / 57,718 | 330,046 / 465,475 / 742,967 |
| **old nets (SQLite)** | **7,637 / 21,642 / 35,220** | **5,001 / 11,227 / 20,020** | — | **46,560 / 96,711 / 214,473** |

The old nets don't hold what the scorer would accept: 63.6% of accepted names on rails,
51.9% on discourse, 13.6% at 2M. Queries whose source is reachable at all (the harness's
"found" ceiling) went from 1,892 of 2,314 (81.8%) to 1,999 (86.4%) with complete recall.
At 2M it was 32.9% against 82.4%: D3's "the cost doesn't grow with the corpus" held for the
decode but not for recall, which the cap truncated harder as the net grew.

Sizes, MB: signatures 1.32 (rails), 1.79 (discourse), 12.0 (300k), 80.0 (2M); the names
themselves about as much again. Pair postings (A) came to 1.20 / 1.35 / 11.1 / 76.3.

Rejected before building: **SymSpell** (C(L,2) deletions per name is ~100 entries a name,
200M at 2M, to serve the typo row that is already 94.5% #1); **BK-tree** (no pruning at
distance 2 on short keys); **fst, FM-index, suffix array** (a substring structure answers
the wrong question: abbreviations aren't substrings, and D21 measured the fst walk visiting
every key).

## Why the scan and not the postings

The spike recommended A: 2–6× faster than B at real sizes, and the only one of the two
still under 1 ms median at 2M. B was built because it is append-only. A needs a base plus
a delta table, a watermark, and a background rebuild; B appends a record and never
rebuilds except to compact. At rails and discourse sizes both are two orders of magnitude
under the old nets. `transition_pairs` is the one piece they share, so postings can be
layered on the same pairs and verifier if a repo with a million names turns up.

Costs accepted: the pair function encodes `align`'s transition rules, so changing those
rules changes the index format (and rebuilds every repo's index). D42 did exactly that
for word skips, the fix for `first+last`'s low rate: pairs of "any earlier character →
word initial", and `aligns` reaches every word start after the earliest position it holds,
which replaced its Kogge-Stone fill.
