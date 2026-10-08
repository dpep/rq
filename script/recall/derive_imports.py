#!/usr/bin/env python3
"""Derive the JS/TS anchored sets: queries asked from an import.

Each row is a named import (`import { a, b as c } from './x'`) in a pinned
JS/TS corpus (docs/RECALL.md): the query is the imported name, the anchor is
that name in the import statement, and the truth is where it is defined.

By default (anchored_imports.tsv) the specifier is relative, and the truth is
the top-level definition of the name in the file it resolves to (TypeScript's
rules: extension and `/index` probing, and `./x.js` naming `./x.ts`), or in the
implementation beside it when that is a declaration (`x.js` beside `x.d.ts`);
a re-export is skipped.

With --packages (anchored_packages.tsv) the specifier names one of the repo's
own workspace packages (`from 'next/document'`, `from '@jest/globals'`: a
`packages/*/package.json` by its `name`), the import sits in a test or example
tree outside that package, and the truth is the package's one exported
top-level definition of the name outside its own tests and examples; a name it
exports more than once there, or not at all (a virtual or re-exported module),
is skipped. This is the other direction from the relative
set, whose truths sit beside the anchor: here the answer is library code
outside the anchor's tree.

Either way the name must be defined at least twice in the repo (otherwise
nothing to rank). Run once, by hand; the output is committed. Needs a release
build of rq. Deterministic for a given corpus and seed.

    script/recall/derive_imports.py > script/recall/anchored_imports.tsv
    script/recall/derive_imports.py --packages > script/recall/anchored_packages.tsv
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
# a declaration file's implementation beside it, the definition a reader wants
IMPLEMENTATION_OF = {".d.ts": [".js", ".jsx"], ".d.mts": [".mjs"], ".d.cts": [".cjs"]}
# rq's test and example directory names (src/search/score.rs)
SECONDARY_DIRS = {"test", "tests", "spec", "specs", "__tests__", "__mocks__", "testdata", "fixtures",
                  "example", "examples", "_examples", "demo", "demos", "docs", "dev-docs"}


def secondary(file):
    """Is `file` under a test or example directory, as rq reads one?"""
    return any(seg in SECONDARY_DIRS for seg in file.split("/")[:-1])


def workspace_packages(root):
    """Each `packages/*/package.json`'s name -> its directory, with a trailing `/`."""
    out = {}
    for manifest in sorted((root / "packages").glob("*/package.json")):
        try:
            name = json.loads(manifest.read_text()).get("name")
        except (OSError, ValueError):
            continue
        if name:
            out[name] = f"packages/{manifest.parent.name}/"
    return out


def package_of(specifier):
    """The package a bare specifier names: `next/document` -> `next`, `@jest/globals/x` -> `@jest/globals`."""
    parts = specifier.split("/")
    return "/".join(parts[:2]) if specifier.startswith("@") else parts[0]


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
    ap.add_argument("--packages", action="store_true",
                    help="imports of the repo's own workspace packages, from its test and example trees")
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
            count, top, exported = {}, {}, {}
            for name, file, line, parent, vis in con.execute(
                    "SELECT s.name, f.path, s.line, s.parent, s.visibility FROM symbols s "
                    "JOIN files f ON f.id = s.file_id"):
                count[name] = count.get(name, 0) + 1
                if parent is None:
                    key = (file, name)
                    top[key] = min(top.get(key, line), line)
                    if vis == "public":
                        exported[key] = min(exported.get(key, line), line)
            files = {f for (f,) in con.execute(
                "SELECT path FROM files WHERE language IN ('typescript', 'javascript')")}
            packages = workspace_packages(path) if args.packages else {}
            # a package's API is what it exports: a private `const path = require('path')`
            # is no answer to `import { path } from 'next/root-params'` (a virtual module)
            exported_by_name = {}
            for (f, n), line in sorted(exported.items()):
                exported_by_name.setdefault(n, []).append((f, line))

            def truth(file, specifier, name):
                if count.get(name, 0) < 2:
                    return None
                if not args.packages:
                    if not specifier.startswith("."):
                        return None
                    target = resolve(file, specifier, files)
                    if (target, name) not in top:
                        return None
                    # the implementation over its declaration, where both define it
                    decl = next((d for d in IMPLEMENTATION_OF if target.endswith(d)), None)
                    if decl:
                        stem = target[: -len(decl)]
                        target = next((stem + e for e in IMPLEMENTATION_OF[decl]
                                       if (stem + e, name) in top), target)
                    return f"{target}:{top[(target, name)]}"
                pkg = packages.get(package_of(specifier))
                if specifier.startswith(".") or not pkg or file.startswith(pkg) or not secondary(file):
                    return None
                defs = [f"{f}:{line}" for f, line in exported_by_name.get(name, [])
                        if f.startswith(pkg) and not secondary(f)]
                return defs[0] if len(defs) == 1 else None

            by_name = {}
            for file in sorted(files):
                for name, line, col, specifier in sites(path, file):
                    if t := truth(file, specifier, name):
                        by_name.setdefault(name, []).append((f"{file}:{line}:{col}", t))
            kept = 0
            for name in rng.sample(sorted(by_name), len(by_name)):
                take = min(SITES_PER_NAME, len(by_name[name]), SITES_PER_REPO - kept)
                for anchor, t in rng.sample(by_name[name], take):
                    print(f"{repo}\t{name}\t{anchor}\t{t}\t{'package' if args.packages else 'import'}")
                    kept += 1
            print(f"{repo}: {kept} import sites ({len(by_name)} names)", file=sys.stderr)


if __name__ == "__main__":
    main()
