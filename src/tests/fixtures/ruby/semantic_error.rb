# Fixture: an error that leaves the structure whole — a keyword argument in
# an index assignment. No `end` shifts, so def bodies stay unwalked.

class Grid
  def fill
    cells[1, at: 2] = 3
    def helper; end
  end
end
