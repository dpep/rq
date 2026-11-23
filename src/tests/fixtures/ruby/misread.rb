# Fixture: valid Ruby the Tree-sitter grammar misreads — a method named
# `self` — for the parity test's oracle-misparse rule. Definitions before it
# must still agree.

class Tracer
  def before; end

  def self
    :me
  end

  def after; end
end
