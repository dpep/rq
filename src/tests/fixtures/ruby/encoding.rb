# encoding: shift_jis
# Fixture: UTF-8 source under a magic comment declaring another encoding,
# read as the UTF-8 rq hands the parser.

class Kana
  def māÜ; end
  attr_reader :nāÜ
  Cā = 1
  def after; end
end
