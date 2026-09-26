//! Fuzzy ranking on concrete cases: which reading of a mistyped or abbreviated
//! query wins when several could.

use crate::core::{Kind, RepoIdentity, Symbol};
use crate::search::{self, ActiveFiles};
use crate::store::Store;

/// A definition in its own file, spanning `lines` lines.
fn def(name: &str, kind: Kind, file: &str, lines: u32) -> (String, Symbol) {
    let sym = Symbol {
        name: name.into(),
        kind,
        language: "ruby".into(),
        file: file.into(),
        line: 1,
        end_line: lines,
        parent: None,
        visibility: None,
    };
    (file.to_string(), sym)
}

fn store_with(defs: Vec<(String, Symbol)>) -> Store {
    let mut store = Store::open_in_memory().unwrap();
    let repo = store
        .upsert_repository(&RepoIdentity::local("/tmp/x"), None)
        .unwrap();
    for (file, sym) in defs {
        store
            .replace_file_symbols(repo, &file, "ruby", None, "h", &[sym])
            .unwrap();
    }
    store
}

fn first(store: &Store, query: &str) -> String {
    search::search(store, query, None, None, &ActiveFiles::default(), 10)
        .unwrap()
        .first()
        .map_or_else(|| "<none>".into(), |h| h.name.clone())
}

#[test]
fn a_transposition_finds_the_name() {
    let store = store_with(vec![
        def("Select", Kind::Class, "lib/select.rb", 20),
        def("Selector", Kind::Class, "lib/selector.rb", 20),
    ]);
    assert_eq!(first(&store, "sleect"), "Select");
}

#[test]
fn a_transposed_name_beats_a_name_that_merely_holds_the_letters() {
    // `fetch_conversations` holds every letter of the query in order, which
    // used to keep the typo reading from being considered at all
    let store = store_with(vec![
        def("fetch_version", Kind::Method, "lib/version.rb", 5),
        def("fetch_conversations", Kind::Method, "lib/chat.rb", 5),
    ]);
    assert_eq!(first(&store, "fethc_version"), "fetch_version");
}

#[test]
fn letters_picked_from_inside_a_word_are_not_an_abbreviation() {
    // `t…e` sits inside `naTivE` before the match reaches a word start; the
    // module's size and kind used to carry it past the name that reads as the
    // query
    let store = store_with(vec![
        def("NativeStorage", Kind::Module, "lib/native_storage.rb", 400),
        def("test_tag", Kind::Method, "lib/tags.rb", 2),
    ]);
    assert_eq!(first(&store, "testag"), "test_tag");
}

#[test]
fn a_weak_guess_does_not_beat_a_real_abbreviation() {
    // `cli` is two edits from `cmlz` and keeps two of its letters; `camelize`
    // holds all four in order
    let store = store_with(vec![
        def("camelize", Kind::Method, "lib/inflector.rb", 5),
        def("Cli", Kind::Class, "lib/cli.rb", 200),
    ]);
    assert_eq!(first(&store, "cmlz"), "camelize");
}
