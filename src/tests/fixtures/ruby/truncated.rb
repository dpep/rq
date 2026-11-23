# Fixture: a file mid-edit — the last method is still being typed and nothing
# is closed. The definitions above the cut keep their places.

module Gizmos
  class Sprocket
    attr_reader :teeth

    def self.build(teeth)
      new(teeth)
    end

    def spin
      @teeth.each do |t|
        t
      end
    end

    private

    def mesh(other)
      if other.teeth == @teeth
