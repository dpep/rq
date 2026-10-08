//! One invariant over every way rq can be asked: once a tree is indexed, a
//! query for nothing settles to a miss (exit 1) — and nothing hangs.
//!
//! "Is this file in the index?" is answered by several code paths: the git
//! enumeration, the filesystem walk, the moved-detectors, each entrypoint's
//! own staleness check. A rule applied in one of them and not another reads
//! as a tree that is forever "still warming" (exit 2), or a pass that never
//! ends. Each row is a file that has split them before; each column a state
//! the tree can be in, crossed with every entrypoint. A new hostile file is
//! one row, a new entrypoint one line in `ENTRYPOINTS`.

mod common;

use std::fs;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::Path;
use std::time::Duration;

use common::{Fifo, Scratch, git_cmd, rq_bounded};

const MISS: &str = "NoSuchSymbolAnywhere";

/// How each entrypoint asks: args, and the stdin a batch reads.
const ENTRYPOINTS: [(&str, &[&str], Option<&str>); 4] = [
    ("search", &[MISS, "--json"], None),
    ("--no-wait", &[MISS, "--json", "--no-wait"], None),
    ("batch", &["-J"], Some("NoSuchSymbolAnywhere\n")),
    (
        "batch --no-wait",
        &["-J", "--no-wait"],
        Some("NoSuchSymbolAnywhere\n"),
    ),
];

/// A hostile file, planted in three steps: before the tree is committed (so
/// git tracks it), after (before the index reads it), and after the index.
/// `cleanup` undoes what would stop the scratch dir's removal.
struct Row {
    name: &'static str,
    plant: fn(&Path),
    mangle: fn(&Path),
    after_index: fn(&Path),
    cleanup: fn(&Path),
}

const ROW: Row = Row {
    name: "",
    plant: |_| {},
    mangle: |_| {},
    after_index: |_| {},
    cleanup: |_| {},
};

fn rows() -> Vec<Row> {
    let mut rows = vec![
        Row {
            name: "plain",
            ..ROW
        },
        Row {
            name: "latin-1",
            plant: |d| fs::write(d.join("latin.rb"), b"# \xe9\nclass Gadget\nend\n").unwrap(),
            ..ROW
        },
        Row {
            name: "binary .ts",
            plant: |d| fs::write(d.join("clip.ts"), b"G\0\x11\0\nexport class Clip {}\n").unwrap(),
            ..ROW
        },
        Row {
            name: "unreadable",
            plant: |d| fs::write(d.join("locked.rb"), "class Locked\nend\n").unwrap(),
            mangle: |d| chmod(&d.join("locked.rb"), 0o000),
            cleanup: |d| chmod(&d.join("locked.rb"), 0o644),
            ..ROW
        },
        Row {
            name: "unreadable dir, after the index",
            plant: |d| {
                fs::create_dir(d.join("sub")).unwrap();
                fs::write(d.join("sub/b.rb"), "class Hidden\nend\n").unwrap();
            },
            after_index: |d| chmod(&d.join("sub"), 0o000),
            cleanup: |d| chmod(&d.join("sub"), 0o755),
            ..ROW
        },
        Row {
            name: ".ignore'd",
            plant: |d| {
                fs::write(d.join(".ignore"), "skip.rb\n").unwrap();
                fs::write(d.join("skip.rb"), "class Skipped\nend\n").unwrap();
            },
            ..ROW
        },
        Row {
            name: ".ignore'd dir",
            plant: |d| {
                fs::write(d.join(".ignore"), "vendor/\n").unwrap();
                fs::create_dir(d.join("vendor")).unwrap();
                fs::write(d.join("vendor/v.rb"), "class Vendored\nend\n").unwrap();
            },
            ..ROW
        },
        Row {
            name: "tracked, then .gitignore'd",
            plant: |d| fs::write(d.join("gen.rb"), "class Generated\nend\n").unwrap(),
            mangle: |d| fs::write(d.join(".gitignore"), "gen.rb\n").unwrap(),
            ..ROW
        },
        Row {
            name: "tracked, then .gitignore'd and edited",
            plant: |d| fs::write(d.join("gen.rb"), "class Generated\nend\n").unwrap(),
            mangle: |d| {
                fs::write(d.join(".gitignore"), "gen.rb\n").unwrap();
                fs::write(d.join("gen.rb"), "class Regenerated\nend\n").unwrap();
            },
            ..ROW
        },
        Row {
            name: "tracked, then excluded",
            plant: |d| fs::write(d.join("excl.rb"), "class Excluded\nend\n").unwrap(),
            mangle: |d| {
                if d.join(".git").is_dir() {
                    fs::write(d.join(".git/info/exclude"), "excl.rb\n").unwrap();
                }
            },
            ..ROW
        },
        Row {
            name: "hidden dir",
            plant: |d| {
                fs::create_dir(d.join(".tools")).unwrap();
                fs::write(d.join(".tools/t.rb"), "class Tool\nend\n").unwrap();
            },
            ..ROW
        },
        Row {
            name: "submodule",
            plant: |d| {
                let sub = d.join("lib");
                fs::create_dir(&sub).unwrap();
                fs::write(sub.join("s.rb"), "class Sub\nend\n").unwrap();
                git(&sub, &["init", "-q"]);
                commit(&sub);
                fs::write(
                    d.join(".gitmodules"),
                    "[submodule \"lib\"]\n\tpath = lib\n\turl = ./lib\n",
                )
                .unwrap();
            },
            // a submodule is active once its url is configured; before a
            // first commit there's nothing to init
            mangle: |d| {
                let _ = git_cmd(d).args(["submodule", "init"]).output();
            },
            ..ROW
        },
        Row {
            name: "symlink",
            plant: |d| std::os::unix::fs::symlink("a.rb", d.join("link.rb")).unwrap(),
            ..ROW
        },
        Row {
            name: "/dev/zero symlink",
            plant: |d| std::os::unix::fs::symlink("/dev/zero", d.join("zero.rb")).unwrap(),
            ..ROW
        },
        Row {
            name: "fifo",
            plant: |d| fs::write(d.join("pipe.rb"), "class Pipe\nend\n").unwrap(),
            mangle: |d| {
                fs::remove_file(d.join("pipe.rb")).unwrap();
                common::mkfifo(&d.join("pipe.rb"));
            },
            ..ROW
        },
        Row {
            name: "deleted without git rm",
            plant: |d| fs::write(d.join("doomed.rb"), "class Doomed\nend\n").unwrap(),
            after_index: |d| fs::remove_file(d.join("doomed.rb")).unwrap(),
            ..ROW
        },
    ];
    if cfg!(target_os = "linux") {
        // APFS refuses a name that isn't UTF-8
        rows.push(Row {
            name: "non-UTF-8 name",
            plant: |d| {
                use std::os::unix::ffi::OsStrExt;
                let name = std::ffi::OsStr::from_bytes(b"caf\xe9.rb");
                fs::write(d.join(name), "class Cafe\nend\n").unwrap();
            },
            ..ROW
        });
    }
    rows
}

