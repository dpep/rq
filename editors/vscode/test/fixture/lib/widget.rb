class Widget
  attr_reader :name

  def initialize(name)
    @name = name
  end

  def save
    true
  end

  def empty?
    name.nil?
  end
end
