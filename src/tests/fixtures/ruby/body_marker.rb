# Fixture: a broken file whose def bodies are walked. A missing `end` nests
# the rest of the class, its `private` included, into a def body; a block in
# a body is runtime metaprogramming.

class Widget
  def self.included(base)
    base.class_eval do
      private
      def runtime; end
    end
  end

  def open
    if x
  end

  private

  def hidden; end
end