/// The states a tree can be indexed in.
#[derive(Clone, Copy, Debug)]
enum State {
    GitMain,
    GitFeature,
    GitUnborn,
    NonGit,
}

impl State {
    fn setup(self, dir: &Path) {
        match self {
            State::GitMain => {
                git(dir, &["init", "-q", "-b", "main"]);
                commit(dir);
            }
            State::GitFeature => {
                git(dir, &["init", "-q", "-b", "main"]);
                commit(dir);
                git(dir, &["checkout", "-qb", "feature"]);
                fs::write(dir.join("feat.rb"), "class Feat\nend\n").unwrap();
                commit(dir);
            }
            State::GitUnborn => git(dir, &["init", "-q"]),
            State::NonGit => {}
        }
    }
}

fn git(dir: &Path, args: &[&str]) {
    let out = git_cmd(dir)
        .args(["-c", "user.email=t@e.st", "-c", "user.name=test"])
        .args(args)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn commit(dir: &Path) {
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-qm", "c"]);
}

fn chmod(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

/// Long enough for a loaded machine, short of hiding a hang.
const LIMIT: Duration = Duration::from_secs(30);

/// Every row in `state`, through every entrypoint: the failures, one line
/// each, so a red run names every cell at once.
fn failures(state: State) -> Vec<String> {
    let mut failed = Vec::new();
    for row in rows() {
        let dir = Scratch::new(&format!("hostile-{state:?}"));
        let db = dir.db();
        fs::write(dir.join("a.rb"), "class Widget\nend\n").unwrap();
        (row.plant)(&dir);
        state.setup(&dir);
        (row.mangle)(&dir);
        let fifo = dir.join("pipe.rb");
        let _release = fs::symlink_metadata(&fifo)
            .is_ok_and(|m| m.file_type().is_fifo())
            .then(|| Fifo(fifo));
        let mut cell = |what: &str, outcome: Result<(), String>| {
            if let Err(e) = outcome {
                failed.push(format!("{state:?} × {} × {what}: {e}", row.name));
            }
        };

        let index = rq_bounded(&db, &dir, &["--index"], &[], None, LIMIT);
        cell("--index", index.map(|_| ()).ok_or_else(|| "hung".into()));
        (row.after_index)(&dir);
        for (entry, args, stdin) in ENTRYPOINTS {
            cell(entry, settles_to_a_miss(&db, &dir, args, stdin));
        }
        let symbols = rq_bounded(
            &db,
            &dir,
            &["--symbols", "a.rb", "--json"],
            &[],
            None,
            LIMIT,
        );
        cell(
            "--symbols",
            match symbols {
                Some((0, _)) => Ok(()),
                Some((code, out)) => Err(format!("exit {code}: {out}")),
                None => Err("hung".into()),
            },
        );
        (row.cleanup)(&dir);
    }
    failed
}

/// The files the index holds, by path.
fn indexed(db: &Path) -> Vec<String> {
    let conn = rusqlite::Connection::open(db).unwrap();
    let mut stmt = conn
        .prepare("SELECT path FROM checkout_files ORDER BY path")
        .unwrap();
    stmt.query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

/// Every row in `state`: the files an explicit index holds against those a
/// cold warm holds once settled, and an index after that warm.
fn set_failures(state: State) -> Vec<String> {
    let mut failed = Vec::new();
    for row in rows() {
        let dir = Scratch::new(&format!("same-set-{state:?}"));
        fs::write(dir.join("a.rb"), "class Widget\nend\n").unwrap();
        (row.plant)(&dir);
        state.setup(&dir);
        (row.mangle)(&dir);
        let fifo = dir.join("pipe.rb");
        let _release = fs::symlink_metadata(&fifo)
            .is_ok_and(|m| m.file_type().is_fifo())
            .then(|| Fifo(fifo));
        let run = |db: &Path, args: &[&str]| {
            rq_bounded(db, &dir, args, &[("RQ_WARM_DETACH", "0")], None, LIMIT).map(|_| indexed(db))
        };
        let warming = dir.db();
        let indexing = std::path::PathBuf::from(format!("{}-index.db", dir.display()));
        let index = run(&indexing, &["--index"]);
        let warm = run(&warming, &["--warm"]);
        let warm_then_index = run(&warming, &["--index"]);
        for suffix in ["", "-wal", "-shm"] {
            let _ = fs::remove_file(format!("{}{suffix}", indexing.display()));
        }
        match (index, warm, warm_then_index) {
            (Some(index), Some(warm), Some(after)) => {
                if index != warm || index != after {
                    failed.push(format!(
                        "{state:?} × {}: --index {index:?}, warm {warm:?}, warm then --index {after:?}",
                        row.name
                    ));
                }
            }
            _ => failed.push(format!("{state:?} × {}: hung", row.name)),
        }
        (row.cleanup)(&dir);
    }
    failed
}

/// Exit 1, allowing a 2 first where a warm was genuinely pending.
fn settles_to_a_miss(
    db: &Path,
    dir: &Path,
    args: &[&str],
    stdin: Option<&str>,
) -> Result<(), String> {
    let mut seen = Vec::new();
    for _ in 0..2 {
        match rq_bounded(db, dir, args, &[], stdin, LIMIT) {
            None => return Err("hung".into()),
            Some((1, _)) => return Ok(()),
            Some((code, out)) => seen.push(format!("exit {code}: {}", out.trim())),
        }
    }
    Err(seen.join(" then "))
}

fn assert_settles(state: State) {
    let failed = failures(state);
    assert!(failed.is_empty(), "\n{}", failed.join("\n"));
}

#[test]
fn on_main() {
    assert_settles(State::GitMain);
}

#[test]
fn on_a_feature_branch() {
    assert_settles(State::GitFeature);
}

#[test]
fn before_a_first_commit() {
    assert_settles(State::GitUnborn);
}

#[test]
fn outside_git() {
    assert_settles(State::NonGit);
}

/// A warm lists files with git; an explicit index walks the disk. Whatever
/// each finds, both hold the same files: the walk decides (D62). Outside git
/// nothing warms.
#[test]
fn a_warm_and_an_index_hold_the_same_files() {
    let failed: Vec<String> = [State::GitMain, State::GitFeature, State::GitUnborn]
        .into_iter()
        .flat_map(set_failures)
        .collect();
    assert!(failed.is_empty(), "\n{}", failed.join("\n"));
}
