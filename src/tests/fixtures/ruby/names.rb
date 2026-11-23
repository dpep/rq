# Fixture: methods named by literal arguments, in every spelling a literal
# takes, and the names built at runtime that can't be read statically.

class Labels
  attr_reader %q(plain), %Q(double), "tab\tless", 'quoted'
  attr_accessor "a#{suffix}"
  define_method("dyn_#{kind}") { kind }

  def hidden; end
  def secret; end
  private %w[hidden], %q(secret)
end
