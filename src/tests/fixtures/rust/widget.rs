//! Fixture: a small, domain-neutral Rust file exercising every kind the plugin
//! extracts (struct and its fields, enum, trait, free fn, impl methods, trait
//! method).

pub struct Widget {
    pub size: u32,
    label: String,
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
        Widget {
            size,
            label: String::new(),
        }
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn resize(&mut self, size: u32) {
        self.size = size;
    }
}

pub fn build_widget() -> Widget {
    Widget::new(0)
}
