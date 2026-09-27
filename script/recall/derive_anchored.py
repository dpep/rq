#!/usr/bin/env python3
"""Derive script/recall/anchored.tsv: queries asked from a real call site.

Each row is a Ruby method call in a pinned corpus (docs/RECALL.md): the query is
the called name, the anchor is the call site, and the truth is the one
definition trekr resolves it to. Kept only when:

- the name is defined at least twice in the repo (otherwise nothing to rank),
- trekr confirms the call and resolves it to exactly one definition in the repo
  at confidence >= 0.9, and
- rq indexes a definition of that name at that file and line.

Run once, by hand; the output is committed. Needs `trekr` on PATH and a release
build of rq. Deterministic for a given corpus, trekr and seed.

    script/recall/derive_anchored.py > script/recall/anchored.tsv
"""
import importlib.util
import json
import os
import random
import sqlite3
import subprocess
import sys
import tempfile
from pathlib import Path

# the corpus pins and checkout logic are recall.py's
spec = importlib.util.spec_from_file_location("recall", Path(__file__).resolve().parent.parent / "recall.py")
recall = importlib.util.module_from_spec(spec)
spec.loader.exec_module(recall)

NAMES_PER_REPO = 250
SITES_PER_NAME = 2
SEED = 18


def main():
    rq = recall.ROOT / "target" / "release" / "rq"
    pins = json.loads((recall.DATA / "corpus.json").read_text())
    cache = recall.cache_dir(None)
    rng = random.Random(SEED)
    print("repo\tquery\tanchor\ttruth\trecv")
    with tempfile.TemporaryDirectory(prefix="rq-anchored-") as tmp:
        for repo, pin in pins.items():
            path = recall.checkout(cache, repo, pin["url"], pin["sha"])
            db = os.path.join(tmp, f"{repo}.db")
            subprocess.run([str(rq), "--index", str(path)], env=recall.isolated_env(db),
                           stdout=subprocess.DEVNULL, check=True)
            con = sqlite3.connect(db)
            defs = {}
            for name, file, line in con.execute(
                    "SELECT s.name, f.path, s.line FROM symbols s JOIN files f ON f.id = s.file_id "
                    "WHERE s.language = 'ruby' AND s.kind = 'method' AND length(s.name) >= 3"):
                defs.setdefault(name, set()).add((file, line))
            names = sorted(n for n, d in defs.items() if len(d) >= 2)
            tenv = dict(os.environ, TREKR_DB=os.path.join(tmp, "trekr.db"), TREKR_JOBS="2")
            subprocess.run(["trekr", "--index", ".", "--no-gems"], cwd=path, env=tenv,
                           stdout=subprocess.DEVNULL, check=True)

            def trekr(*args):
                p = subprocess.run(["trekr", *args, "--json"], cwd=path, env=tenv,
                                   capture_output=True, text=True)
                return json.loads(p.stdout) if p.returncode == 0 and p.stdout.strip() else None

            kept = 0
            for name in rng.sample(names, min(NAMES_PER_REPO, len(names))):
                calls = [r for r in trekr("--refs", name) or []
                         if r.get("role") == "call" and r.get("tier") == "confirmed"
                         and r["path"].endswith(".rb")]
                for site in rng.sample(calls, min(SITES_PER_NAME, len(calls))):
                    at = f"{site['path']}:{site['line']}:{site['col']}"
                    d = trekr("--def", at)
                    if not d or d.get("status") != "resolved" or d.get("confidence", 0) < 0.9:
                        continue
                    sites = d.get("sites") or []
                    if len(sites) != 1 or not sites[0]["path"].startswith(str(path) + "/"):
                        continue
                    truth = (sites[0]["path"][len(str(path)) + 1:], sites[0]["line"])
                    if truth not in defs[name]:
                        continue
                    print(f"{repo}\t{name}\t{at}\t{truth[0]}:{truth[1]}\t{site.get('recv', '')}")
                    kept += 1
            print(f"{repo}: {kept} anchored queries", file=sys.stderr)


if __name__ == "__main__":
    main()
