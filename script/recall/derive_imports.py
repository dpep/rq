#!/usr/bin/env python3
"""Derive script/recall/anchored_imports.tsv: JS/TS queries asked from an import.

Each row is a named import (`import { a, b as c } from './x'`) in a pinned
JS/TS corpus (docs/RECALL.md): the query is the imported name, the anchor is
that name in the import statement, and the truth is the top-level definition of
it in the file the relative specifier resolves to. Kept only when:

- the specifier is relative and resolves to an indexed file (TypeScript's rules:
  extension and `/index` probing, and `./x.js` naming `./x.ts`),
- that file defines the name at top level (a re-export is skipped), and
- the name is defined at least twice in the repo (otherwise nothing to rank).

Run once, by hand; the output is committed. Needs a release build of rq.
Deterministic for a given corpus and seed.

    script/recall/derive_imports.py > script/recall/anchored_imports.tsv
"""
import argparse
import importlib.util
import json
import os
import posixpath
import random
import re
import sqlite3
import subprocess
import sys
import tempfile
from pathlib import Path

# the corpus pins and checkout logic are recall.py's
spec = importlib.util.spec_from_file_location("recall", Path(__file__).resolve().parent.parent / "recall.py")
recall = importlib.util.module_from_spec(spec)
spec.loader.exec_module(recall)

REPOS = ["next.js", "jest", "zod", "react"]
SITES_PER_REPO = 300
SITES_PER_NAME = 3
SEED = 32

IMPORT = re.compile(r"\bimport\s+(?:type\s+)?(?:[\w$]+\s*,\s*)?\{([^}]*)\}\s*from\s*(['\"])([^'\"]+)\2")
SPECIFIER = re.compile(r"(?:\btype\s+)?([A-Za-z_$][\w$]*)(?:\s+as\s+[\w$]+)?")
EXTS = [".ts", ".tsx", ".d.ts", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts"]
# `./x.js` in TypeScript's ESM style names the `.ts` file it compiles from
SOURCE_OF = {".js": [".ts", ".tsx"], ".jsx": [".tsx"], ".mjs": [".mts"], ".cjs": [".cts"]}


def resolve(importer, specifier, files):
    """The indexed file a relative specifier names, by TypeScript's probing."""
    base = posixpath.normpath(posixpath.join(posixpath.dirname(importer), specifier))
    stem, ext = posixpath.splitext(base)
    probes = [stem + e for e in SOURCE_OF.get(ext, [])] + [base]
    probes += [base + e for e in EXTS] + [f"{base}/index{e}" for e in EXTS]
    return next((p for p in probes if p in files), None)


def sites(root, file):
    """Each named import in `file`: (name, line, col, specifier)."""
    try:
        text = (root / file).read_text(errors="replace")
    except OSError:
        return
    for m in IMPORT.finditer(text):
        if not m.group(3).startswith("."):
            continue
        for s in SPECIFIER.finditer(m.group(1)):
            if s.group(1) in ("type", "default"):
                continue
            pos = m.start(1) + s.start(1)
            line = text.count("\n", 0, pos) + 1
            col = pos - (text.rfind("\n", 0, pos) + 1) + 1
            yield s.group(1), line, col, m.group(3)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--bin", default=str(recall.ROOT / "target" / "release" / "rq"))
    ap.add_argument("repos", nargs="*", default=REPOS)
    args = ap.parse_args()
    pins = json.loads((recall.DATA / "corpus.json").read_text())
    cache = recall.cache_dir(None)
    rng = random.Random(SEED)
    print("repo\tquery\tanchor\ttruth\trecv")
    with tempfile.TemporaryDirectory(prefix="rq-imports-") as tmp:
        for repo in args.repos:
            pin = pins[repo]
            path = recall.checkout(cache, repo, pin["url"], pin["sha"])
            db = os.path.join(tmp, f"{repo}.db")
            subprocess.run([args.bin, "--index", str(path)], env=recall.isolated_env(db),
                           stdout=subprocess.DEVNULL, check=True)
            con = sqlite3.connect(db)
            count, top = {}, {}
            for name, file, line, parent in con.execute(
                    "SELECT s.name, f.path, s.line, s.parent FROM symbols s JOIN files f ON f.id = s.file_id"):
                count[name] = count.get(name, 0) + 1
                if parent is None:
                    key = (file, name)
                    top[key] = min(top.get(key, line), line)
            files = {f for (f,) in con.execute(
                "SELECT path FROM files WHERE language IN ('typescript', 'javascript')")}

            by_name = {}
            for file in sorted(files):
                for name, line, col, specifier in sites(path, file):
                    target = resolve(file, specifier, files)
                    if target and (target, name) in top and count[name] >= 2:
                        by_name.setdefault(name, []).append((f"{file}:{line}:{col}", f"{target}:{top[(target, name)]}"))
            kept = 0
            for name in rng.sample(sorted(by_name), len(by_name)):
                take = min(SITES_PER_NAME, len(by_name[name]), SITES_PER_REPO - kept)
                for anchor, truth in rng.sample(by_name[name], take):
                    print(f"{repo}\t{name}\t{anchor}\t{truth}\timport")
                    kept += 1
            print(f"{repo}: {kept} import sites ({len(by_name)} names)", file=sys.stderr)


if __name__ == "__main__":
    main()
