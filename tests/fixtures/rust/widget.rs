//! A small self-contained fixture exercising the Rust symbol extractor.

/// A drawable widget with a label and a size.
pub struct WidgetRenderer {
    pub label: String,
    pub width: u32,
    pub height: u32,
}

/// The orientation a widget can be laid out in.
pub enum Orientation {
    Horizontal,
    Vertical,
}

/// Anything that can be rendered to a string buffer.
pub trait Renderable {
    fn render(&self) -> String;
}

impl Renderable for WidgetRenderer {
    fn render(&self) -> String {
        format!("{} ({}x{})", self.label, self.width, self.height)
    }
}

/// Construct a default horizontal widget.
pub fn make_widget(label: &str) -> WidgetRenderer {
    WidgetRenderer {
        label: label.to_string(),
        width: 100,
        height: 20,
    }
}

pub mod layout {
    /// Compute a packed length for `count` items of `each` size.
    pub fn pack(count: u32, each: u32) -> u32 {
        count * each
    }
}
