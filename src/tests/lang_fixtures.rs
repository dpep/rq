//! Ruby, Go, Python, TypeScript and JavaScript plugins, end to end: index a fixture
//! file and assert the ordering — the named definition wins, with the right
//! kind and qualification.

use crate::search::{self, Context};
use crate::tests::support::{indexed, indexed_files, top};

const WIDGET_RB: &str = include_str!("fixtures/ruby/widget.rb");
const WIDGET_GO: &str = include_str!("fixtures/go/widget.go");
const ACCOUNT_PY: &str = include_str!("fixtures/python/account.py");
const WIDGET_TS: &str = include_str!("fixtures/typescript/widget.ts");
const WIDGET_KIT_TS: &str = include_str!("fixtures/typescript/widget-kit.d.ts");
const ACCOUNT_JSX: &str = include_str!("fixtures/javascript/account.jsx");
const WIDGET_STORE_JS: &str = include_str!("fixtures/javascript/widget-store.js");

/// Every hit for `query`, as (parent, singleton, visibility).
fn members(
    store: &crate::store::Store,
    query: &str,
) -> Vec<(Option<String>, bool, Option<String>)> {
    let mut hits: Vec<_> = search::search(store, query, None, None, &Context::default(), 10)
        .unwrap()
        .hits
        .into_iter()
        .map(|h| (h.parent, h.singleton, h.visibility))
        .collect();
    hits.sort();
    hits
}

#[test]
fn ruby_class_methods_are_singletons() {
    let (store, _dir) = indexed("rb", "widget.rb", WIDGET_RB);
    let widget = || Some("Widget".to_string());
    let public = || Some("public".to_string());
    let private = || Some("private".to_string());

    // a class method and an instance method of one name are two definitions
    assert_eq!(
        members(&store, "Widget#clock"),
        [(widget(), false, public()), (widget(), true, public())]
    );
    assert_eq!(members(&store, "registry"), [(widget(), true, public())]);
    // `class << self`, its macros and its private section, which the class
    // body's `private` after it doesn't reach back into
    assert_eq!(
        members(&store, "default_size"),
        [(widget(), true, public())]
    );
    assert_eq!(members(&store, "register"), [(widget(), true, private())]);
    assert_eq!(
        members(&store, "Widget.size"),
        [(widget(), false, private())]
    );
    // a module function is called on the module; its instance copy is private
    assert_eq!(
        members(&store, "format_size"),
        [(Some("WidgetFormat".into()), true, public())]
    );
    assert!(!top(&store, "Widget").singleton);
    // a reopening makes a class method private after the fact, and an alias
    // copies its original's visibility, not the section's
    assert_eq!(members(&store, "build_part"), [(widget(), true, private())]);
    assert_eq!(members(&store, "measure"), [(widget(), false, private())]);
    assert_eq!(members(&store, "tick"), [(widget(), false, public())]);
}

#[test]
fn go_definitions_rank_and_classify() {
    let (store, _dir) = indexed("go", "widget.go", WIDGET_GO);

    let widget = top(&store, "Widget");
    assert_eq!(widget.name, "Widget");
    assert_eq!(widget.kind, "struct");
    assert_eq!(top(&store, "Renderer").kind, "trait");

    let resize = top(&store, "Resize");
    assert_eq!(resize.kind, "method");
    assert_eq!(resize.parent.as_deref(), Some("Widget"));

    assert_eq!(top(&store, "BuildWidget").kind, "function");

    // a named func type outranks a function whose name only starts with it
    let render = top(&store, "RenderFunc");
    assert_eq!(
        (render.name.as_str(), render.kind.as_str()),
        ("RenderFunc", "type")
    );

    // the const outranks a function whose name only starts with it
    let max = top(&store, "MaxRetries");
    assert_eq!(
        (max.name.as_str(), max.kind.as_str()),
        ("MaxRetries", "constant")
    );
    assert_eq!(top(&store, "ColorBlue").kind, "constant");

    // a field is found by its name and its struct, and ranks below the struct
    // and func whose names it shares (`Widget`, `BuildWidget` above)
    let title = top(&store, "Title");
    assert_eq!(
        (title.kind.as_str(), title.parent.as_deref()),
        ("field", Some("Frame"))
    );
    assert_eq!(top(&store, "Widget.Size").kind, "field");
    let embedded = top(&store, "Frame.Widget");
    assert_eq!(
        (embedded.kind.as_str(), embedded.name.as_str()),
        ("field", "Widget")
    );
}

