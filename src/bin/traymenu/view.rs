//! The popup's widget tree, built fresh each frame from the flat row
//! list `app.rs` holds. Every colour comes from [`hyprforge_look::Theme`]
//! — CLAUDE.md is explicit that no app may define its own colour
//! constant, the same rule `hyprforge-clipmenu::view` follows.

use crate::layout::MenuLayout;
use hyprforge_look::Theme;
use hyprforge_tray::menu::{ItemKind, MenuItem};
use iced_runtime::core::text::Wrapping;
use iced_runtime::core::{Element, Length, Padding};
use iced_widget::{container, row, text, Space};

pub fn to_iced(c: hyprforge_look::Color) -> iced_runtime::core::Color {
    iced_runtime::core::Color::from_rgba8(c.r, c.g, c.b, c.a as f32 / 255.0)
}


/// Builds every row — separators included — in the same top-to-bottom
/// order `MenuLayout::row_at` walks, so a hover or a click resolved
/// against one index always names the row drawn at that same position.
pub fn rows<'a, Message: 'a>(
    items: &'a [MenuItem],
    layout: &MenuLayout,
    hovered: Option<usize>,
    theme: &'a Theme,
) -> Vec<Element<'a, Message, iced_widget::Theme, iced_tiny_skia::Renderer>> {
    items
        .iter()
        .enumerate()
        .map(|(index, item)| row_element(item, layout, Some(index) == hovered, theme))
        .collect()
}

fn row_element<'a, Message: 'a>(
    item: &'a MenuItem,
    layout: &MenuLayout,
    hovered: bool,
    theme: &'a Theme,
) -> Element<'a, Message, iced_widget::Theme, iced_tiny_skia::Renderer> {
    if item.kind == ItemKind::Separator {
        let line_color = to_iced(theme.surfaces.text_dim);
        return container(
            container(Space::new())
                .width(Length::Fill)
                .height(Length::Fixed(1.0))
                .style(move |_: &iced_widget::Theme| container::Style {
                    background: Some(line_color.into()),
                    ..Default::default()
                }),
        )
        .width(Length::Fill)
        .height(Length::Fixed(layout.separator_height as f32))
        .align_y(iced_runtime::core::alignment::Vertical::Center)
        .into();
    }

    let clickable = MenuLayout::is_clickable(item);
    let text_color = if !clickable {
        to_iced(theme.surfaces.text_dim)
    } else {
        to_iced(theme.surfaces.text)
    };

    // A checkmark gets a fixed-width glyph column so every row's label
    // starts at the same left edge whether or not it has one — a ragged
    // left edge between checkmark and plain rows reads as a layout bug
    // even though nothing is actually misaligned.
    let check: Element<'a, Message, iced_widget::Theme, iced_tiny_skia::Renderer> = match item.toggle {
        Some(on) => {
            let mark = if on { "\u{2713}" } else { "" };
            text(mark).size(theme.font_size).color(text_color).into()
        }
        None => Space::new().width(Length::Fixed(0.0)).into(),
    };
    let check: Element<'a, Message, iced_widget::Theme, iced_tiny_skia::Renderer> = container(check)
        .width(Length::Fixed(MenuLayout::CHECK_WIDTH as f32))
        .align_y(iced_runtime::core::alignment::Vertical::Center)
        .into();

    // The label arrives already fitted: `main` measured every row with
    // the real shaper (`measure.rs`), sized the popup to the widest, and
    // cut — with an ellipsis — only what is wider than the widest popup
    // allows. `Wrapping::None` keeps a row one line tall whatever it
    // holds; without the cut, that is what let a long Wi-Fi SSID draw
    // straight off the popup's right edge, mid-word.
    let label: Element<'a, Message, iced_widget::Theme, iced_tiny_skia::Renderer> =
        text(item.label.as_str()).size(theme.font_size).wrapping(Wrapping::None).color(text_color).into();

    let content = row![check, label].align_y(iced_runtime::core::alignment::Vertical::Center);

    let background = if hovered && clickable { Some(to_iced(theme.accent).scale_alpha(0.18)) } else { None };

    container(content)
        .width(Length::Fill)
        .height(Length::Fixed(layout.row_height as f32))
        .padding(Padding {
            top: 0.0,
            right: MenuLayout::ROW_HPADDING as f32,
            bottom: 0.0,
            left: MenuLayout::ROW_HPADDING as f32,
        })
        .align_y(iced_runtime::core::alignment::Vertical::Center)
        // The backstop, not the mechanism: the label was measured to
        // fit, and anything that still runs over (a rounding at the
        // last pixel) is clipped at the row's own edge rather than
        // drawing into the row after it.
        .clip(true)
        .style(move |_: &iced_widget::Theme| container::Style {
            background: background.map(Into::into),
            ..Default::default()
        })
        .into()
}
