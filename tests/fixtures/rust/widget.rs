//! Fixture: a small, domain-neutral Rust file exercising every kind the plugin
//! extracts (struct, enum, trait, free fn, impl methods, trait method).

pub struct Widget {
    pub size: u32,
}

pub enum Shape {
    Square,
    Round,
}

pub trait Render {
    fn render(&self) -> String;
}

impl Render for Widget {
    fn render(&self) -> String {
        String::new()
    }
}

impl Widget {
    pub fn new(size: u32) -> Self {
        Widget { size }
    }

    pub fn resize(&mut self, size: u32) {
        self.size = size;
    }
}

pub fn build_widget() -> Widget {
    Widget::new(0)
}
