//! Where this popup's rows are, in the same logical-pixel space
//! `view.rs` draws into — the menu's own analogue of
//! `hyprforge-clipmenu::geometry::RowLayout`, and built for the same
//! reason: `app.rs`'s hit-test and `view.rs`'s drawing have to agree on
//! exactly the same numbers, or a click lands on a row that is not the
//! one drawn under the pointer. See `hyprforge-popup::popup`'s own module
//! doc for why that is the one property every `PopupApp` in this suite
//! is built to keep.
//!
//! Unlike `RowLayout`, there is no scrolling here: a tray menu is a
//! handful of rows (`hyprforge-trayd`'s own `radio_menu` never produces
//! more than about six), so [`MenuLayout::popup_height`] simply asks for
//! enough room for every row, and `main.rs` sizes the popup to exactly
//! that.

use hyprforge_tray::menu::{ItemKind, MenuItem};

/// One row's height depends only on whether it is a separator — every
/// other kind (standard, checkmark, disabled) draws one line of text at
/// the same fixed height, the same reason `RowLayout` gives for a
/// clipboard row: a height that depended on the label's own length would
/// make the thing drawn and the thing hit-tested two different heights
/// the moment a proportional font wrapped differently than expected.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MenuLayout {
    /// Padding around the whole popup's content, all four sides.
    pub padding: f64,
    /// Height of an ordinary row (standard, checkmark, or disabled).
    pub row_height: f64,
    /// Height of a separator row — deliberately thinner than an ordinary
    /// row, the same way a real menu draws a rule rather than a blank
    /// clickable line.
    pub separator_height: f64,
}

impl MenuLayout {
    pub const PADDING: f64 = 8.0;
    /// Padding inside an ordinary row, top and bottom.
    pub const ROW_PADDING: f64 = 7.0;
    pub const SEPARATOR_HEIGHT: f64 = 9.0;
    /// Horizontal padding inside a row, and the width reserved on the
    /// left for a checkmark's own glyph — see `view::row`.
    pub const ROW_HPADDING: f64 = 10.0;
    pub const CHECK_WIDTH: f64 = 18.0;

    /// Derives the layout from the theme's font size, the one variable
    /// both this and `view.rs` already agree on — see `RowLayout::for_font_size`'s
    /// own doc for why that is what keeps the two from drifting apart.
    pub fn for_font_size(font_size: f32) -> MenuLayout {
        let font_size = font_size as f64;
        let line = font_size * 1.2;
        MenuLayout {
            padding: Self::PADDING,
            row_height: line + Self::ROW_PADDING * 2.0,
            separator_height: Self::SEPARATOR_HEIGHT,
        }
    }

    fn height_of(&self, item: &MenuItem) -> f64 {
        if item.kind == ItemKind::Separator {
            self.separator_height
        } else {
            self.row_height
        }
    }

    /// The top of `rows[index]`, measured from the popup's own top-left
    /// corner (i.e. already past [`Self::padding`]).
    ///
    /// `view.rs` never needs this: `iced`'s own `column` widget stacks
    /// rows automatically, so nothing in production computes a row's
    /// offset by hand the way `hyprforge-clipmenu::view` positions a
    /// row's pin toggle from a hand-computed rectangle. This exists
    /// purely so a test can name "the pixel row N starts at" without
    /// duplicating [`Self::height_of`]'s summing by hand — `#[cfg(test)]`
    /// says so, rather than leaving a production-only method that only
    /// tests happen to call.
    #[cfg(test)]
    pub fn row_top(&self, rows: &[MenuItem], index: usize) -> f64 {
        rows[..index].iter().map(|r| self.height_of(r)).sum()
    }

    /// Total content height — every row stacked with no gap between them
    /// (a menu reads as one continuous block, not a list of spaced
    /// cards) — plus this layout's own padding on all sides. This is
    /// exactly the popup's own height: nothing here ever scrolls, so
    /// there is no separate "viewport" figure the way `RowLayout` needs
    /// one.
    pub fn popup_height(&self, rows: &[MenuItem]) -> f64 {
        let content: f64 = rows.iter().map(|r| self.height_of(r)).sum();
        (content + self.padding * 2.0).max(self.padding * 2.0)
    }

    /// Whether `item` is something a click can choose — the same
    /// definition [`Self::row_at`] uses to reject a hit, and `view.rs`
    /// uses to decide whether to dim a row and skip its hover highlight.
    /// A separator is never clickable regardless of its (meaningless)
    /// `enabled`/`action` fields; anything else needs both an action to
    /// run and to not be disabled — `MenuItem::disabled` rows carry
    /// neither.
    pub fn is_clickable(item: &MenuItem) -> bool {
        item.kind != ItemKind::Separator && item.enabled && item.action.is_some()
    }

    /// The clickable row under `local_y`, or `None` for a separator, a
    /// disabled row, the padding above the first row, or past the last
    /// row entirely — every one of those is "do nothing", the same as an
    /// unrecognised key press.
    pub fn row_at(&self, rows: &[MenuItem], local_y: f64) -> Option<usize> {
        let mut y = local_y - self.padding;
        if y < 0.0 {
            return None;
        }
        for (index, item) in rows.iter().enumerate() {
            let height = self.height_of(item);
            if y < height {
                return Self::is_clickable(item).then_some(index);
            }
            y -= height;
        }
        None
    }