#[test]
fn python_definitions_rank_and_classify() {
    let (store, _dir) = indexed("py", "account.py", ACCOUNT_PY);

    let account = top(&store, "Account");
    assert_eq!(account.name, "Account");
    assert_eq!(account.kind, "class");

    let deposit = top(&store, "deposit");
    assert_eq!(deposit.kind, "method");
    assert_eq!(deposit.parent.as_deref(), Some("Account"));

    assert_eq!(top(&store, "build_account").kind, "function");

    // an enum.Enum subclass is an enum of variants, found through its scope
    let status = top(&store, "AccountStatus");
    assert_eq!(
        (status.name.as_str(), status.kind.as_str()),
        ("AccountStatus", "enum")
    );
    let open = top(&store, "AccountStatus.OPEN");
    assert_eq!(
        (open.name.as_str(), open.kind.as_str()),
        ("OPEN", "variant")
    );

    // a closure ranks below the module-level function it shares a name with,
    // and says why
    let audits = search::search(&store, "_audit", None, None, &Context::default(), 10)
        .unwrap()
        .hits;
    let parents: Vec<_> = audits.iter().map(|h| h.parent.as_deref()).collect();
    assert_eq!(parents, [None, Some("build_account")]);
    assert!(audits[1].features.iter().any(|f| f.name == "local"));

    // the constant outranks a function whose name only starts with it
    let max = top(&store, "MAX_RETRIES");
    assert_eq!(
        (max.name.as_str(), max.kind.as_str()),
        ("MAX_RETRIES", "constant")
    );
    let default = top(&store, "DEFAULT_BALANCE");
    assert_eq!(default.kind, "constant");
    assert_eq!(default.parent.as_deref(), Some("Account"));

    // class attributes, annotated or assigned, are fields of their class; an
    // instance's `self.x = …` is not a declaration
    for (query, name) in [
        ("owner", "owner"),
        ("Account.status", "status"),
        ("currency", "currency"),
    ] {
        let f = top(&store, query);
        assert_eq!(
            (f.name.as_str(), f.kind.as_str()),
            (name, "field"),
            "{query}"
        );
        assert_eq!(f.parent.as_deref(), Some("Account"));
    }
    // class and static methods are the class's own; an instance method isn't
    assert!(top(&store, "from_owner").singleton);
    assert!(top(&store, "is_valid_amount").singleton);
    assert!(!top(&store, "withdraw").singleton);

    let settings = search::search(&store, "deposit", None, None, &Context::default(), 10)
        .unwrap()
        .hits;
    let kinds: Vec<_> = settings.iter().map(|h| h.kind.as_str()).collect();
    assert_eq!(kinds, ["method", "field"], "the method before the field");
    assert!(
        search::search(&store, "last_deposit", None, None, &Context::default(), 10)
            .unwrap()
            .hits
            .is_empty()
    );
}

#[test]
fn typescript_definitions_rank_and_classify() {
    let (store, _dir) = indexed("ts", "widget.ts", WIDGET_TS);

    let widget = top(&store, "Widget");
    assert_eq!(widget.name, "Widget");
    assert_eq!(widget.kind, "class");
    assert_eq!(widget.language, "typescript");

    assert_eq!(top(&store, "Renderer").kind, "trait");
    assert_eq!(top(&store, "WidgetSize").kind, "type");
    assert_eq!(top(&store, "WidgetColor").kind, "enum");

    // an enum member is found by its own name and scoped by its enum
    let green = top(&store, "WidgetColor.Green");
    assert_eq!(
        (green.name.as_str(), green.kind.as_str()),
        ("Green", "variant")
    );
    assert_eq!(green.parent.as_deref(), Some("WidgetColor"));

    let resize = top(&store, "resize");
    assert_eq!(resize.kind, "method");
    assert_eq!(resize.parent.as_deref(), Some("Widget"));

    assert_eq!(top(&store, "buildWidget").kind, "function");
    // an arrow assigned to a const is a function like any other
    assert_eq!(top(&store, "defaultWidget").kind, "function");

    // the constant outranks a function whose name only starts with it
    let max = top(&store, "MAX_RETRIES");
    assert_eq!(
        (max.name.as_str(), max.kind.as_str()),
        ("MAX_RETRIES", "constant")
    );
    // an exported `let` is the module's API, like an exported `const`
    let active = top(&store, "activeWidget");
    assert_eq!(
        (active.kind.as_str(), active.visibility.as_deref()),
        ("constant", Some("public"))
    );
    // an `export { … }` list or `export default name` exports a declaration
    // made earlier, by its local name
    for name in ["WIDGET_THEME", "formatWidget", "trimWidgetName"] {
        assert_eq!(
            top(&store, name).visibility.as_deref(),
            Some("public"),
            "{name}"
        );
    }
    let width = top(&store, "DEFAULT_WIDTH");
    assert_eq!(width.kind, "constant");
    assert_eq!(width.parent.as_deref(), Some("Widget"));

    // an interface's, a class's and an object type's properties are fields
    let color = top(&store, "color");
    assert_eq!(
        (color.kind.as_str(), color.parent.as_deref()),
        ("field", Some("WidgetOptions"))
    );
    assert_eq!(top(&store, "WidgetSize.height").kind, "field");
    let owner = top(&store, "Widget.owner");
    assert_eq!(
        (owner.kind.as_str(), owner.visibility.as_deref()),
        ("field", Some("private"))
    );
    // static members are the class's own: methods, getters and fields
    for name in ["create", "registry", "instances"] {
        assert!(top(&store, name).singleton, "{name}");
    }
    assert!(!top(&store, "resize").singleton);
    // a `static readonly` constant is a constant, never an instance's
    assert!(!top(&store, "DEFAULT_WIDTH").singleton);
    // a field ranks below the function it shares a name with
    let build = search::search(&store, "defaultWidget", None, None, &Context::default(), 10)
        .unwrap()
        .hits;
    let kinds: Vec<_> = build.iter().map(|h| h.kind.as_str()).collect();
    assert_eq!(kinds, ["function", "field"]);
    // an object literal's properties are values, not declarations
    assert!(
        search::search(&store, "label", None, None, &Context::default(), 10)
            .unwrap()
            .hits
            .is_empty()
    );
}

