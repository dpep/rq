# Fixture: the syntax around a definition that a parser can trip over —
# heredocs, endless and operator defs, `class << self`, blocks that define,
# unicode names, block comments and a data section.

=begin
class Commented
  def hidden; end
end
=end

module Forms
  TEMPLATE = <<~TEXT
    class NotAClass
      def not_a_method; end
    end
  TEXT

  QUERY = <<-SQL.strip
    SELECT 1
  SQL

  class Vector
    attr_reader :x, :y

    def initialize(x, y)
      @x, @y = x, y
    end
    def length = Math.sqrt(x**2 + y**2)
    def [](i) = i.zero? ? x : y
    def []=(i, v); end
    def <=>(other) = length <=> other.length
    def ==(other) = to_a == other.to_a
    def ~@; end
    def -@ = self.class.new(-x, -y)
    def +@; self; end
    def !@; false; end
    def call!(arg) = arg
    def ready? = true
    def x=(value); end
    def `(cmd); end

    def describe
      <<~DOC
        def inside_a_heredoc; end
      DOC
    end

    class << self
      def origin = new(0, 0)

      private

      def cache; end
    end
  end

  Point = Struct.new(:x, :y) do
    def to_s = "(#{x}, #{y})"
    def norm; end
    private :norm
  end

  class Größe
    def maß; end
    ÜBER = 1
  end

  class Dynamic
    define_method(:spin) { 1 }
    define_method("twirl") do
      2
    end
    define_method(name) { 3 }
    %i[a b].each { |n| define_method(n) { n } }

    class self.class::Scoped
      def inside; end
    end
  end
end

__END__
class AfterEnd
  def nope; end
end
