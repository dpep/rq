# Fixture: methods named by literal arguments, in every spelling a literal
# takes, and the names built at runtime that can't be read statically.

class Labels
  attr_reader %q(plain), %Q(double), "esc\u0061ped", 'quoted'
  attr_accessor "a#{suffix}"
  define_method("dyn_#{kind}") { kind }
  scope "/:slug", -> { all }

  def hidden; end
  def secret; end
  private %w[hidden], %q(secret)
end

class Symbols
  attr_reader :"ready?", :'single', %s(percent), :☠
  define_method(:"dyn_#{kind}") { kind }
  alias_method :"copy?", :"ready?"
  alias :"spaced name" :single
  alias :"twin?" :single
  alias :"alias_#{kind}" :single

  def shown; end
  def kept; end
  private %I[shown#{kind} kept], :"shown"
end
