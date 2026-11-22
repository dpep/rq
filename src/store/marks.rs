//! Holder marks: who is working on a checkout, as `meta` rows.

#[cfg(test)]
mod tests {
    use super::super::*;

    /// Above any pid the OS hands out.
    const DEAD: u32 = 4_000_000;

    /// Another live process: the test runner's parent.
    fn other() -> u32 {
        std::os::unix::process::parent_id()
    }

    fn stale() -> i64 {
        now_unix() - WARM_LOCK_TTL_SECS - 1
    }

    #[test]
    fn a_dead_pid_holds_nothing() {
        let mut store = Store::open_in_memory().unwrap();
        store.begin_pass("/r", DEAD, None).unwrap();
        store
            .meta_set("warm_lock:/r", &format!("{DEAD}:{}", now_unix()))
            .unwrap();
        assert!(store.other_indexers("/r").is_empty());
        assert!(!store.indexing_anywhere().unwrap());
    }

    #[test]
    fn a_stale_stamp_holds_nothing() {
        let store = Store::open_in_memory().unwrap();
        let p = other();
        store
            .meta_set(&format!("pass:/r:{p}"), &stale().to_string())
            .unwrap();
        store
            .meta_set("warm_lock:/r", &format!("{p}:{}", stale()))
            .unwrap();
        assert!(store.other_indexers("/r").is_empty());
        assert!(!store.indexing_anywhere().unwrap());
    }

    #[test]
    fn this_process_is_not_another_indexer() {
        let mut store = Store::open_in_memory().unwrap();
        store.begin_pass("/r", std::process::id(), None).unwrap();
        assert!(store.other_indexers("/r").is_empty());
        assert!(!store.indexing_anywhere().unwrap());
    }

    #[test]
    fn a_warm_lock_counts_as_a_holder() {
        let store = Store::open_in_memory().unwrap();
        let p = other();
        store
            .meta_set("warm_lock:/r", &format!("{p}:{}", now_unix()))
            .unwrap();
        assert_eq!(store.other_indexers("/r"), vec![p]);
        assert!(store.indexing_anywhere().unwrap());
    }

    #[test]
    fn a_warm_lock_and_a_pass_by_one_process_name_it_once() {
        let mut store = Store::open_in_memory().unwrap();
        let p = other();
        store.begin_pass("/r", p, None).unwrap();
        store
            .meta_set("warm_lock:/r", &format!("{p}:{}", now_unix()))
            .unwrap();
        assert_eq!(store.other_indexers("/r"), vec![p]);
    }

    #[test]
    fn a_mark_holds_only_its_own_root() {
        let mut store = Store::open_in_memory().unwrap();
        let p = other();
        store.begin_pass("/a/b", p, None).unwrap();
        for root in ["/a/bc", "/a", "/a/b/c", "/a/b:1"] {
            assert!(store.other_indexers(root).is_empty(), "{root}");
        }
        assert_eq!(store.other_indexers("/a/b"), vec![p]);
    }

    /// The cleanup's "anyone indexing anything" and a drop's "anyone indexing
    /// this" are one judgement, over the stored encodings.
    #[test]
    fn indexing_anywhere_agrees_with_other_indexers() {
        let (me, p, now) = (std::process::id(), other(), now_unix());
        let stale = stale();
        // each mark as `key=value`, the strings an rq of any version stores
        let cases: Vec<(&str, Vec<String>, bool)> = vec![
            ("empty", vec![], false),
            ("live pass", vec![format!("pass:/a:{p}={now}")], true),
            ("dead pass", vec![format!("pass:/a:{DEAD}={now}")], false),
            ("stale pass", vec![format!("pass:/a:{p}={stale}")], false),
            ("own pass", vec![format!("pass:/a:{me}={now}")], false),
            ("live lock", vec![format!("warm_lock:/a={p}:{now}")], true),
            (
                "dead lock",
                vec![format!("warm_lock:/a={DEAD}:{now}")],
                false,
            ),
            (
                "stale lock",
                vec![format!("warm_lock:/a={p}:{stale}")],
                false,
            ),
            ("own lock", vec![format!("warm_lock:/a={me}:{now}")], false),
            ("garbled lock", vec!["warm_lock:/a=x".into()], false),
            (
                "phase alone",
                vec![format!("phase:/a:{p}=reading:{now}")],
                false,
            ),
            ("span alone", vec!["span:/a=3".into()], false),
            ("colon in root", vec![format!("pass:/a:b:{p}={now}")], true),
            (
                "mixed, one live",
                vec![
                    format!("pass:/a:{DEAD}={now}"),
                    format!("pass:/b:{me}={now}"),
                    format!("warm_lock:/c={p}:{now}"),
                ],
                true,
            ),
        ];
        for (name, rows, expected) in cases {
            let store = Store::open_in_memory().unwrap();
            for row in &rows {
                let (k, v) = row.split_once('=').unwrap();
                store.meta_set(k, v).unwrap();
            }
            let roots = ["/a", "/a:b", "/b", "/c"];
            let others = roots.iter().any(|r| !store.other_indexers(r).is_empty());
            let anywhere = store.indexing_anywhere().unwrap();
            assert_eq!(others, expected, "{name}: other_indexers");
            assert_eq!(anywhere, expected, "{name}: indexing_anywhere");
        }
    }

    /// Pinned as it is, not as it should be: a multi-byte root's prefix is
    /// measured in bytes, SQLite's `substr` in characters.
    #[test]
    fn a_non_ascii_root() {
        let root = "/x/café";
        let mut store = Store::open_in_memory().unwrap();
        store.begin_pass(root, DEAD, None).unwrap();
        store.begin_pass(root, other(), None).unwrap();
        assert!(
            store.other_indexers(root).is_empty(),
            "a live pass goes unseen"
        );
        assert!(store.indexing_anywhere().unwrap(), "the cleanup sees it");
        assert!(
            store
                .meta_get(&format!("pass:{root}:{DEAD}"))
                .unwrap()
                .is_some(),
            "a dead pass's mark is never cleared"
        );
    }
}
