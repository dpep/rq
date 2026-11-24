# Fixture: a file mid-edit — a block opened in a method and never closed.
# Every `end` after it closes one construct too early, so the defs below
# land in a def's body, and are still found there.

class Store
  def open
    items.each do |item|
  end

  def close; end

  def flush
    true
  end
end