#[test]
fn typescript_declarations_rank_below_implementations() {
    let (store, _dir) = indexed_files(
        "ts-ambient",
        &[("widget.ts", WIDGET_TS), ("widget-kit.d.ts", WIDGET_KIT_TS)],
    );
    let hits = |q: &str| {
        search::search(&store, q, None, None, &Context::default(), 10)
            .unwrap()
            .hits
    };

    // the implementation first, and its declaration still found
    let build = hits("buildWidget");
    let files: Vec<_> = build.iter().map(|h| h.file.as_str()).collect();
    assert_eq!(files, ["widget.ts", "widget-kit.d.ts"]);
    assert!(build[1].features.iter().any(|f| f.name == "stub"));

    // overload signatures fold into the implementation that follows them
    let resize = hits("resizeAll");
    assert_eq!(resize.len(), 1, "{resize:?}");
    assert_eq!(resize[0].declarations, 3);
    assert!(resize[0].end_line > Some(resize[0].line + 1), "the body");

    // declaration-only API is found, scoped by its module, and folds too
    let open = top(&store, "widget-kit.openStore");
    assert_eq!((open.kind.as_str(), open.declarations), ("function", 2));
    assert_eq!(
        top(&store, "openStore.sync").parent.as_deref(),
        Some("widget-kit.openStore")
    );
    assert_eq!(top(&store, "widget-kit.Store").kind, "class");

    // `declare global` adds to the top level
    let debug = top(&store, "WIDGET_DEBUG");
    assert_eq!((debug.kind.as_str(), debug.parent), ("constant", None));
    assert_eq!(top(&store, "trackWidget").kind, "function");
}

#[test]
fn javascript_definitions_rank_and_classify() {
    let (store, _dir) = indexed("jsx", "account.jsx", ACCOUNT_JSX);

    let account = top(&store, "Account");
    assert_eq!(account.name, "Account");
    assert_eq!(account.kind, "class");
    assert_eq!(account.language, "javascript");

    let deposit = top(&store, "deposit");
    assert_eq!(deposit.kind, "method");
    assert_eq!(deposit.parent.as_deref(), Some("Account"));
    assert!(!deposit.singleton);
    assert!(top(&store, "Account.open").singleton);

    assert_eq!(top(&store, "buildAccount").kind, "function");
    // a JSX-returning component in a `.jsx` file still parses
    assert_eq!(top(&store, "AccountBadge").kind, "function");
    // so is a component wrapped in `memo(…)`, over a longer name it prefixes
    let row = top(&store, "AccountRow");
    assert_eq!(
        (row.name.as_str(), row.kind.as_str()),
        ("AccountRow", "function")
    );

    // a camelCase const is a constant too, and outranks a longer function
    let default = top(&store, "defaultAccount");
    assert_eq!(
        (default.name.as_str(), default.kind.as_str()),
        ("defaultAccount", "constant")
    );
}

#[test]
fn commonjs_assignments_rank_above_test_doubles() {
    // a test double assigned the same way is test code, and ranks below
    let (store, _dir) = indexed_files(
        "cjs",
        &[
            ("widget-store.js", WIDGET_STORE_JS),
            (
                "widget-store.test.js",
                "proto.listen = function listen() {};\nexports.createWidgetStore = () => {};\n",
            ),
        ],
    );

    let render = top(&store, "WidgetStore#render");
    assert_eq!(
        (render.kind.as_str(), render.parent.as_deref()),
        ("method", Some("WidgetStore"))
    );
    for (query, parent) in [
        ("listen", Some("proto")),
        ("createWidgetStore", None),
        ("show", None),
        ("flush", None),
        ("clear", Some("WidgetCache")),
    ] {
        let hit = top(&store, query);
        assert_eq!(hit.file, "widget-store.js", "{query}");
        assert_eq!(hit.parent.as_deref(), parent, "{query}");
    }
    // a constructor's own name, re-exported, is still its declaration
    // and exporting it makes it public
    let ctor = top(&store, "WidgetStore");
    assert_eq!(
        (ctor.kind.as_str(), ctor.line, ctor.visibility.as_deref()),
        ("function", 9, Some("public"))
    );
}
