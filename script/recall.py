#!/usr/bin/env python3
"""Fuzzy-ranking recall over pinned real corpora. See docs/RECALL.md.

Every query in script/recall/queries.tsv was derived from a real symbol name in
rails or discourse (its ground truth, the `source`). This builds an isolated
index of both repos at the pinned commits, runs every query through rq, and
reports where each source ranks: #1, top 10, or found at all. With a baseline
it also lists the sources that lost #1 or the top 10.

    script/recall.py                    # target/release/rq
    script/recall.py --base main        # diff against a build of a git ref
    script/recall.py --base-bin OLD_RQ  # ...or against a binary you built
    script/recall.py --json             # machine-readable, stable field names

Not part of `cargo test` or CI: the corpora are fetched over the network (once,
cached) and a run takes minutes.
"""
import argparse
import json
import os
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from collections import Counter
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DATA = ROOT / "script" / "recall"
TYPES = ["abbr3", "abbr2", "first+last", "consonants", "typo", "glob"]
# Recency decays against the wall clock, so a checkout's real dates would make
# the numbers drift with the calendar. Every file and the one commit are dated
# here instead, far past the decay, which zeroes the signal for good.
FROZEN = 946684800  # 2000-01-01T00:00:00Z


def die(msg):
    print(f"recall: {msg}", file=sys.stderr)
    sys.exit(2)


def note(msg):
    print(msg, file=sys.stderr, flush=True)


def run(cmd, **kw):
    return subprocess.run(cmd, check=True, **kw)


def git(*args, cwd, env=None):
    p = subprocess.run(["git", "-C", str(cwd), *args], env=env, capture_output=True, text=True)
    if p.returncode:
        die(f"git {args[0]} failed in {cwd}: {p.stderr.strip()}")
    return p.stdout.strip()


# ----- corpus -----


def cache_dir(arg):
    if arg:
        return Path(arg).expanduser()
    if os.environ.get("RQ_RECALL_CACHE"):
        return Path(os.environ["RQ_RECALL_CACHE"]).expanduser()
    base = os.environ.get("XDG_CACHE_HOME") or Path.home() / ".cache"
    return Path(base) / "rq-recall"


def checkout(cache, repo, url, sha):
    """A checkout of `sha` under `cache`, fetched once (shallow) and reused."""
    dest = cache / f"{repo}-{sha[:12]}"
    ready = cache / f"{repo}-{sha[:12]}.ready"
    if ready.exists():
        return dest
    note(f"fetching {repo}@{sha[:12]} into {dest} (once)")
    shutil.rmtree(dest, ignore_errors=True)
    dest.mkdir(parents=True)
    git("init", "-q", cwd=dest)
    git("fetch", "-q", "--depth", "1", "--no-tags", url, sha, cwd=dest)
    # One commit on `main` holding the pinned tree, dated FROZEN: a trunk branch
    # has no branch-changed files, and `git log` gives every file the same old
    # time. `origin` keeps the repo identity rq would give a real clone.
    stamp = f"@{FROZEN} +0000"
    env = dict(os.environ, GIT_AUTHOR_NAME="rq recall", GIT_AUTHOR_EMAIL="recall@localhost",
               GIT_COMMITTER_NAME="rq recall", GIT_COMMITTER_EMAIL="recall@localhost",
               GIT_AUTHOR_DATE=stamp, GIT_COMMITTER_DATE=stamp)
    commit = git("commit-tree", f"{sha}^{{tree}}", "-m", f"{repo} {sha}", cwd=dest, env=env)
    git("update-ref", "refs/heads/main", commit, cwd=dest)
    git("symbolic-ref", "HEAD", "refs/heads/main", cwd=dest)
    git("reset", "-q", "--hard", cwd=dest)
    git("remote", "add", "origin", url, cwd=dest)
    for d, dirs, files in os.walk(dest):
        dirs[:] = [x for x in dirs if x != ".git"]
        for f in files:
            p = os.path.join(d, f)
            if not os.path.islink(p):
                os.utime(p, (FROZEN, FROZEN))
    git("update-index", "-q", "--refresh", cwd=dest)
    ready.touch()
    return dest


# ----- binaries -----