    /// The pixel width left over for a row's own label once the popup's
    /// outer padding, the row's own horizontal padding, and the
    /// checkmark column have taken their own space — the same reasoning
    /// `hyprforge-clipmenu::geometry::RowLayout::preview_width` gives for
    /// deriving a truncation width from the exact numbers the row is
    /// drawn with, rather than a flat character count that has no idea
    /// how wide this popup actually is.
    ///
    /// Floors at `0.0` for a degenerate popup, the same "never go
    /// negative" discipline `preview_width` and `view::corner_radius`
    /// both already keep.
    pub fn label_width(&self, popup_width: f64) -> f64 {
        (popup_width - self.padding * 2.0 - Self::ROW_HPADDING * 2.0 - Self::CHECK_WIDTH).max(0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyprforge_tray::menu::Menu;

    fn rows() -> Vec<MenuItem> {
        Menu::new(vec![
            MenuItem::checkmark("Wi-Fi", true, "wifi:radio:off"),
            MenuItem::separator(),
            MenuItem::standard("home", "wifi:connect:home"),
            MenuItem::disabled("guest (enterprise)"),
            MenuItem::separator(),
            MenuItem::standard("Wi-Fi settings…", "wifi:settings"),
        ])
        .items
    }

    #[test]
    fn a_point_above_the_first_row_hits_nothing() {
        let layout = MenuLayout::for_font_size(15.0);
        assert_eq!(layout.row_at(&rows(), 0.0), None);
        assert_eq!(layout.row_at(&rows(), layout.padding - 0.01), None);
    }

    #[test]
    fn the_first_row_is_hit_right_up_to_its_own_bottom_edge() {
        let layout = MenuLayout::for_font_size(15.0);
        assert_eq!(layout.row_at(&rows(), layout.padding), Some(0));
        assert_eq!(layout.row_at(&rows(), layout.padding + layout.row_height - 0.01), Some(0));
    }

    #[test]
    fn a_separator_is_never_a_hit() {
        let layout = MenuLayout::for_font_size(15.0);
        let top = layout.padding + layout.row_top(&rows(), 1);
        assert_eq!(layout.row_at(&rows(), top + layout.separator_height / 2.0), None);
    }

    #[test]
    fn a_disabled_row_is_never_a_hit_even_though_it_has_its_own_height() {
        let layout = MenuLayout::for_font_size(15.0);
        let top = layout.padding + layout.row_top(&rows(), 3);
        assert_eq!(layout.row_at(&rows(), top + layout.row_height / 2.0), None);
    }

    #[test]
    fn an_ordinary_row_past_a_separator_still_resolves_to_its_own_index() {
        let layout = MenuLayout::for_font_size(15.0);
        let rows = rows();
        let top = layout.padding + layout.row_top(&rows, 2);
        assert_eq!(layout.row_at(&rows, top + layout.row_height / 2.0), Some(2));
        let settings_top = layout.padding + layout.row_top(&rows, 5);
        assert_eq!(layout.row_at(&rows, settings_top + layout.row_height / 2.0), Some(5));
    }

    #[test]
    fn a_point_past_the_last_row_hits_nothing() {
        let layout = MenuLayout::for_font_size(15.0);
        let rows = rows();
        let end = layout.padding + layout.row_top(&rows, rows.len());
        assert_eq!(layout.row_at(&rows, end + 1.0), None);
    }

    #[test]
    fn popup_height_is_every_row_stacked_plus_padding_on_both_sides() {
        let layout = MenuLayout::for_font_size(15.0);
        let rows = rows();
        let expected = layout.row_top(&rows, rows.len()) + layout.padding * 2.0;
        assert_eq!(layout.popup_height(&rows), expected);
    }

    #[test]
    fn an_empty_menu_is_just_the_padding_not_a_negative_or_zero_height() {
        let layout = MenuLayout::for_font_size(15.0);
        assert_eq!(layout.popup_height(&[]), layout.padding * 2.0);
    }

    #[test]
    fn a_bigger_font_makes_a_taller_row_but_not_a_taller_separator() {
        let small = MenuLayout::for_font_size(12.0);
        let large = MenuLayout::for_font_size(40.0);
        assert!(large.row_height > small.row_height);
        assert_eq!(large.separator_height, small.separator_height);
    }

    #[test]
    fn label_width_leaves_room_for_the_checkmark_column_and_both_paddings() {
        let layout = MenuLayout::for_font_size(15.0);
        let popup_width = 220.0;
        let width = layout.label_width(popup_width);
        let expected = popup_width - layout.padding * 2.0 - MenuLayout::ROW_HPADDING * 2.0 - MenuLayout::CHECK_WIDTH;
        assert_eq!(width, expected);
        assert!(width > 0.0 && width < popup_width);
    }

    #[test]
    fn label_width_floors_at_zero_for_a_popup_narrower_than_its_own_padding() {
        let layout = MenuLayout::for_font_size(15.0);
        assert_eq!(layout.label_width(0.0), 0.0);
        assert_eq!(layout.label_width(-100.0), 0.0);
    }

    #[test]
    fn only_an_enabled_row_with_an_action_is_clickable() {
        assert!(MenuLayout::is_clickable(&MenuItem::standard("home", "wifi:connect:home")));
        assert!(MenuLayout::is_clickable(&MenuItem::checkmark("Wi-Fi", true, "wifi:radio:off")));
        assert!(!MenuLayout::is_clickable(&MenuItem::separator()));
        assert!(!MenuLayout::is_clickable(&MenuItem::disabled("guest (enterprise)")));
        assert!(!MenuLayout::is_clickable(&MenuItem::disabled_checkmark("Wi-Fi (blocked)", false)));
    }
}
