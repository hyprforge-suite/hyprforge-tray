//! Submenus, as levels of one popup.
//!
//! A row with children opens them in place: the popup shows that row's
//! children under a "Back" row, and Back (or Left, or Backspace) returns
//! to the level above. One layer surface, never a second one beside it —
//! a layer-shell popup has no parent to hang a cascading child from, and
//! a menu of half a dozen rows doesn't need one.
//!
//! Before this, the tree was flattened into one list: every child drawn
//! as an ordinary row under its parent, and the parent itself drawn as a
//! dead row with no action. It read as one long menu with stray headings
//! in it, and nothing said which rows belonged to which.
//!
//! Every row has a place in [`Levels::flat`], depth-first — the index the
//! popup's outcome carries, which `main` reads the chosen row's action
//! back out of.

use hyprforge_tray::menu::{ItemKind, Menu, MenuItem};

/// What a row of the level on screen is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shown {
    /// Back to the level above.
    Back,
    /// `flat[index]`.
    Item(usize),
}

/// The action a row opening a submenu carries while it is shown, so the
/// layout's "clickable" test — enabled and with an action — holds for
/// it. Never printed: choosing such a row opens it rather than ending.
pub const OPENS: &str = "\0submenu";

/// The label of the row that goes back up.
pub const BACK: &str = "\u{2039} Back";

/// The whole tree, flattened, with who holds whom.
pub struct Levels {
    /// Every row, depth-first, without its children.
    pub flat: Vec<MenuItem>,
    /// Each row's children, by index into `flat`.
    children: Vec<Vec<usize>>,
    parent: Vec<Option<usize>>,
    top: Vec<usize>,
}

impl Levels {
    pub fn new(menu: &Menu) -> Levels {
        let mut levels = Levels { flat: Vec::new(), children: Vec::new(), parent: Vec::new(), top: Vec::new() };
        levels.top = levels.add(&menu.items, None);
        levels
    }

    fn add(&mut self, items: &[MenuItem], parent: Option<usize>) -> Vec<usize> {
        let mut here = Vec::new();
        for item in items.iter().filter(|i| i.visible) {
            let index = self.flat.len();
            self.flat.push(MenuItem { children: Vec::new(), ..item.clone() });
            self.children.push(Vec::new());
            self.parent.push(parent);
            here.push(index);
            let kids = self.add(&item.children, Some(index));
            self.children[index] = kids;
        }
        here
    }

    /// Whether choosing `flat[index]` opens a level rather than ending.
    pub fn opens(&self, index: usize) -> bool {
        self.flat[index].kind != ItemKind::Separator && !self.children[index].is_empty()
    }

    /// The level above `level`: `None` is the top.
    pub fn up(&self, level: Option<usize>) -> Option<usize> {
        level.and_then(|l| self.parent[l])
    }

    /// Every level there is: the top, then each row that opens one.
    pub fn all(&self) -> impl Iterator<Item = Option<usize>> + '_ {
        std::iter::once(None).chain((0..self.flat.len()).filter(|&i| self.opens(i)).map(Some))
    }

    /// The rows of `level` as the popup draws them, and what each is.
    /// A submenu's level starts with Back; a row that opens a level
    /// carries [`OPENS`] so it can be chosen even though it has no action
    /// of its own.
    pub fn rows(&self, level: Option<usize>) -> (Vec<MenuItem>, Vec<Shown>) {
        let indices = match level {
            None => &self.top,
            Some(l) => &self.children[l],
        };
        let mut rows = Vec::new();
        let mut shown = Vec::new();
        if level.is_some() {
            rows.push(MenuItem::standard(BACK, OPENS));
            shown.push(Shown::Back);
        }
        for &i in indices {
            let mut row = self.flat[i].clone();
            if self.opens(i) && row.enabled {
                row.action = Some(OPENS.to_string());
            }
            rows.push(row);
            shown.push(Shown::Item(i));
        }
        (rows, shown)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn menu() -> Menu {
        let mut more = MenuItem::standard("More", "unused");
        more.action = None;
        more.children = vec![MenuItem::standard("Deep", "deep:action"), MenuItem::standard("Deeper", "deeper")];
        Menu::new(vec![MenuItem::standard("Top", "top"), MenuItem::separator(), more])
    }

    #[test]
    fn the_top_level_shows_a_submenu_as_one_row_that_opens_it() {
        let levels = Levels::new(&menu());
        let (rows, shown) = levels.rows(None);
        assert_eq!(rows.iter().map(|r| r.label.as_str()).collect::<Vec<_>>(), ["Top", "", "More"]);
        assert_eq!(rows[2].action.as_deref(), Some(OPENS), "chosen, it opens");
        assert_eq!(shown[2], Shown::Item(2));
        assert!(levels.opens(2) && !levels.opens(0));
    }

    /// A submenu's level is Back and its children, and each child keeps
    /// the index the outcome carries.
    #[test]
    fn a_submenu_level_is_back_then_its_children() {
        let levels = Levels::new(&menu());
        let (rows, shown) = levels.rows(Some(2));
        assert_eq!(rows.iter().map(|r| r.label.as_str()).collect::<Vec<_>>(), [BACK, "Deep", "Deeper"]);
        assert_eq!(shown, [Shown::Back, Shown::Item(3), Shown::Item(4)]);
        assert_eq!(levels.flat[3].action.as_deref(), Some("deep:action"));
        assert_eq!(levels.up(Some(2)), None);
    }

    #[test]
    fn every_level_is_listed_for_sizing_the_popup() {
        assert_eq!(Levels::new(&menu()).all().collect::<Vec<_>>(), [None, Some(2)]);
    }

    /// A hidden row is not a row, and neither is anything under it.
    #[test]
    fn hidden_rows_and_their_children_are_left_out() {
        let mut hidden = MenuItem::standard("Hidden", "x");
        hidden.visible = false;
        hidden.children = vec![MenuItem::standard("Under", "y")];
        let levels = Levels::new(&Menu::new(vec![MenuItem::standard("Shown", "z"), hidden]));
        assert_eq!(levels.flat.len(), 1);
    }
}
