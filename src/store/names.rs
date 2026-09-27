//! The name index's storage (docs/NAME_INDEX.md): each repo's distinct symbol
//! names and its file paths, as signatures in append-order chunks, and the
//! recall scans over them.
//!
//! Appends ride the transaction that writes the symbols, so a reader sees a
//! name exactly when it sees the rows holding it. A name or file whose rows are
//! gone stays until the next rebuild; it verifies and then fetches nothing.

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use super::{CANDIDATE_COLS, CANDIDATE_FROM, Result, Store, SymbolRow, row_to_candidate};
use crate::search::{NAME_INDEX_FORMAT, PRIMARY_KINDS, Probe, SIG_BYTES, Signature, path_stem};

/// Keys per chunk. An append rewrites the last chunk, and a scan reads one row
/// per chunk.
const CHUNK: usize = 512;

/// Appends since the last rebuild that call for another, as a share of what
/// that rebuild wrote: a dead key costs a scan without ever fetching a row.
const COMPACT_SHARE: i64 = 4;
/// ...and never fewer than this, so a small repo isn't rebuilt every edit.
const COMPACT_MIN: i64 = 1000;

/// What a chunk's keys are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Keys {
    /// Distinct symbol names, each signed as itself.
    Names = 0,
    /// Repo-relative file paths, each signed by its stem: `score` lets a file
    /// named like the query surface its primary definitions.
    Files = 1,
}

impl Keys {
    fn sign(self, key: &str) -> Signature {
        match self {
            Keys::Names => Signature::of(key),
            Keys::Files => Signature::of(path_stem(key)),
        }
    }
}

/// A chunk's `keys`: each key's end offset (u32, little-endian), then the
/// keys' bytes. The offsets let a scan read only the keys that survive.
fn keys_blob(keys: &[&str]) -> Vec<u8> {
    let mut out = Vec::with_capacity(keys.len() * 4);
    let mut end = 0u32;
    for key in keys {
        end += key.len() as u32;
        out.extend_from_slice(&end.to_le_bytes());
    }
    for key in keys {
        out.extend_from_slice(key.as_bytes());
    }
    out
}

/// The `i`th of a chunk's `n` keys.
fn key(keys: &[u8], n: usize, i: usize) -> &str {
    let end = |j: usize| u32::from_le_bytes(keys[4 * j..4 * j + 4].try_into().unwrap()) as usize;
    let start = if i == 0 { 0 } else { end(i - 1) };
    std::str::from_utf8(&keys[4 * n + start..4 * n + end(i)]).unwrap_or("")
}

fn decode(keys: &[u8], n: usize) -> Vec<&str> {
    (0..n).map(|i| key(keys, n, i)).collect()
}

/// Is `repository_id`'s index current — built under this format, and kept
/// since?
pub(super) fn current(conn: &Connection, repository_id: i64) -> Result<bool> {
    let format: Option<i64> = conn
        .prepare_cached("SELECT format FROM name_index WHERE repository_id = ?1")?
        .query_row(params![repository_id], |r| r.get(0))
        .optional()?;
    Ok(format == Some(i64::from(NAME_INDEX_FORMAT)))
}

/// A repo with nothing indexed yet starts with an empty index, current from
/// the first name it's given.
pub(super) fn start(conn: &Connection, repository_id: i64) -> Result<()> {
    conn.prepare_cached(
        "INSERT OR IGNORE INTO name_index (repository_id, format, built) \
         SELECT ?1, ?2, 0 WHERE NOT EXISTS (SELECT 1 FROM files WHERE repository_id = ?1)",
    )?
    .execute(params![repository_id, NAME_INDEX_FORMAT])?;
    Ok(())
}