def build_ref(ref):
    """A release build of git `ref`, cached by commit under target/recall/."""
    sha = git("rev-parse", "--verify", "--end-of-options", f"{ref}^{{commit}}", cwd=ROOT)
    out = ROOT / "target" / "recall" / f"rq-{sha[:12]}"
    if out.exists():
        return out, sha
    note(f"building {ref} ({sha[:12]})")
    with tempfile.TemporaryDirectory(prefix="rq-recall-src-") as src:
        archive = subprocess.Popen(["git", "-C", str(ROOT), "archive", sha], stdout=subprocess.PIPE)
        run(["tar", "-x", "-C", src], stdin=archive.stdout)
        if archive.wait() != 0:
            die(f"git archive {sha} failed")
        # Homebrew's keg-only rustup may leave cargo off PATH (see CLAUDE.md)
        cargo = os.environ.get("CARGO") or shutil.which("cargo") or "/opt/homebrew/opt/rustup/bin/cargo"
        # a shared target dir, so successive baselines only rebuild rq itself
        env = dict(os.environ, CARGO_TARGET_DIR=str(ROOT / "target" / "recall" / "build"))
        run([cargo, "build", "--release", "--quiet", "--manifest-path", f"{src}/Cargo.toml"], env=env)
        shutil.copy2(ROOT / "target" / "recall" / "build" / "release" / "rq", out)
    return out, sha


# ----- one run -----


def load_queries():
    rows = []
    with open(DATA / "queries.tsv") as f:
        header = f.readline().rstrip("\n").split("\t")
        for line in f:
            rows.append(dict(zip(header, line.rstrip("\n").split("\t"))))
    return rows


def isolated_env(db):
    """Never the user's index. RQ_WARM_DETACH=0 leaves no background child writing
    to a DB about to be deleted. One parse worker, because parallel workers commit
    files in a different order each run, and rowid order breaks score ties and
    decides where a capped net truncates, so reruns would disagree."""
    return dict(os.environ, RQ_DB=db, RQ_WARM_DETACH="0", RQ_JOBS="1")


def measure(label, binary, corpus, queries, jobs):
    """Index the corpus into a throwaway DB and run every query through `binary`."""
    with tempfile.TemporaryDirectory(prefix="rq-recall-db-") as tmp:
        env = isolated_env(os.path.join(tmp, "rq.db"))
        t = time.monotonic()
        for path in corpus.values():
            run([str(binary), "--index", str(path)], env=env, stdout=subprocess.DEVNULL)
        index_s = time.monotonic() - t

        def one(q):
            p = subprocess.run([str(binary), q["query"], "--json", "--no-wait", "--limit", "0"],
                               cwd=corpus[q["repo"]], env=env, capture_output=True, text=True)
            try:
                hits = json.loads(p.stdout) if p.stdout.strip() else []
            except json.JSONDecodeError:
                die(f"{label}: unparseable output for {q['repo']} {q['query']!r}: {p.stdout[:200]}")
            if not isinstance(hits, list):
                hits = []
            names = [h.get("name") for h in hits]
            rank = names.index(q["source"]) + 1 if q["source"] in names else None
            top = [(h.get("name"), h.get("file"), h.get("line")) for h in hits[:10]]
            return {"rank": rank, "top": top}

        t = time.monotonic()
        with ThreadPoolExecutor(jobs) as ex:
            results = list(ex.map(one, queries))
        query_s = time.monotonic() - t
    return {"label": label, "bin": str(binary), "index_s": round(index_s, 1),
            "query_s": round(query_s, 1), "results": results}


# ----- report -----


def tally(rows):
    n = len(rows)
    c = Counter()
    for r in rows:
        k = r["rank"]
        c["first"] += k == 1
        c["top10"] += k is not None and k <= 10
        c["found"] += k is not None
    pct = lambda x: round(100 * x / n, 1) if n else 0.0  # noqa: E731
    return {"n": n, "first": c["first"], "top10": c["top10"], "found": c["found"],
            "first_pct": pct(c["first"]), "top10_pct": pct(c["top10"]), "found_pct": pct(c["found"])}


