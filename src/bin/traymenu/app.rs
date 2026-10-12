//! The seam between `hyprforge-popup`'s Wayland/iced plumbing and this
//! crate's own model: a flat list of rows, a hover index, and nothing
//! else — see `hyprforge_popup::popup`'s own module doc for why this is
//! one `impl PopupApp` rather than three smaller traits.

use hyprforge_look::Theme;
use hyprforge_popup::{Dismissal, Keysym, Modifiers, PopupApp};
use hyprforge_tray::menu::MenuItem;
use iced_runtime::core::{Element, Length, Padding};
use iced_widget::{column, container};
use std::convert::Infallible;

use crate::layout::MenuLayout;
use crate::levels::{Levels, Shown};
use crate::view;

/// However this popup ended — see [`hyprforge_popup::Outcome::App`].
///
/// `Chosen` carries the row's index in [`Levels::flat`] rather than its
/// [`MenuItem::action`] string directly, because [`PopupApp::Outcome`]
/// must be `Copy` — a `String` cannot be. `main.rs` keeps the flattened
/// rows outside the app for exactly this reason, and reads
/// `flat[index].action` once [`hyprforge_popup::Popup::run`] returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuOutcome {
    Chosen(usize),
    Cancelled,
}

pub struct TrayMenuApp {
    levels: Levels,
    /// Which submenu is open: `None` is the top level.
    level: Option<usize>,
    /// The open level's rows as drawn, and what each one is — rebuilt
    /// whenever the level changes, so hit-testing and drawing read one
    /// list.
    rows: Vec<MenuItem>,
    shown: Vec<Shown>,
    layout: MenuLayout,
    hovered: Option<usize>,
    /// What a click outside this menu should do — the user's own
    /// `tray.toml` setting, read once by `main.rs` and handed in here.
    /// See [`hyprforge_popup::Dismissal`] for why "close on a click
    /// elsewhere" and "how the keyboard is held" are one choice.
    dismissal: Dismissal,
}

impl TrayMenuApp {
    /// [`hyprforge_popup::PopupApp::pointer_click`]'s own `width`
    /// parameter is still taken and ignored: every row still draws
    /// `Length::Fill` rather than positioning anything from the popup's
    /// right edge the way `hyprforge-clipmenu`'s pin toggle and time
    /// label do, so a click never needs it. [`PopupApp::view`]'s `width`
    /// is different — `view.rs::row_element` now uses it (via
    /// `MenuLayout::label_width`) to truncate a long label before it
    /// draws, so that one *is* read.
    pub fn new(levels: Levels, layout: MenuLayout, dismissal: Dismissal) -> Self {
        let (rows, shown) = levels.rows(None);
        TrayMenuApp { levels, level: None, rows, shown, layout, hovered: None, dismissal }
    }

    /// Shows `level`, with the highlight on its first row when the
    /// keyboard got there, and on nothing when the pointer did — the
    /// pointer is about to say where it is anyway.
    fn open(&mut self, level: Option<usize>, by_keyboard: bool) {
        self.level = level;
        (self.rows, self.shown) = self.levels.rows(level);
        self.hovered = if by_keyboard { next_clickable(&self.rows, None) } else { None };
    }

    /// What choosing row `index` of the open level does: end with it,
    /// or move to another level and keep going.
    fn choose(&mut self, index: usize, by_keyboard: bool) -> Option<MenuOutcome> {
        match self.shown.get(index)? {
            Shown::Back => {
                self.open(self.levels.up(self.level), by_keyboard);
                None
            }
            &Shown::Item(flat) if self.levels.opens(flat) => {
                self.open(Some(flat), by_keyboard);
                None
            }
            &Shown::Item(flat) => Some(MenuOutcome::Chosen(flat)),
        }
    }

    /// Which rows of the open level open a submenu, for the mark.
    fn opens(&self) -> Vec<bool> {
        self.shown.iter().map(|s| matches!(*s, Shown::Item(f) if self.levels.opens(f))).collect()
    }
}

impl PopupApp for TrayMenuApp {
    type Outcome = MenuOutcome;

    /// Unlike the clipboard and emoji popups, nothing here is typed
    /// into: the rows are chosen with a pointer, and the only keys that
    /// matter are Escape and the arrows. That is what makes the
    /// on-demand option available at all — see [`Dismissal`].
    fn dismissal(&self) -> Dismissal {
        self.dismissal
    }

