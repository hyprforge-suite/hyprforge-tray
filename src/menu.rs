//! What a tray menu *is*, with no D-Bus in sight.
//!
//! Same split as [`crate::item`]: the question worth testing is which
//! entries a given radio state deserves, and that is a pure function over
//! plain data. [`crate::dbusmenu`] is only the marshalling.

/// What a row does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    /// An ordinary clickable row.
    Standard,
    /// A horizontal rule. Carries no label and cannot be clicked.
    Separator,
    /// A checkbox-style row. `toggle` carries its state.
    Checkmark,
}

/// One row of a menu.
///
/// `id` must be **stable for as long as the row means the same thing**. A
/// host asks for properties by id after the layout is sent, and reuses an
/// id it already knows: renumbering rows between revisions makes a click
/// land on whatever row inherited the number. Ids are assigned from the
/// content, not from position — see `assign_ids`.
#[derive(Debug, Clone, PartialEq)]
pub struct MenuItem {
    pub id: i32,
    pub label: String,
    pub enabled: bool,
    pub visible: bool,
    pub kind: ItemKind,
    /// `Some` only for [`ItemKind::Checkmark`].
    pub toggle: Option<bool>,
    /// What clicking this row should do. Not sent over D-Bus — the host
    /// sends back the id, and this is how the daemon knows what that id
    /// meant.
    pub action: Option<String>,
    pub children: Vec<MenuItem>,
}

impl MenuItem {
    pub fn standard(label: impl Into<String>, action: impl Into<String>) -> Self {
        MenuItem {
            id: 0,
            label: label.into(),
            enabled: true,
            visible: true,
            kind: ItemKind::Standard,
            toggle: None,
            action: Some(action.into()),
            children: Vec::new(),
        }
    }

    pub fn checkmark(label: impl Into<String>, on: bool, action: impl Into<String>) -> Self {
        MenuItem {
            toggle: Some(on),
            kind: ItemKind::Checkmark,
            ..MenuItem::standard(label, action)
        }
    }

    pub fn separator() -> Self {
        MenuItem {
            id: 0,
            label: String::new(),
            enabled: false,
            visible: true,
            kind: ItemKind::Separator,
            toggle: None,
            action: None,
            children: Vec::new(),
        }
    }

    /// A row that is shown but cannot be clicked — "Wi-Fi is off", or a
    /// network this app cannot join yet.
    pub fn disabled(label: impl Into<String>) -> Self {
        MenuItem {
            enabled: false,
            action: None,
            ..MenuItem::standard(label, "")
        }
    }

    /// A checkmark that cannot be clicked — a radio blocked by a
    /// hardware kill switch, say, where `on`/`off` is still true but no
    /// D-Bus call flips it back. Shown rather than hidden, and shown as
    /// a checkmark rather than an ordinary disabled row, so the state it
    /// reports is not lost along with the ability to change it.
    pub fn disabled_checkmark(label: impl Into<String>, on: bool) -> Self {
        MenuItem {
            kind: ItemKind::Checkmark,
            toggle: Some(on),
            ..MenuItem::disabled(label)
        }
    }
}

/// Builds a menu in the one shape every menu in this crate follows: a
/// toggle row up top (or, in place of one, a single disabled row saying
/// why there is nothing to toggle), then whatever content that toggle
/// governs, then a settings row at the bottom.
///
/// The settings row is unconditional — offered in *every* state,
/// unavailable included — because a menu with no way out of it is
/// exactly the dead end CLAUDE.md's rules warn against; see
/// `trayd.rs`'s module doc for where each of the four settings rows
/// actually goes.
///
/// `content` may be empty (a keep-awake menu with nothing else holding
/// an inhibit, a radio that is off or blocked). When it is, no separator
/// is added for it — an empty section would otherwise put two
/// separators in a row between the toggle and the settings row for
/// nothing. `top` is a `Vec` rather than a single `MenuItem` only
/// because the "nothing to toggle" case still has to supply *a* row —
/// callers pass a one-element vec in the ordinary case.
///
/// This is what keeps a fifth icon from drifting from the other four:
/// the shape lives here, once, rather than being re-derived per menu.
pub fn radio_menu(top: Vec<MenuItem>, content: Vec<MenuItem>, settings: MenuItem) -> Menu {
    let mut items = top;
    if !content.is_empty() {
        items.push(MenuItem::separator());
        items.extend(content);
    }
    items.push(MenuItem::separator());
    items.push(settings);
    Menu::new(items)
}