def summarize(run_, queries):
    sourced = [(q, r) for q, r in zip(queries, run_["results"]) if q["source"]]
    out = {k: run_[k] for k in ("label", "bin", "index_s", "query_s")}
    out.update(tally([r for _, r in sourced]))
    out["by_type"] = {t: tally([r for q, r in sourced if q["type"] == t]) for t in TYPES}
    return out


def diff(base, new, queries):
    far = float("inf")
    moves = Counter()
    lost_first, lost_top10 = [], []
    changed = 0
    for q, a, b in zip(queries, base["results"], new["results"]):
        changed += a["top"] != b["top"]
        if not q["source"]:
            continue
        ra, rb = a["rank"] or far, b["rank"] or far
        moves["up" if rb < ra else "down" if rb > ra else "same"] += 1
        row = {"repo": q["repo"], "query": q["query"], "type": q["type"], "source": q["source"],
               "base_rank": a["rank"], "new_rank": b["rank"],
               "new_first": b["top"][0][0] if b["top"] else None}
        if ra == 1 and rb != 1:
            lost_first.append(row)
        if ra <= 10 < rb:
            lost_top10.append(row)
    return {"up": moves["up"], "down": moves["down"], "same": moves["same"],
            "top10_changed": changed, "lost_first": lost_first, "lost_top10": lost_top10}


def print_report(report):
    c = report["corpus"]
    print("corpus: " + ", ".join(f"{r}@{v['sha'][:12]}" for r, v in c.items()) +
          f"  |  {report['queries']} queries, {report['sourced']} with a source")
    runs = report["runs"]
    width = max(len(r["label"]) for r in runs)
    print(f"\n{'':{width}}  {'source #1':>14}  {'top 10':>14}  {'found':>14}  {'index':>6}  {'queries':>7}")
    for r in runs:
        cell = lambda k: f"{r[k]:>5} {r[k + '_pct']:5.1f}%"  # noqa: E731
        print(f"{r['label']:{width}}  {cell('first'):>14}  {cell('top10'):>14}  {cell('found'):>14}"
              f"  {r['index_s']:5.0f}s  {r['query_s']:6.0f}s")

    print(f"\n{'type':<11} {'n':>4}  " + "  ".join(f"{'#1 ' + r['label'][:8]:>12}" for r in runs)
          + "  " + "  ".join(f"{'top10 ' + r['label'][:8]:>15}" for r in runs))
    for t in TYPES:
        n = runs[0]["by_type"][t]["n"]
        print(f"{t:<11} {n:>4}  "
              + "  ".join(f"{r['by_type'][t]['first_pct']:>11.1f}%" for r in runs) + "  "
              + "  ".join(f"{r['by_type'][t]['top10_pct']:>14.1f}%" for r in runs))

    d = report.get("diff")
    if d:
        print(f"\nsources: {d['up']} up, {d['down']} down, {d['same']} unchanged; "
              f"top 10 changed in {d['top10_changed']} of {report['queries']} queries")
        rank = lambda x: "-" if x is None else f"#{x}"  # noqa: E731
        for key, title in (("lost_first", "lost #1"), ("lost_top10", "lost the top 10")):
            print(f"\n{title}: {len(d[key])}")
            for x in d[key]:
                print(f"  {x['repo']:<9} {x['query']!r:<28} {x['source']:<34} "
                      f"{rank(x['base_rank']):>4} -> {rank(x['new_rank']):<5} now #1: {x['new_first']}")

    b = report.get("bench")
    if b:
        print(f"\nlatency, {b['reps']} interleaved reps x {b['queries']} hand-picked queries "
              "(median / p90 of per-query medians, ms):")
        for r in b["runs"]:
            print(f"  {r['label']:{width}}  query phase {r['query_ms']['median']:.1f} / {r['query_ms']['p90']:.1f}"
                  f"   first answer {r['first_ms']['median']:.1f} / {r['first_ms']['p90']:.1f}"
                  f"   wall {r['wall_ms']['median']:.1f} / {r['wall_ms']['p90']:.1f}")


# ----- latency -----


