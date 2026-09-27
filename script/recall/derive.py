#!/usr/bin/env python3
"""Derive recall queries for a corpus. See docs/RECALL.md.

    derive.py sample  REPO DIR --n 200                       # names sampled from DIR
    derive.py touched REPO DIR --git GITDIR --commits 300    # names real work touched

Both print `queries.tsv` rows (repo, query, type, source) on stdout. Rust only:
ground-truth names are read from DIR's source with a regex, not from rq, so a
definition rq fails to extract is still asked for and shows up as a miss. An rq
index of DIR (the rq on PATH, or $RQ_BIN) supplies owners for `Owner::name` and
the names a fuzzy query must not be a prefix of.
"""
import argparse
import os
import random
import re
import sqlite3
import subprocess
import sys
import tempfile
from collections import Counter

FUZZY = ["abbr3", "abbr2", "first+last", "consonants", "typo", "glob"]
VOWELS = set("aeiou")

# An item definition on an added line of a diff, or in a hunk header's context.
DEF = re.compile(
    r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:(?:async|unsafe|const|default|extern\s+\"[^\"]*\")\s+)*"
    r"(?:fn|struct|enum|trait|type|mod|union|const|static|macro_rules!)\s+([A-Za-z_][A-Za-z0-9_]*)"
)


def words(name):
    """`test_float_limits` -> [test, float, limits]; `LateAttachmentsProxy` ->
    [late, attachments, proxy]; `HTTPServer` -> [http, server]."""
    out = []
    for part in re.split(r"[^A-Za-z0-9]+", name):
        out += re.findall(r"[A-Z]+(?=[A-Z][a-z]|\d|\b)|[A-Z]?[a-z]+|[A-Z]+|\d+", part)
    return [w.lower() for w in out if w]


def typo(name, rng):
    spots = [i for i in range(len(name) - 1) if name[i] != name[i + 1]]
    if not spots:
        return None
    i = rng.choice(spots)
    return name[:i] + name[i + 1] + name[i] + name[i + 2:]


def fuzzy(name, rng):
    """One query per fuzzy type, per the table in docs/RECALL.md."""
    w = words(name)
    if not w:
        return {}
    letters = "".join(w)
    out = {
        "abbr3": "".join(x[:3] for x in w),
        "abbr2": "".join(x[:2] for x in w),
        "consonants": (letters[0] + "".join(c for c in letters[1:] if c not in VOWELS and not c.isdigit()))[:6],
        "typo": typo(name, rng),
        "glob": f"{w[0][:3]}*{w[-1][:3]}" if len(w) > 1 else None,
    }
    if len(w) > 1:
        out["first+last"] = w[0][:3] + w[-1]
    return {k: v for k, v in out.items() if v}


def case_variant(name):
    """The other convention: snake -> camelCase, Camel -> snake."""
    w = words(name)
    if len(w) < 2:
        return None
    if "_" in name:
        return w[0] + "".join(x.capitalize() for x in w[1:])
    return "_".join(w)


def index(dir_):
    """(name, parent, kind, path, in_test) for every symbol rq extracts from DIR."""
    rq = os.environ.get("RQ_BIN", "rq")
    with tempfile.TemporaryDirectory(prefix="rq-derive-") as tmp:
        db = os.path.join(tmp, "rq.db")
        env = dict(os.environ, RQ_DB=db, RQ_WARM_DETACH="0", RQ_JOBS="1")
        subprocess.run([rq, "--index", dir_], env=env, check=True, stdout=subprocess.DEVNULL)
        con = sqlite3.connect(db)
        return con.execute(
            "SELECT s.name, s.parent, s.kind, f.path FROM symbols s JOIN files f ON f.id = s.file_id"
        ).fetchall()


def is_test(parent, path):
    segs = path.split("/")
    return (parent or "").split("::")[-1] == "tests" or any(
        s in ("tests", "test", "benches", "tests-integration", "tests-build") for s in segs[:-1])


def emit(repo, rows):
    for q, t, src in rows:
        print(f"{repo}\t{q}\t{t}\t{src}")


def fuzzy_rows(name, lowers, rng):
    rows = []
    for t, q in fuzzy(name, rng).items():
        # every non-glob query must reach the fuzzy layers: no name starts with it
        if t != "glob" and any(n.startswith(q.lower()) for n in lowers):
            continue
        rows.append((q, t, name))
    return rows


def touched(gitdir, commits, rev):
    log = subprocess.run(["git", "-C", gitdir, "log", "-p", f"-{commits}", "--format=", rev, "--", "*.rs"],
                         capture_output=True, text=True, check=True).stdout
    names = Counter()
    for line in log.splitlines():
        if line.startswith("@@"):
            text = line.split("@@", 2)[-1]
        elif line.startswith("+") and not line.startswith("+++"):
            text = line[1:]
        else:
            continue
        m = DEF.match(text)
        if m:
            names[m.group(1)] += 1
    return names


def defined(dir_):
    """Every item name defined in DIR's .rs files, read from source rather than
    rq, so a name rq fails to extract still counts. Maps name -> {path: in_test};
    a definition after a file's first `#[cfg(test)]` is test code (in-file test
    modules sit at the bottom by convention)."""
    out = {}
    for root, dirs, files in os.walk(dir_):
        dirs[:] = [d for d in dirs if d not in (".git", "target")]
        for f in files:
            if not f.endswith(".rs"):
                continue
            p = os.path.join(root, f)
            rel = os.path.relpath(p, dir_)
            test = is_test(None, rel)
            for line in open(p, errors="replace"):
                test = test or line.strip().startswith("#[cfg(test)]")
                m = DEF.match(line)
                if m:
                    seen = out.setdefault(m.group(1), {})
                    seen[rel] = seen.get(rel, True) and test
    return out


def name_rows(name, syms, lowers, rng):
    """Every query a developer might type for `name`: the name itself, the other
    case convention, the fuzzy types, and `Owner::name` when it has an owner."""
    rows = [(name, "exact", name)]
    if (v := case_variant(name)) and v != name:
        rows.append((v, "case", name))
    if sum(c.isalpha() for c in name) >= 5:
        rows += fuzzy_rows(name, lowers, rng)
    owners = sorted({s[1].split("::")[-1] for s in syms
                     if s[0] == name and s[1] and s[2] in ("method", "function") and not is_test(s[1], s[3])})
    if owners:
        rows.append((f"{rng.choice(owners)}::{name}", "qualified", name))
    return rows


def derive(a, names):
    syms = index(a.dir)
    lowers = {s[0].lower() for s in syms}
    defs = defined(a.dir)
    rng = random.Random(a.seed)
    # a name defined only in tests or examples is rarely what anyone navigates to
    pool = sorted(n for n in names if n in defs and len(n) >= 3 and not all(defs[n].values())
                  and not all(any(seg in ("examples", "benches", "fuzz") for seg in p.split("/")) for p in defs[n]))
    rows = []
    for name in rng.sample(pool, min(a.n, len(pool))) if a.n else pool:
        rows += name_rows(name, syms, lowers, rng)
    emit(a.repo, rows)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    s = sub.add_parser("sample", help="names sampled at random from DIR's source")
    t = sub.add_parser("touched", help="names the last COMMITS commits of --git added or changed")
    t.add_argument("--git", required=True, help="the live repo whose history says what was touched")
    t.add_argument("--commits", type=int, default=300)
    t.add_argument("--rev", default="HEAD", help="walk history back from here (the corpus pin)")
    for p in (s, t):
        p.add_argument("repo")
        p.add_argument("dir")
        p.add_argument("--n", type=int, default=0, help="sample this many names (default: all)")
        p.add_argument("--seed", type=int, default=1)
    a = ap.parse_args()
    names = touched(a.git, a.commits, a.rev) if a.cmd == "touched" else defined(a.dir)
    derive(a, names)


if __name__ == "__main__":
    sys.exit(main())
