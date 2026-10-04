# Fixture: a small, domain-neutral Ruby file exercising the ways a method is
# defined on the class itself rather than its instances.

class Widget
  def self.clock
    Clock.new
  end

  def clock
    @clock
  end

  def Widget.registry
    @registry ||= {}
  end

  class << self
    attr_reader :default_size

    private

    def register(name)
      name
    end
  end

  private

  def size
    1
  end
end

module WidgetFormat
  module_function

  def format_size(size)
    size.to_s
  end
end

class Widget
  def self.build_part(name)
    name
  end
  private_class_method :build_part

  alias_method :measure, :size
  alias tick clock
end
