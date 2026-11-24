# Fixture: constant paths with whitespace around their `::`, which Ruby
# allows; the names are their parts. The Tree-sitter grammar errors here.

module Shapes
  class Base; end

  class Base::
    Spread
    def wide; end
  end

  class Base:: Narrow; end

  Base::
    LIMIT = 1
end
