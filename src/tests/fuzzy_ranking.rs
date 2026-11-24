//! Fuzzy ranking on concrete cases: which reading of a mistyped or abbreviated
//! query wins when several could.

use crate::core::{Kind, RepoIdentity, Symbol};
use crate::search::{self, Context};
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
        stub: false,
        singleton: false,
    };
    (file.to_string(), sym)
}

fn store_with(defs: Vec<(String, Symbol)>) -> Store {
    let mut store = Store::open_in_memory().unwrap();
    let repo = store.test_checkout(&RepoIdentity::local("/tmp/x"));
    for (file, sym) in defs {
        store
            .replace_file_symbols(repo, &file, "ruby", None, "h", &[sym])
            .unwrap();
    }
    store
}

fn first(store: &Store, query: &str) -> String {
    search::search(store, query, None, None, &Context::default(), 10)
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
fn a_match_past_the_cap_of_names_that_cannot_match_is_still_found() {
    // More names sharing the query's substrings than the candidate cap, none
    // holding its letters in order, all written before the one that does
    let mut store = Store::open_in_memory().unwrap();
    let repo = store.test_checkout(&RepoIdentity::local("/tmp/x"));
    let decoys: Vec<Symbol> = (0..8_100)
        .map(|i| {
            def(
                &format!("zz_connection_{i:05}"),
                Kind::Method,
                "lib/a.rb",
                1,
            )
            .1
        })
        .collect();
    store
        .replace_file_symbols(repo, "lib/a.rb", "ruby", None, "h", &decoys)
        .unwrap();
    let (file, target) = def("new_connection", Kind::Method, "lib/pool.rb", 5);
    store
        .replace_file_symbols(repo, &file, "ruby", None, "h", &[target])
        .unwrap();
    assert_eq!(first(&store, "newconnection"), "new_connection");
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

fn names(store: &Store, query: &str) -> Vec<String> {
    search::search(store, query, None, None, &Context::default(), 10)
        .unwrap()
        .iter()
        .map(|h| h.name.clone())
        .collect()
}

#[test]
fn a_leading_star_prefers_the_name_that_ends_in_the_literal() {
    // `*Error` asks for something before `Error`: `FooError` is the answer,
    // `ErrorLog` merely contains it
    let store = store_with(vec![
        def("ErrorLog", Kind::Class, "lib/error_log.rb", 20),
        def("FooError", Kind::Class, "lib/foo_error.rb", 20),
    ]);
    assert_eq!(first(&store, "*Error"), "FooError");
}

#[test]
fn a_trailing_star_prefers_the_name_that_starts_with_the_literal() {
    let store = store_with(vec![
        def("PROC_INACTIVE", Kind::Class, "lib/proc.rb", 20),
        def("Activity", Kind::Class, "lib/activity.rb", 20),
        def("ActionThing", Kind::Class, "lib/action_thing.rb", 20),
    ]);
    let names = names(&store, "Act*");
    assert_eq!(
        names.last().map(String::as_str),
        Some("PROC_INACTIVE"),
        "{names:?}"
    );
}

#[test]
fn an_inner_star_still_finds_the_name_it_spans() {
    let store = store_with(vec![
        def(
            "WidgetProcessor",
            Kind::Class,
            "lib/widget_processor.rb",
            20,
        ),
        def("Processor", Kind::Class, "lib/processor.rb", 20),
    ]);
    assert_eq!(first(&store, "Widget*proc"), "WidgetProcessor");

    let store = store_with(vec![
        def("latest_limit", Kind::Method, "lib/latest.rb", 5),
        def("test_float_limits", Kind::Method, "lib/float.rb", 5),
    ]);
    assert_eq!(first(&store, "tes*lim"), "test_float_limits");
}