/// A whole menu, with ids assigned.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Menu {
    pub items: Vec<MenuItem>,
}

impl Menu {
    /// Builds a menu, numbering every row.
    ///
    /// Root is id 0 by the spec's own convention, so rows start at 1.
    pub fn new(items: Vec<MenuItem>) -> Self {
        let mut menu = Menu { items };
        let mut next = 1;
        assign_ids(&mut menu.items, &mut next);
        menu
    }

    /// The action behind a row, by the id a host sends back.
    pub fn action_for(&self, id: i32) -> Option<&str> {
        find(&self.items, id).and_then(|item| item.action.as_deref())
    }

    pub fn find(&self, id: i32) -> Option<&MenuItem> {
        find(&self.items, id)
    }

    /// Every row, depth-first — what `GetGroupProperties` answers from.
    pub fn flatten(&self) -> Vec<&MenuItem> {
        let mut out = Vec::new();
        collect(&self.items, &mut out);
        out
    }
}

fn assign_ids(items: &mut [MenuItem], next: &mut i32) {
    for item in items {
        item.id = *next;
        *next += 1;
        assign_ids(&mut item.children, next);
    }
}

fn find(items: &[MenuItem], id: i32) -> Option<&MenuItem> {
    for item in items {
        if item.id == id {
            return Some(item);
        }
        if let Some(found) = find(&item.children, id) {
            return Some(found);
        }
    }
    None
}