/// Does the repo already hold a symbol named exactly `name`? Every such name
/// is in a current index, so only the others are appended.
pub(super) fn known(tx: &Transaction, repository_id: i64, name: &str) -> Result<bool> {
    tx.prepare_cached(
        "SELECT 1 FROM symbols WHERE repository_id = ?1 AND name_lower = ?2 AND name = ?3 LIMIT 1",
    )?
    .query_row(
        params![repository_id, name.to_lowercase(), name],
        |_| Ok(()),
    )
    .optional()
    .map(|r| r.is_some())
}

/// Append `keys` to the repo's last chunk of their kind, starting new ones as
/// it fills.
pub(super) fn append(
    tx: &Transaction,
    repository_id: i64,
    kind: Keys,
    keys: &[String],
) -> Result<()> {
    let mut pending = keys.iter().map(String::as_str).peekable();
    if pending.peek().is_none() {
        return Ok(());
    }
    let last: Option<(i64, i64, Vec<u8>, Vec<u8>)> = tx
        .prepare_cached(
            "SELECT chunk, n, sigs, keys FROM name_sigs WHERE repository_id = ?1 AND kind = ?2 \
             ORDER BY chunk DESC LIMIT 1",
        )?
        .query_row(params![repository_id, kind as i64], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .optional()?;
    // the last chunk's keys keep the signatures they have
    let (mut chunk, mut sigs, mut held) = match &last {
        Some((chunk, n, sigs, keys)) if (*n as usize) < CHUNK => {
            (*chunk, sigs.clone(), decode(keys, *n as usize))
        }
        Some((chunk, ..)) => (chunk + 1, Vec::new(), Vec::new()),
        None => (0, Vec::new(), Vec::new()),
    };
    let mut insert = tx.prepare_cached(
        "INSERT OR REPLACE INTO name_sigs (repository_id, kind, chunk, n, sigs, keys) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?;
    loop {
        for key in pending.by_ref().take(CHUNK - held.len()) {
            sigs.extend_from_slice(&kind.sign(key).to_bytes());
            held.push(key);
        }
        let n = held.len() as i64;
        insert.execute(params![
            repository_id,
            kind as i64,
            chunk,
            n,
            sigs,
            keys_blob(&held)
        ])?;
        if pending.peek().is_none() {
            return Ok(());
        }
        chunk += 1;
        held.clear();
        sigs.clear();
    }
}

impl Store {
    /// Can recall read the name index for `repo` — or, unscoped, for every
    /// repo? Otherwise it runs the FTS nets.
    pub(crate) fn name_index_ready(&self, repo: Option<i64>) -> Result<bool> {
        match repo {
            Some(id) => current(&self.conn, id),
            None => self.conn.query_row(
                "SELECT NOT EXISTS (SELECT 1 FROM repositories r \
                   LEFT JOIN name_index n ON n.repository_id = r.id \
                   WHERE n.format IS NOT ?1)",
                params![i64::from(NAME_INDEX_FORMAT)],
                |r| r.get(0),
            ),
        }
    }

    /// Stop maintaining the repo's index until it's rebuilt: a cold pass
    /// writes too many names to check one by one, so it skips the appends, and
    /// recall falls back to FTS meanwhile.
    pub(crate) fn suspend_name_index(&self, repository_id: i64) -> Result<()> {
        self.conn.execute(
            "DELETE FROM name_index WHERE repository_id = ?1",
            params![repository_id],
        )?;
        Ok(())
    }

    /// Rebuild the repo's index when it's missing, stale, or carries enough
    /// keys since its last rebuild that some are likely dead. Returns whether
    /// it rebuilt.
    pub(crate) fn maintain_name_index(&self, repository_id: i64) -> Result<bool> {
        let state: Option<(i64, i64)> = self
            .conn
            .query_row(
                "SELECT format, built FROM name_index WHERE repository_id = ?1",
                params![repository_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let held: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(n), 0) FROM name_sigs WHERE repository_id = ?1",
            params![repository_id],
            |r| r.get(0),
        )?;
        let stale = state.is_none_or(|(format, built)| {
            format != i64::from(NAME_INDEX_FORMAT)
                || held - built > COMPACT_MIN.max(built / COMPACT_SHARE)
        });
        if stale {
            self.rebuild_name_index(repository_id)?;
        }
        Ok(stale)
    }

    /// Write the repo's index afresh from its symbols and files, in one
    /// transaction.
    pub(crate) fn rebuild_name_index(&self, repository_id: i64) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        tx.execute(
            "DELETE FROM name_sigs WHERE repository_id = ?1",
            params![repository_id],
        )?;
        let mut built = 0;
        for (kind, sql) in [
            (
                Keys::Names,
                "SELECT DISTINCT name FROM symbols WHERE repository_id = ?1",
            ),
            (
                Keys::Files,
                "SELECT path FROM files WHERE repository_id = ?1",
            ),
        ] {
            let keys: Vec<String> = tx
                .prepare(sql)?
                .query_map(params![repository_id], |r| r.get(0))?
                .collect::<Result<_>>()?;
            append(&tx, repository_id, kind, &keys)?;
            built += keys.len();
        }
        tx.execute(
            "INSERT OR REPLACE INTO name_index (repository_id, format, built) VALUES (?1, ?2, ?3)",
            params![repository_id, NAME_INDEX_FORMAT, built as i64],
        )?;
        tx.commit()
    }

    /// Every key of `kind` in `repo` (or every repo) that `accepts` takes,
    /// with its repository, screening each signature with `screen` first.
    fn scan(
        &self,
        repo: Option<i64>,
        kind: Keys,
        screen: impl Fn(&Signature) -> bool,
        accepts: impl Fn(&str, &Signature) -> bool,
    ) -> Result<Vec<(i64, String)>> {
        let sql = match repo {
            Some(_) => {
                "SELECT repository_id, n, sigs, keys FROM name_sigs \
                 WHERE repository_id = ?2 AND kind = ?1 ORDER BY chunk"
            }
            None => {
                "SELECT repository_id, n, sigs, keys FROM name_sigs \
                 WHERE kind = ?1 ORDER BY repository_id, chunk"
            }
        };
        let mut stmt = self.conn.prepare_cached(sql)?;
        let mut rows = match repo {
            Some(id) => stmt.query(params![kind as i64, id])?,
            None => stmt.query(params![kind as i64])?,
        };
        let mut out = Vec::new();
        let mut survivors = Vec::new();
        while let Some(row) = rows.next()? {
            survivors.clear();
            let sigs = row.get_ref(2)?.as_blob()?;
            for (i, sig) in sigs.as_chunks::<SIG_BYTES>().0.iter().enumerate() {
                let sig = Signature::from_bytes(sig);
                if screen(&sig) {
                    survivors.push((i, sig));
                }
            }
            if survivors.is_empty() {
                continue; // the keys are never read
            }
            let (repository_id, n): (i64, i64) = (row.get(0)?, row.get(1)?);
            let keys = row.get_ref(3)?.as_blob()?;
            for (i, sig) in &survivors {
                let key = key(keys, n as usize, *i);
                if accepts(key, sig) {
                    out.push((repository_id, key.to_string()));
                }
            }
        }
        Ok(out)
    }

    /// The rows of every name `probe` accepts, fetched by
    /// `(repository_id, name_lower)`. The cap still bounds them: when the names
    /// hold more rows than `limit`, the best names' rows are the ones kept.
    pub(super) fn named_candidates(
        &self,
        repo: Option<i64>,
        probe: &Probe,
        limit: usize,
    ) -> Result<Vec<(i64, SymbolRow)>> {
        let names = self.scan(
            repo,
            Keys::Names,
            |sig| probe.screen(sig),
            |name, sig| probe.accepts(name, sig),
        )?;
        // case variants share their rows
        let mut keys: Vec<(i64, String, String)> = names
            .into_iter()
            .map(|(r, name)| (r, name.to_lowercase(), name))
            .collect();
        keys.sort_unstable();
        keys.dedup_by(|a, b| (a.0, &a.1) == (b.0, &b.1));
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT {CANDIDATE_COLS} {CANDIDATE_FROM} \
             WHERE s.repository_id = ?1 AND s.name_lower = ?2"
        ))?;
        let mut fetch = |keys: &[(i64, String, String)]| -> Result<(Vec<(i64, SymbolRow)>, bool)> {
            let mut rows = Vec::new();
            for (r, lower, _) in keys {
                if rows.len() >= limit {
                    return Ok((rows, true));
                }
                for row in stmt.query_map(params![r, lower], row_to_candidate)? {
                    rows.push(row?);
                }
            }
            Ok((rows, false))
        };
        let (mut rows, over) = fetch(&keys)?;
        if over {
            // best first, rather than in whatever order the names were met
            let mut ranked: Vec<(f64, (i64, String, String))> =
                keys.into_iter().map(|k| (probe.rank(&k.2), k)).collect();
            ranked.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
            let keys: Vec<_> = ranked.into_iter().map(|(_, k)| k).collect();
            rows = fetch(&keys)?.0;
        }
        rows.truncate(limit);
        Ok(rows)
    }

    /// The primary definitions of every file whose stem `probe` accepts: what a
    /// path match alone lets `score` surface.
    pub(super) fn filed_candidates(
        &self,
        repo: Option<i64>,
        probe: &Probe,
        limit: usize,
    ) -> Result<Vec<(i64, SymbolRow)>> {
        let files = self.scan(
            repo,
            Keys::Files,
            |sig| probe.screen_stem(sig),
            |path, _| probe.accepts_stem(path_stem(path)),
        )?;
        let kinds = PRIMARY_KINDS.map(|k| format!("'{k}'")).join(", ");
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT {CANDIDATE_COLS} {CANDIDATE_FROM} \
             WHERE fi.repository_id = ?1 AND fi.path = ?2 AND s.kind IN ({kinds})"
        ))?;
        let mut rows = Vec::new();
        for (r, path) in &files {
            if rows.len() >= limit {
                break;
            }
            for row in stmt.query_map(params![r, path], row_to_candidate)? {
                rows.push(row?);
            }
        }
        rows.truncate(limit);
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{Kind, RepoIdentity, Symbol};

    #[test]
    fn a_chunk_round_trips_its_keys() {
        let keys = ["Widget", "", "naïve_café", "parse_file"];
        assert_eq!(decode(&keys_blob(&keys), keys.len()), keys);
    }

    fn sym(name: &str, kind: Kind) -> Symbol {
        Symbol {
            name: name.into(),
            kind,
            language: "rust".into(),
            file: String::new(),
            line: 1,
            end_line: 1,
            parent: None,
            visibility: None,
        }
    }

    fn repo(store: &Store, path: &str) -> i64 {
        store
            .upsert_repository(&RepoIdentity::local(path), None)
            .unwrap()
    }

    fn write(store: &mut Store, repo: i64, file: &str, names: &[&str]) {
        let syms: Vec<Symbol> = names.iter().map(|n| sym(n, Kind::Function)).collect();
        // a fresh hash each time, so every call is a rewrite
        let hash = format!("{file}{names:?}");
        store
            .replace_file_symbols(repo, file, "rust", None, &hash, &syms)
            .unwrap();
    }

    /// The names fuzzy recall hands on for `query`, from the index alone.
    fn recalled(store: &Store, repo: Option<i64>, query: &str) -> Vec<String> {
        let mut names: Vec<String> = store
            .named_candidates(repo, &Probe::new(query), 1000)
            .unwrap()
            .into_iter()
            .map(|(_, c)| c.name)
            .collect();
        names.sort();
        names
    }

    /// Every key the repo's index holds, of `kind`, in order.
    fn held(store: &Store, repo: i64, kind: Keys) -> Vec<String> {
        store
            .scan(Some(repo), kind, |_| true, |_, _| true)
            .unwrap()
            .into_iter()
            .map(|(_, k)| k)
            .collect()
    }

    #[test]
    fn a_name_is_recalled_as_soon_as_its_file_is_written() {
        let mut store = Store::open_in_memory().unwrap();
        let r = repo(&store, "/tmp/a");
        assert!(
            store.name_index_ready(Some(r)).unwrap(),
            "a new repo starts current"
        );
        write(&mut store, r, "a.rs", &["WidgetFactory"]);
        assert_eq!(recalled(&store, Some(r), "wdgfac"), ["WidgetFactory"]);
        write(&mut store, r, "b.rs", &["GadgetFactory"]);
        assert_eq!(recalled(&store, Some(r), "gdgfac"), ["GadgetFactory"]);
        assert_eq!(held(&store, r, Keys::Files), ["a.rs", "b.rs"]);
    }

    #[test]
    fn a_rewrite_adds_only_names_the_repo_lacked() {
        let mut store = Store::open_in_memory().unwrap();
        let r = repo(&store, "/tmp/a");
        write(&mut store, r, "a.rs", &["Widget", "Widget", "widget"]);
        write(&mut store, r, "a.rs", &["Widget", "Gadget"]);
        write(&mut store, r, "b.rs", &["Gadget"]);
        // case variants are names of their own: `align` reads the case
        assert_eq!(held(&store, r, Keys::Names), ["Widget", "widget", "Gadget"]);
        assert_eq!(held(&store, r, Keys::Files), ["a.rs", "b.rs"]);
    }

    #[test]
    fn a_name_whose_rows_are_gone_fetches_nothing_until_a_rebuild_drops_it() {
        let mut store = Store::open_in_memory().unwrap();
        let r = repo(&store, "/tmp/a");
        write(&mut store, r, "a.rs", &["WidgetFactory"]);
        write(&mut store, r, "a.rs", &["GadgetFactory"]);
        assert!(recalled(&store, Some(r), "wdgfac").is_empty());
        assert!(held(&store, r, Keys::Names).contains(&"WidgetFactory".to_string()));
        store.rebuild_name_index(r).unwrap();
        assert_eq!(held(&store, r, Keys::Names), ["GadgetFactory"]);
    }

    #[test]
    fn a_suspended_index_is_not_read_and_a_rebuild_catches_it_up() {
        let mut store = Store::open_in_memory().unwrap();
        let r = repo(&store, "/tmp/a");
        write(&mut store, r, "a.rs", &["WidgetFactory"]);
        store.suspend_name_index(r).unwrap();
        assert!(!store.name_index_ready(Some(r)).unwrap());
        assert!(
            !store.name_index_ready(None).unwrap(),
            "unscoped needs every repo"
        );
        write(&mut store, r, "b.rs", &["GadgetFactory"]);
        assert!(
            store.maintain_name_index(r).unwrap(),
            "a missing index is rebuilt"
        );
        assert!(store.name_index_ready(Some(r)).unwrap());
        assert_eq!(recalled(&store, Some(r), "gdgfac"), ["GadgetFactory"]);
    }

    #[test]
    fn an_index_from_another_format_is_rebuilt_not_read() {
        let mut store = Store::open_in_memory().unwrap();
        let r = repo(&store, "/tmp/a");
        write(&mut store, r, "a.rs", &["WidgetFactory"]);
        store
            .conn
            .execute("UPDATE name_index SET format = 0", [])
            .unwrap();
        assert!(!store.name_index_ready(Some(r)).unwrap());
        assert!(store.maintain_name_index(r).unwrap());
        assert!(store.name_index_ready(Some(r)).unwrap());
        assert!(!store.maintain_name_index(r).unwrap(), "then left alone");
    }

    #[test]
    fn enough_appends_since_a_rebuild_compact_the_index() {
        let mut store = Store::open_in_memory().unwrap();
        let r = repo(&store, "/tmp/a");
        let names: Vec<String> = (0..=COMPACT_MIN).map(|i| format!("name{i}")).collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        write(&mut store, r, "a.rs", &names[..10]);
        assert!(!store.maintain_name_index(r).unwrap());
        write(&mut store, r, "a.rs", &names);
        assert!(store.maintain_name_index(r).unwrap());
        assert!(!store.maintain_name_index(r).unwrap());
    }

    #[test]
    fn names_past_a_chunk_append_and_scan_across_chunks() {
        let mut store = Store::open_in_memory().unwrap();
        let r = repo(&store, "/tmp/a");
        let first: Vec<String> = (0..CHUNK - 3).map(|i| format!("alpha_{i}")).collect();
        let first: Vec<&str> = first.iter().map(String::as_str).collect();
        write(&mut store, r, "a.rs", &first);
        let second: Vec<String> = (0..CHUNK + 10).map(|i| format!("beta_{i}")).collect();
        let second: Vec<&str> = second.iter().map(String::as_str).collect();
        write(&mut store, r, "b.rs", &second);
        write(&mut store, r, "c.rs", &["WidgetFactory"]);
        let held = held(&store, r, Keys::Names);
        assert_eq!(held.len(), first.len() + second.len() + 1);
        assert_eq!(held.last().map(String::as_str), Some("WidgetFactory"));
        assert_eq!(recalled(&store, Some(r), "wdgfac"), ["WidgetFactory"]);
        let chunks: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM name_sigs WHERE kind = 0", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(chunks, 3);
    }

    #[test]
    fn unscoped_recall_reads_every_repo_and_dropping_one_forgets_its_index() {
        let mut store = Store::open_in_memory().unwrap();
        let (a, b) = (repo(&store, "/tmp/a"), repo(&store, "/tmp/b"));
        write(&mut store, a, "a.rs", &["WidgetFactory"]);
        write(&mut store, b, "b.rs", &["WidgetFacade"]);
        assert_eq!(
            recalled(&store, None, "wdgfac"),
            ["WidgetFacade", "WidgetFactory"]
        );
        assert_eq!(recalled(&store, Some(b), "wdgfac"), ["WidgetFacade"]);
        store.drop_repository(b).unwrap();
        let left: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM name_sigs WHERE repository_id = ?1",
                [b],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(left, 0);
    }

    #[test]
    fn a_capped_recall_keeps_the_best_matched_names() {
        let mut store = Store::open_in_memory().unwrap();
        let r = repo(&store, "/tmp/a");
        write(
            &mut store,
            r,
            "a.rs",
            &["w_x_i_x_d", "WidgetDetail", "WideIndexDriver", "Widget"],
        );
        let kept: Vec<String> = store
            .named_candidates(Some(r), &Probe::new("wid"), 2)
            .unwrap()
            .into_iter()
            .map(|(_, c)| c.name)
            .collect();
        assert_eq!(kept.len(), 2);
        assert!(kept.contains(&"Widget".to_string()), "{kept:?}");
        assert!(!kept.contains(&"w_x_i_x_d".to_string()), "{kept:?}");
    }

    #[test]
    fn a_file_named_like_the_query_recalls_its_primary_definitions() {
        let mut store = Store::open_in_memory().unwrap();
        let r = repo(&store, "/tmp/a");
        let syms = [sym("Base", Kind::Module), sym("helper", Kind::Function)];
        store
            .replace_file_symbols(r, "lib/connection_pool.rb", "ruby", None, "h", &syms)
            .unwrap();
        let rows = store
            .filed_candidates(Some(r), &Probe::new("conpool"), 100)
            .unwrap();
        let names: Vec<&str> = rows.iter().map(|(_, c)| c.name.as_str()).collect();
        assert_eq!(names, ["Base"]);
    }
}