    fn view<'a>(
        &'a mut self,
        theme: &'a Theme,
        _now: u64,
        _width: f64,
    ) -> Element<'a, Infallible, iced_widget::Theme, iced_tiny_skia::Renderer> {
        let opens = self.opens();
        let rows = view::rows(&self.rows, &opens, &self.layout, self.hovered, theme);
        let popup_border = theme.accent;
        let popup_radius = theme.corner_radius();
        let root_background = theme.surfaces.root;
        container(column(rows).spacing(0).width(Length::Fill))
            .width(Length::Fill)
            .height(Length::Fill)
            .padding(Padding::from(MenuLayout::PADDING as f32))
            .style(move |_: &iced_widget::Theme| container::Style {
                background: Some(view::to_iced(root_background).into()),
                // Same reasoning as `hyprforge-clipmenu::view`'s own
                // popup border: a layer-shell surface draws no window
                // decoration of its own, so the popup has to paint its
                // own edge to read as a floating surface at all.
                border: iced_runtime::core::Border {
                    radius: popup_radius.into(),
                    width: 1.0,
                    color: view::to_iced(popup_border),
                },
                ..Default::default()
            })
            .into()
    }

    /// Every row always "fits" — this popup never scrolls (see
    /// `layout.rs`'s own module doc), so this is simply how many rows
    /// there are; nothing here windows them the way a clipboard history
    /// or an emoji grid does.
    fn rows_that_fit(&self, _theme: &Theme, _height: f64) -> usize {
        self.rows.len()
    }

    fn pointer_move(&mut self, _theme: &Theme, position: (f64, f64)) -> bool {
        let hit = self.layout.row_at(&self.rows, position.1);
        let changed = hit != self.hovered;
        self.hovered = hit;
        changed && hit.is_some()
    }

    fn pointer_click(&mut self, _theme: &Theme, _width: f64, position: (f64, f64)) -> Option<Self::Outcome> {
        let index = self.layout.row_at(&self.rows, position.1)?;
        self.choose(index, false)
    }

    fn pointer_scroll(&mut self, _rows: i32) {
        // Nothing to scroll — see `layout.rs`'s own module doc.
    }

    fn key(&mut self, keysym: Keysym, _utf8: Option<String>, _modifiers: Modifiers) -> Option<Self::Outcome> {
        match keysym {
            Keysym::Escape => Some(MenuOutcome::Cancelled),
            Keysym::Return | Keysym::KP_Enter => {
                let index = self.hovered.filter(|&index| MenuLayout::is_clickable(&self.rows[index]))?;
                self.choose(index, true)
            }
            // Into a submenu from the row that opens it, and back out.
            Keysym::Right => {
                let index = self.hovered?;
                match self.shown.get(index) {
                    Some(&Shown::Item(flat)) if self.levels.opens(flat) && self.rows[index].enabled => self.choose(index, true),
                    _ => None,
                }
            }
            Keysym::Left | Keysym::BackSpace => {
                if self.level.is_some() {
                    self.open(self.levels.up(self.level), true);
                }
                None
            }
            Keysym::Up => {
                self.hovered = previous_clickable(&self.rows, self.hovered);
                None
            }
            Keysym::Down => {
                self.hovered = next_clickable(&self.rows, self.hovered);
                None
            }
            _ => None,
        }
    }

    fn needs_finish(_outcome: Self::Outcome) -> bool {
        // Nothing to synthesize after teardown — the daemon that spawned
        // this popup is the one performing the chosen action, not this
        // process. See `main.rs`'s own doc for the whole division.
        false
    }

    fn finish(&mut self, _outcome: Self::Outcome) {}
}

fn next_clickable(rows: &[MenuItem], from: Option<usize>) -> Option<usize> {
    let start = from.map(|i| i + 1).unwrap_or(0);
    (start..rows.len()).find(|&i| MenuLayout::is_clickable(&rows[i])).or_else(|| {
        // Wrap: nothing past `start` was clickable, so try from the top
        // instead of leaving the selection stuck at the last item.
        (0..start).find(|&i| MenuLayout::is_clickable(&rows[i]))
    })
}

