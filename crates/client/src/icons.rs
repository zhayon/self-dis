//! Те же контуры, что рисует страница: штрих 1.8, холст 24×24.

use std::sync::OnceLock;

use iced::widget::svg::{self, Handle};
use iced::{Color, Element, Length};

#[derive(Clone, Copy)]
pub enum Glyph {
    Hash,
    Speaker,
    Mic,
    MicOff,
    Headphones,
    HeadphonesOff,
    Leave,
}

pub fn draw<'a, Message: 'a>(glyph: Glyph, color: Color, size: f32) -> Element<'a, Message> {
    svg::Svg::new(handle(glyph))
        .width(Length::Fixed(size))
        .height(Length::Fixed(size))
        .style(move |_theme, _status| svg::Style { color: Some(color) })
        .into()
}

fn handle(glyph: Glyph) -> Handle {
    static HANDLES: OnceLock<[Handle; 7]> = OnceLock::new();
    HANDLES.get_or_init(|| {
        [
            svg_handle(&["M4 9h16", "M4 15h16", "M10 3 8 21", "M16 3l-2 18"]),
            svg_handle(&[
                "M4 10v4h3.5L14 19V5L7.5 10Z",
                "M17 9a4 4 0 0 1 0 6",
                "M19 7a7 7 0 0 1 0 10",
            ]),
            svg_handle(&[
                "M9 6a3 3 0 0 1 6 0v5a3 3 0 0 1-6 0Z",
                "M8 11a4 4 0 0 0 8 0",
                "M12 15v3",
                "M9 19h6",
            ]),
            svg_handle(&[
                "M9 6a3 3 0 0 1 6 0v5a3 3 0 0 1-6 0Z",
                "M8 11a4 4 0 0 0 8 0",
                "M12 15v3",
                "M9 19h6",
                "M5 5l14 14",
            ]),
            svg_handle(&["M4 13a8 8 0 0 1 16 0", "M4 13h3v6H4Z", "M17 13h3v6h-3Z"]),
            svg_handle(&[
                "M4 13a8 8 0 0 1 16 0",
                "M4 13h3v6H4Z",
                "M17 13h3v6h-3Z",
                "M5 5l14 14",
            ]),
            svg_handle(&["M10 5H6v14h8v-4", "M13 12h7", "M17 9l3 3-3 3"]),
        ]
    })[glyph as usize]
        .clone()
}

fn svg_handle(paths: &[&str]) -> Handle {
    let mut body = String::from(r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24">"#);
    for path in paths {
        body.push_str(&format!(
            r##"<path d="{path}" fill="none" stroke="#000" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"/>"##
        ));
    }
    body.push_str("</svg>");
    Handle::from_memory(body.into_bytes())
}
