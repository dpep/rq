# Fixture for the extraction guard only: the forms widget.rb leaves out —
# constants, protected, the macros that define methods, a rooted class, an
# access call beside a block that defines the same name.

module Gadgets
  LIMIT = 10
  Names::DEFAULT = "x"
  A, B = 1, 2
  Shape = Struct.new(:width)

  class Gadget
    attr_accessor :balance
    attr :label
    define_method(:spin) { 1 }
    delegate :name, :size, to: :owner
    scope :active, -> { where(active: true) }
    has_many :parts
    has_one :cover
    belongs_to :owner
    field :title

    protected

    def compare(other)
      other
    end

    alias_method :match, :compare
  end

  class ::RootedGadget
  end
end

class Thing
  def go; end
  Helper = Struct.new(:a) do
    def go; end
  end
  private :go
end