fn previous_clickable(rows: &[MenuItem], from: Option<usize>) -> Option<usize> {
    let start = from.unwrap_or(rows.len());
    (0..start).rev().find(|&i| MenuLayout::is_clickable(&rows[i])).or_else(|| {
        (start..rows.len()).rev().find(|&i| MenuLayout::is_clickable(&rows[i]))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyprforge_tray::menu::Menu;

    fn menu() -> Menu {
        Menu::new(vec![
            MenuItem::checkmark("Wi-Fi", true, "wifi:radio:off"),
            MenuItem::separator(),
            MenuItem::standard("home", "wifi:connect:home"),
            MenuItem::disabled("guest (enterprise)"),
        ])
    }

    fn rows() -> Vec<MenuItem> {
        menu().items
    }

    fn app() -> TrayMenuApp {
        TrayMenuApp::new(Levels::new(&menu()), MenuLayout::for_font_size(15.0), Dismissal::CloseOnFocusLoss)
    }

    /// A menu with a submenu: a click on its row opens it rather than
    /// ending, its children are chosen by their place in the whole tree,
    /// and Back — or Left — returns to the top.
    #[test]
    fn a_submenu_opens_in_place_and_back_returns() {
        let mut more = MenuItem::standard("More", "unused");
        more.action = None;
        more.children = vec![MenuItem::standard("Deep", "deep")];
        let menu = Menu::new(vec![MenuItem::standard("Top", "top"), more]);
        let layout = MenuLayout::for_font_size(15.0);
        let mut app = TrayMenuApp::new(Levels::new(&menu), layout, Dismissal::CloseOnFocusLoss);
        let row = |app: &TrayMenuApp, i| layout.padding + layout.row_top(&app.rows, i) + 2.0;

        let more_at = row(&app, 1);
        assert_eq!(app.pointer_click(&Theme::default(), 220.0, (10.0, more_at)), None, "opens, doesn't end");
        assert_eq!(app.rows[0].label, crate::levels::BACK);
        let deep_at = row(&app, 1);
        assert_eq!(app.pointer_click(&Theme::default(), 220.0, (10.0, deep_at)), Some(MenuOutcome::Chosen(2)));

        let back_at = row(&app, 0);
        assert_eq!(app.pointer_click(&Theme::default(), 220.0, (10.0, back_at)), None);
        assert_eq!(app.rows[0].label, "Top", "back at the top");

        app.hovered = Some(1);
        assert_eq!(app.key(Keysym::Right, None, Modifiers::default()), None);
        assert_eq!(app.hovered, Some(0), "the keyboard lands on the level's first row");
        assert_eq!(app.key(Keysym::Left, None, Modifiers::default()), None);
        assert_eq!(app.rows[0].label, "Top");
    }

    #[test]
    fn a_click_on_a_separator_resolves_to_nothing_rather_than_the_nearest_row() {
        let layout = MenuLayout::for_font_size(15.0);
        let mut app = app();
        let sep_top = layout.padding + layout.row_top(&rows(), 1);
        assert_eq!(
            app.pointer_click(&Theme::default(), 220.0, (10.0, sep_top + 2.0)),
            None
        );
    }

    #[test]
    fn a_click_on_an_enabled_row_chooses_its_index() {
        let layout = MenuLayout::for_font_size(15.0);
        let mut app = app();
        let top = layout.padding + layout.row_top(&rows(), 2);
        assert_eq!(
            app.pointer_click(&Theme::default(), 220.0, (10.0, top + 2.0)),
            Some(MenuOutcome::Chosen(2))
        );
    }

    #[test]
    fn escape_cancels_regardless_of_hover_state() {
        let mut app = app();
        assert_eq!(app.key(Keysym::Escape, None, Modifiers::default()), Some(MenuOutcome::Cancelled));
    }

    #[test]
    fn down_then_enter_chooses_the_first_clickable_row_skipping_the_separator() {
        let mut app = app();
        assert_eq!(app.key(Keysym::Down, None, Modifiers::default()), None);
        assert_eq!(app.hovered, Some(0), "row 0 is the first clickable row");
        assert_eq!(app.key(Keysym::Return, None, Modifiers::default()), Some(MenuOutcome::Chosen(0)));
    }

    #[test]
    fn down_navigation_skips_the_separator_and_the_disabled_row() {
        let mut app = app();
        app.hovered = Some(0);
        assert_eq!(app.key(Keysym::Down, None, Modifiers::default()), None);
        assert_eq!(app.hovered, Some(2), "index 1 is a separator, so this must land on 2");
    }

    #[test]
    fn enter_with_nothing_hovered_does_nothing() {
        let mut app = app();
        assert_eq!(app.key(Keysym::Return, None, Modifiers::default()), None);
    }
}
