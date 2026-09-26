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
    popup_width: f64,
) -> Vec<Element<'a, Message, iced_widget::Theme, iced_tiny_skia::Renderer>> {
    items
        .iter()
        .enumerate()
        .map(|(index, item)| row_element(item, layout, Some(index) == hovered, theme, popup_width))
        .collect()
}

/// A rough character cap for a label of `available_width` logical
/// pixels — the same estimate, for the same reason, as
/// `hyprforge-clipmenu::view::max_preview_chars`: this crate builds a
/// fresh `UserInterface` every frame rather than keeping one that could
/// measure a string's actual shaped width, so there is no real
/// text-metrics call to make here. `0.6` is a plain average-glyph-width
/// factor for a proportional font, generous enough that ordinary text
/// reliably fits inside the estimate; `.clip(true)` on the row below is
/// the backstop for whatever this slightly undershoots, not the
/// mechanism itself.
fn max_label_chars(font_size: f32, available_width: f64) -> usize {
    let avg_char_width = (font_size as f64 * 0.6).max(1.0);
    ((available_width / avg_char_width).floor() as usize).max(1)
}

/// Truncates `label` to `max_chars`, character-safe — never byte-safe:
/// a menu label is user-controlled text (a Wi-Fi SSID, here), which can
/// hold multi-byte characters an arbitrary byte offset would split in
/// half. Ends in an ellipsis when anything was actually cut, the same
/// shape `hyprforge_clipboard::types::Content::preview` already uses.
fn truncate_label(label: &str, max_chars: usize) -> std::borrow::Cow<'_, str> {
    if label.chars().count() <= max_chars {
        return std::borrow::Cow::Borrowed(label);
    }
    let cut: String = label.chars().take(max_chars.saturating_sub(1)).collect();
    std::borrow::Cow::Owned(format!("{cut}\u{2026}"))
}

fn row_element<'a, Message: 'a>(
    item: &'a MenuItem,
    layout: &MenuLayout,
    hovered: bool,
    theme: &'a Theme,
    popup_width: f64,
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

    // `layout.label_width` is derived from exactly the numbers this
    // function lays the row out with (the outer padding, the row's own
    // `ROW_HPADDING`, `CHECK_WIDTH`) — the same discipline
    // `Layout::chars_that_fit` keeps in `hyprforge-clipmenu`, so the
    // label ends before the row's own edge rather than being clipped by
    // it. `Wrapping::None` keeps this crate's own behaviour of never
    // growing a row taller for a long label; without truncation that
    // combination is what let a long Wi-Fi SSID draw straight past its
    // own column and off the popup's right edge, mid-word, with nothing
    // to show it continued.
    let max_chars = max_label_chars(theme.font_size, layout.label_width(popup_width));
    let label_text = truncate_label(&item.label, max_chars);
    let label: Element<'a, Message, iced_widget::Theme, iced_tiny_skia::Renderer> =
        text(label_text.into_owned()).size(theme.font_size).wrapping(Wrapping::None).color(text_color).into();

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
        // Belt and braces alongside truncation above, the same pairing
        // `hyprforge-clipmenu::view::entry_row` uses: a label estimated
        // to fit but actually a little wider (a font whose glyphs run
        // wider than the `0.6` average, say) is clipped at the row's own
        // edge instead of drawing into the row after it — truncation is
        // the mechanism, this is the backstop for what it undershoots.
        .clip(true)
        .style(move |_: &iced_widget::Theme| container::Style {
            background: background.map(Into::into),
            ..Default::default()
        })
        .into()
}
