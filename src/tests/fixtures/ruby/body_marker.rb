# Fixture: a broken file whose def bodies are walked — a `private` inside a
# method body is the method's business at runtime, not a class section.

class Widget
  def setup
    private
    attr_reader :leak
    def inner; end
  end

  def later_public; end

  def broken(
end