def bench(runs, corpus, queries, reps):
    """Interleave every binary on every hand-picked query, rotating the order per
    rep, so machine drift lands on each binary equally."""
    qs = [q for q in queries if q["type"] == "hand"]
    samples = {r["label"]: {i: [] for i in range(len(qs))} for r in runs}
    with tempfile.TemporaryDirectory(prefix="rq-recall-bench-") as tmp:
        envs = {}
        for r in runs:
            env = isolated_env(os.path.join(tmp, f"{len(envs)}.db"))
            for path in corpus.values():
                run([r["bin"], "--index", str(path)], env=env, stdout=subprocess.DEVNULL)
            envs[r["label"]] = env
        for rep in range(reps):
            order = runs[rep % len(runs):] + runs[:rep % len(runs)]
            for i, q in enumerate(qs):
                for r in order:
                    t = time.perf_counter()
                    p = subprocess.run([r["bin"], q["query"], "--json", "--no-wait", "--profile"],
                                       cwd=corpus[q["repo"]], env=envs[r["label"]],
                                       capture_output=True, text=True)
                    wall = (time.perf_counter() - t) * 1000
                    phases = {x["name"]: x["ms"] for x in json.loads(p.stderr.strip().splitlines()[-1])["phases"]}
                    samples[r["label"]][i].append(
                        {"query": phases["query"], "first": phases.get("first answer", phases["query"]), "wall": wall})

    def dist(label, key):
        meds = sorted(statistics.median(s[key] for s in samples[label][i]) for i in range(len(qs)))
        return {"median": round(statistics.median(meds), 1), "p90": round(meds[int(len(meds) * 0.9)], 1)}

    return {"reps": reps, "queries": len(qs),
            "runs": [{"label": r["label"], "query_ms": dist(r["label"], "query"),
                      "first_ms": dist(r["label"], "first"), "wall_ms": dist(r["label"], "wall")} for r in runs]}


# ----- main -----


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--bin", default=str(ROOT / "target" / "release" / "rq"),
                    help="the rq under test (default: target/release/rq)")
    base = ap.add_mutually_exclusive_group()
    base.add_argument("--base", metavar="REF", help="git ref to build and diff against")
    base.add_argument("--base-bin", metavar="PATH", help="baseline rq binary to diff against")
    ap.add_argument("--json", action="store_true", help="print the report as JSON")
    ap.add_argument("--fail-on-loss", action="store_true",
                    help="exit 1 if any source loses #1 or the top 10 against the baseline")
    ap.add_argument("--bench", type=int, metavar="REPS", default=0,
                    help="also time the hand-picked queries, interleaved, REPS times each")
    ap.add_argument("--jobs", type=int, default=4, help="queries in flight at once (default 4)")
    ap.add_argument("--cache", metavar="DIR",
                    help="where corpora are fetched (default $RQ_RECALL_CACHE, else ~/.cache/rq-recall)")
    args = ap.parse_args()

    if args.fail_on_loss and not (args.base or args.base_bin):
        die("--fail-on-loss needs a baseline (--base or --base-bin)")
    new_bin = Path(args.bin).resolve()
    if not new_bin.exists():
        die(f"{new_bin} not found; run `cargo build --release` (or `make recall`)")

    pins = json.loads((DATA / "corpus.json").read_text())
    cache = cache_dir(args.cache)
    corpus = {repo: checkout(cache, repo, p["url"], p["sha"]) for repo, p in pins.items()}
    queries = load_queries()

    bins = []
    if args.base:
        path, sha = build_ref(args.base)
        bins.append((f"{args.base}", path))
    elif args.base_bin:
        bins.append(("base", Path(args.base_bin).resolve()))
    bins.append(("new", new_bin))

    raw = []
    for label, path in bins:
        note(f"{label}: indexing and running {len(queries)} queries ({path})")
        raw.append(measure(label, path, corpus, queries, args.jobs))

    report = {"corpus": pins, "queries": len(queries),
              "sourced": sum(1 for q in queries if q["source"]),
              "runs": [summarize(r, queries) for r in raw]}
    if len(raw) == 2:
        report["diff"] = diff(raw[0], raw[1], queries)
    if args.bench:
        report["bench"] = bench(report["runs"], corpus, queries, args.bench)

    if args.json:
        json.dump(report, sys.stdout, indent=2)
        print()
    else:
        print_report(report)

    d = report.get("diff")
    if args.fail_on_loss and (d["lost_first"] or d["lost_top10"]):
        sys.exit(1)


if __name__ == "__main__":
    main()