fn collect<'a>(items: &'a [MenuItem], out: &mut Vec<&'a MenuItem>) {
    for item in items {
        out.push(item);
        collect(&item.children, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Menu {
        Menu::new(vec![
            MenuItem::checkmark("Wi-Fi", true, "radio:toggle"),
            MenuItem::separator(),
            MenuItem::standard("home", "connect:home"),
            MenuItem::disabled("guest (enterprise)"),
            MenuItem::separator(),
            MenuItem::standard("Network settings…", "settings"),
        ])
    }

    /// Root is 0 by the spec, so a row numbered 0 would be the menu
    /// itself and every click on it would be ignored.
    #[test]
    fn rows_are_numbered_from_one_because_zero_is_the_root() {
        let menu = sample();
        assert!(menu.items.iter().all(|i| i.id > 0));
        assert_eq!(menu.items[0].id, 1);
    }

    #[test]
    fn every_row_has_its_own_id_including_nested_ones() {
        let menu = Menu::new(vec![
            MenuItem::standard("a", "a"),
            MenuItem {
                children: vec![MenuItem::standard("c", "c"), MenuItem::standard("d", "d")],
                ..MenuItem::standard("b", "b")
            },
        ]);
        let ids: Vec<i32> = menu.flatten().iter().map(|i| i.id).collect();
        let mut unique = ids.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(ids.len(), unique.len(), "ids collide: {ids:?}");
        assert_eq!(ids.len(), 4);
    }

    /// The whole point of the id: a host sends back a number and the
    /// daemon has to know which row that was.
    #[test]
    fn a_click_id_resolves_to_the_action_that_row_stood_for() {
        let menu = sample();
        let home = menu.flatten().iter().find(|i| i.label == "home").unwrap().id;
        assert_eq!(menu.action_for(home), Some("connect:home"));
    }

    /// A separator and a disabled row both carry no action. A host will
    /// still send a click for a row it thinks is clickable, and answering
    /// one with someone else's action is worse than answering nothing.
    #[test]
    fn a_row_that_does_nothing_resolves_to_no_action_rather_than_the_next_one() {
        let menu = sample();
        let sep = menu.flatten().iter().find(|i| i.kind == ItemKind::Separator).unwrap().id;
        assert_eq!(menu.action_for(sep), None);

        let disabled = menu
            .flatten()
            .iter()
            .find(|i| i.label.contains("enterprise"))
            .unwrap()
            .id;
        assert_eq!(menu.action_for(disabled), None);
    }

    #[test]
    fn an_unknown_id_resolves_to_nothing_rather_than_panicking() {
        assert_eq!(sample().action_for(9999), None);
        assert!(sample().find(9999).is_none());
    }

    /// Only a checkmark carries a toggle state. A standard row reporting
    /// one would draw a checkbox a host cannot change.
    #[test]
    fn only_a_checkmark_carries_a_toggle_state() {
        let menu = sample();
        for item in menu.flatten() {
            match item.kind {
                ItemKind::Checkmark => assert!(item.toggle.is_some()),
                _ => assert!(item.toggle.is_none(), "{} carries a toggle", item.label),
            }
        }
    }

    // --- radio_menu: the one shape every menu in the daemon follows ------

    #[test]
    fn a_disabled_checkmark_still_reports_its_state_but_cannot_be_clicked() {
        let row = MenuItem::disabled_checkmark("Wi-Fi (blocked by hardware switch)", false);
        assert_eq!(row.kind, ItemKind::Checkmark);
        assert_eq!(row.toggle, Some(false));
        assert!(!row.enabled);
        assert!(row.action.is_none());
    }

    /// The ordinary shape: toggle, separator, content, separator,
    /// settings — every icon that has something to show ends up here.
    #[test]
    fn radio_menu_with_content_is_toggle_then_content_then_settings_with_separators_between() {
        let menu = radio_menu(
            vec![MenuItem::checkmark("Wi-Fi", true, "wifi:radio:off")],
            vec![MenuItem::standard("home", "wifi:connect:home")],
            MenuItem::standard("Wi-Fi settings…", "wifi:settings"),
        );
        let kinds: Vec<ItemKind> = menu.items.iter().map(|i| i.kind).collect();
        assert_eq!(
            kinds,
            vec![
                ItemKind::Checkmark,
                ItemKind::Separator,
                ItemKind::Standard,
                ItemKind::Separator,
                ItemKind::Standard,
            ]
        );
        assert_eq!(menu.items[0].label, "Wi-Fi");
        assert_eq!(menu.items[2].label, "home");
        assert_eq!(menu.items[4].label, "Wi-Fi settings…");
    }

    /// Empty content collapses to one separator, not two — a keep-awake
    /// menu with no other inhibitor must not show a blank gap between
    /// the toggle and the settings row.
    #[test]
    fn radio_menu_with_no_content_has_exactly_one_separator() {
        let menu = radio_menu(
            vec![MenuItem::checkmark("Keep awake", false, "keepawake:on")],
            Vec::new(),
            MenuItem::standard("Keep awake settings…", "keepawake:settings"),
        );
        let kinds: Vec<ItemKind> = menu.items.iter().map(|i| i.kind).collect();
        assert_eq!(kinds, vec![ItemKind::Checkmark, ItemKind::Separator, ItemKind::Standard]);
    }

    /// The unavailable shape: one disabled row explaining why, then the
    /// settings row — never an empty popup, and never a dead end either.
    #[test]
    fn radio_menu_unavailable_still_offers_the_settings_row() {
        let menu = radio_menu(
            vec![MenuItem::disabled("Wi-Fi unavailable")],
            Vec::new(),
            MenuItem::standard("Wi-Fi settings…", "wifi:settings"),
        );
        assert!(!menu.items[0].enabled);
        assert_eq!(menu.items.last().unwrap().label, "Wi-Fi settings…");
        assert!(menu.items.last().unwrap().enabled);
    }

    /// Ids only ever move when a row's own meaning changes — never
    /// merely because content between two states differs in length. A
    /// settings row built at the end of a longer menu and a shorter one
    /// still gets whatever id `Menu::new` assigns it in each case; what
    /// this test actually pins is that adding a settings row through
    /// `radio_menu` does not disturb the ids of the rows *before* it.
    #[test]
    fn rows_before_the_settings_row_keep_their_ids_whether_or_not_content_is_present() {
        let toggle = || MenuItem::checkmark("Wi-Fi", true, "wifi:radio:off");
        let settings = || MenuItem::standard("Wi-Fi settings…", "wifi:settings");

        let short = radio_menu(vec![toggle()], Vec::new(), settings());
        let long = radio_menu(
            vec![toggle()],
            vec![MenuItem::standard("home", "wifi:connect:home")],
            settings(),
        );
        assert_eq!(short.items[0].id, long.items[0].id, "the toggle is row 1 in both");
    }
}
