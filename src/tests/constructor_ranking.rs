//! `Foo.new` answers with Foo's own constructor, or Foo itself when it declares
//! none: never another class's constructor that merely shares Foo's file.

use crate::tests::support::{indexed_files, top};

#[test]
fn a_sibling_class_in_the_owners_file_does_not_answer_its_constructor() {
    let (store, _dir) = indexed_files(
        "ctor-sibling",
        &[
            (
                "widget_fetcher.rb",
                "class WidgetFetcher\n  def run; end\nend\n\nclass Helper\n  def initialize; end\nend\n",
            ),
            (
                "gadget_loader.py",
                "class GadgetLoader:\n    def run(self):\n        pass\n\nclass Helper:\n    def __init__(self):\n        pass\n",
            ),
            (
                "part_maker.ts",
                "export class PartMaker {\n  run() {}\n}\n\nexport class Helper {\n  constructor() {}\n}\n",
            ),
        ],
    );
    for (query, owner) in [
        ("WidgetFetcher.new", "WidgetFetcher"),
        ("GadgetLoader.new", "GadgetLoader"),
        ("PartMaker.new", "PartMaker"),
    ] {
        let hit = top(&store, query);
        assert_eq!(
            (hit.name.as_str(), hit.kind.as_str()),
            (owner, "class"),
            "{query}"
        );
    }
}

#[test]
fn a_nested_class_in_a_file_named_for_the_owner_does_not_answer_its_constructor() {
    // `Widget::Part` lives in widget.rb, so `Widget` is in its path, but its
    // constructor is Part's, not Widget's
    let (store, _dir) = indexed_files(
        "ctor-nested-in-path",
        &[
            (
                "widget.rb",
                "class Widget::Part\n  def initialize; end\nend\n",
            ),
            ("widget_model.rb", "class Widget\n  def run; end\nend\n"),
        ],
    );
    let hit = top(&store, "Widget.new");
    assert_eq!((hit.name.as_str(), hit.kind.as_str()), ("Widget", "class"));
}

#[test]
fn an_owner_without_a_constructor_answers_before_a_nested_namesakes_constructor() {
    // `Widget` inherits its constructor; `Dev::Widget` declares one. The class
    // the query names in full is the answer, not the one that only ends in it,
    // and a `new_*` factory of Widget's is not its constructor either
    let (store, _dir) = indexed_files(
        "ctor-owner-vs-nested",
        &[
            (
                "widget.rb",
                "class Widget < Base\n  def self.new_from_params; end\n  def run; end\nend\n",
            ),
            (
                "dev_widget.rb",
                "module Dev\n  class Widget\n    def initialize; end\n  end\nend\n",
            ),
            (
                "gadget.py",
                "class Gadget(Base):\n    def run(self):\n        pass\n",
            ),
            (
                "factory.py",
                "class Factory:\n    class Gadget:\n        def __init__(self):\n            pass\n",
            ),
        ],
    );
    for (query, owner) in [("Widget.new", "Widget"), ("Gadget.new", "Gadget")] {
        let hit = top(&store, query);
        assert_eq!(
            (hit.name.as_str(), hit.kind.as_str()),
            (owner, "class"),
            "{query}"
        );
        assert_eq!(hit.parent, None, "{query}");
    }
    // naming the nested one in full still reaches its constructor
    let hit = top(&store, "Dev::Widget.new");
    assert_eq!(
        (hit.name.as_str(), hit.parent.as_deref()),
        ("initialize", Some("Dev::Widget"))
    );
    let hit = top(&store, "Factory.Gadget.new");
    assert_eq!(hit.name, "__init__");
}
