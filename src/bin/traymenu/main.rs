//! Draws `hyprforge-trayd`'s own right-click menu, themed like the rest
//! of the suite, and prints back whichever row's action was chosen.
//!
//! This is a per-invocation program, spawned by `hyprforge-trayd` itself
//! (`hyprforge_tray::launch::show`) — never by a keybind, and never run
//! standalone against a real menu. The shape is otherwise exactly
//! `hyprforge-clipmenu`'s: a short-lived process that cannot leak a stuck
//! layer surface, taking `KeyboardInteractivity::Exclusive` for exactly
//! as long as it needs to and no longer. See that crate's own `main.rs`
//! doc for the fuller case for a per-invocation popup over a daemon.
//!
//! # The daemon keeps the state; this only asks a human a question
//!
//! `hyprforge-trayd` stays the only thing that talks to NetworkManager,
//! BlueZ, logind and hyprsunset — this process never does, and does not
//! link `zbus` for that reason. The menu arrives as JSON on stdin (the
//! [`hyprforge_tray::menu::Menu`] the daemon already built for this poll
//! tick), and the chosen row's own `action` string — not an id, since
//! there is no dbusmenu id-indirection needed on a private pipe between
//! a daemon and the popup it just spawned — goes back out on stdout, one
//! line, nothing else. The daemon performs that action exactly as it
//! always has; this process never touches a radio, a device or a
//! systemd unit. That is the same "only writer" discipline `hyprforge-clipd`
//! keeps over the clipboard, applied to tray state instead.
//!
//! # Why this depends on `hyprforge-tray` at all
//!
//! Only for `menu::{Menu, MenuItem}` — see this crate's own `Cargo.toml`
//! for the build-graph cost that brings along (zbus, hyprforge-network,
//! hyprforge-bluetooth, …, none of it linked into anything this binary
//! actually calls). That was a real trade-off, made deliberately rather
//! than duplicating the type: two independently-maintained copies of
//! "what a menu row is" is exactly the drift CLAUDE.md's "one place to
//! fix a shared thing" rule exists to prevent, and `hyprforge-tray` is a
//! small, non-async, zbus-optional-in-spirit library crate even though
//! Cargo cannot express "only the Menu/MenuItem part, please".

mod app;
mod layout;
mod measure;
mod view;

use app::{MenuOutcome, TrayMenuApp};
use hyprforge_popup::geometry::{Point, Size};
use hyprforge_tray::menu::Menu;
use layout::MenuLayout;

/// The name this popup's single-instance lock is filed under — its own,
/// distinct from `hyprforge-clipmenu.lock` and `hyprforge-emojimenu.lock`,
/// so opening a tray menu is never refused because one of those (or a
/// second tray menu) happens to be open, and vice versa. See
/// `hyprforge_popup::singleton`'s own doc for why an `flock` rather than
/// a name match or a PID file.
const LOCK_NAME: &str = "hyprforge-traymenu.lock";


/// Reads `--x <n> --y <n>` off argv — the icon's own global logical
/// position, exactly as the host handed it to `ContextMenu`, with no
/// `Prefs::menu_y_offset` applied to it any more (see `place_below_bar`'s
/// own doc for why, and for why only `x` actually ends up used for
/// placement). Nothing else on the command line is recognised; the menu
/// itself only ever arrives on stdin.
fn parse_anchor(mut args: impl Iterator<Item = String>) -> Option<Point> {
    let mut x = None;
    let mut y = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--x" => x = args.next().and_then(|v| v.parse().ok()),
            "--y" => y = args.next().and_then(|v| v.parse().ok()),
            _ => {}
        }
    }
    Some(Point { x: x?, y: y? })
}

fn main() -> std::process::ExitCode {
    // Only one tray menu at a time — see `LOCK_NAME`'s own doc.
    let lock_path = hyprforge_popup::singleton::lock_path(LOCK_NAME);
    let _lock = match hyprforge_popup::singleton::acquire(&lock_path) {
        Ok(Some(lock)) => Some(lock),
        // Someone already has it. Not an error — exit quietly, printing
        // no action, which `hyprforge-trayd` reads as "nothing was
        // chosen". This is the backstop, not the normal path: the daemon
        // closes the menu it already opened before spawning another
        // (`hyprforge_tray::launch::OpenMenu`), precisely because
        // refusing here is invisible to a user who right-clicked a
        // *different* icon and is waiting for its menu. What is left for
        // this branch is a copy started by something other than that
        // daemon — by hand, or a second daemon.
        Ok(None) => return std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("couldn't set up the single-instance lock ({e}) — continuing anyway");
            None
        }
    };

    let Some(anchor) = parse_anchor(std::env::args().skip(1)) else {
        eprintln!("usage: hyprforge-traymenu --x <n> --y <n> (menu JSON on stdin)");
        return std::process::ExitCode::FAILURE;
    };

    let mut input = String::new();
    {
        use std::io::Read;
        if let Err(e) = std::io::stdin().read_to_string(&mut input) {
            eprintln!("couldn't read the menu from stdin: {e}");
            return std::process::ExitCode::FAILURE;
        }
    }
    let menu: Menu = match serde_json::from_str(&input) {
        Ok(menu) => menu,
        Err(e) => {
            eprintln!("couldn't parse the menu handed to this popup: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    // Depth-first, the same order `com.canonical.dbusmenu`'s own
    // `GetLayout` used to flatten a tree into — `layout.rs` and
    // `view.rs` both walk this same `Vec` in order, so a row's index
    // means the same thing everywhere it is used.
    let mut rows: Vec<_> = menu.flatten().into_iter().cloned().collect();

    let mut theme = hyprforge_appearance::look::resolve();
    theme.font_size = theme.drawable_font_size();
    let row_layout = MenuLayout::for_font_size(theme.font_size);
    let popup_height = row_layout.popup_height(&rows);

    // As wide as the widest label, within `MenuLayout`'s bounds — then
    // anything wider than that is cut to fit, measured, with an
    // ellipsis. A separator's label is empty and measures nothing.
    let widest = rows.iter().map(|r| measure::width(&r.label, theme.font_size)).fold(0.0, f64::max);
    let popup_width = row_layout.popup_width(widest);
    let available = row_layout.label_width(popup_width);
    for row in &mut rows {
        if let std::borrow::Cow::Owned(cut) = measure::fit(&row.label, theme.font_size, available) {
            row.label = cut;
        }
    }

    let monitors = hyprforge_popup::monitors();
    if monitors.is_empty() {
        eprintln!("couldn't read any monitors from hyprctl — is Hyprland running?");
        return std::process::ExitCode::FAILURE;
    }
    let popup_size = Size { width: popup_width, height: popup_height };
    // Never collapsed with "menu_y_offset is unreadable" turning into
    // silence — an unreadable `tray.toml` is already warned about by
    // `hyprforge-trayd`'s own poll loop every time it changes; this is
    // just the one other place that needs a number out of it, and the
    // default is the least surprising thing to use rather than refusing
    // to open the menu at all.
    let prefs = match hyprforge_tray::prefs::load() {
        Ok(prefs) => prefs,
        Err(e) => {
            eprintln!("couldn't read tray.toml ({e}) — using the defaults for this menu");
            hyprforge_tray::prefs::Prefs::default()
        }
    };
    let menu_y_offset = prefs.menu_y_offset;
    let dismissal = if prefs.menu_closes_on_click_outside {
        hyprforge_popup::Dismissal::CloseOnFocusLoss
    } else {
        hyprforge_popup::Dismissal::HoldKeyboard
    };
    // Anchored to the bar's own reserved area on Y (so the popup lands
    // in the same place every time, regardless of where on the icon the
    // click landed) and to the click on X (so it still visibly belongs
    // to the icon that opened it) — see
    // `hyprforge_popup::placement::place_below_bar`'s own doc for why
    // this replaced `place` here, and for how the click coordinates
    // `anchor` carries were confirmed to already be logical.
    let Some(placement) = hyprforge_popup::place_below_bar(&monitors, anchor, menu_y_offset, popup_size) else {
        eprintln!("couldn't work out where to place the menu");
        return std::process::ExitCode::FAILURE;
    };

    let connection = match hyprforge_popup::Connection::connect_to_env() {
        Ok(connection) => connection,
        Err(e) => {
            eprintln!("couldn't connect to the compositor: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    let app = TrayMenuApp::new(rows.clone(), row_layout, dismissal);

    match hyprforge_popup::Popup::run(connection, placement, app, theme) {
        Ok(hyprforge_popup::Outcome::App(MenuOutcome::Chosen(index))) => {
            // The one line `hyprforge_tray::launch::show` reads back —
            // no action at all (an id past the end, in principle
            // unreachable since `pointer_click`/`key` only ever return an
            // index `layout::MenuLayout` itself resolved) prints nothing,
            // which the daemon already treats as "nothing was chosen".
            if let Some(action) = rows.get(index).and_then(|row| row.action.as_deref()) {
                println!("{action}");
            }
            std::process::ExitCode::SUCCESS
        }
        Ok(hyprforge_popup::Outcome::App(MenuOutcome::Cancelled)) => std::process::ExitCode::SUCCESS,
        // Closed by the compositor (an output unplugged, say) or a dead
        // connection: nothing was chosen, which is not this popup's
        // failure to report as one — the daemon's own action pipeline
        // already treats empty stdout as "the user chose nothing".
        Ok(hyprforge_popup::Outcome::Closed | hyprforge_popup::Outcome::Disconnected) => {
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{e}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> impl Iterator<Item = String> + '_ {
        s.split_whitespace().map(str::to_string)
    }

    #[test]
    fn both_flags_are_required_in_either_order() {
        assert_eq!(parse_anchor(args("--x 10 --y 20")), Some(Point { x: 10.0, y: 20.0 }));
        assert_eq!(parse_anchor(args("--y 20 --x 10")), Some(Point { x: 10.0, y: 20.0 }));
        assert_eq!(parse_anchor(args("--x 10")), None, "missing --y");
        assert_eq!(parse_anchor(args("--y 20")), None, "missing --x");
        assert_eq!(parse_anchor(args("")), None);
    }

    #[test]
    fn an_unparsable_number_is_treated_as_missing_rather_than_panicking() {
        assert_eq!(parse_anchor(args("--x not-a-number --y 20")), None);
    }

    #[test]
    fn a_non_finite_font_size_falls_back_rather_than_panicking_the_renderer() {
        let theme = hyprforge_look::Theme { font_size: f32::NAN, ..hyprforge_look::Theme::default() };
        assert_eq!(theme.drawable_font_size(), 15.0);
        let theme = hyprforge_look::Theme { font_size: 0.0, ..hyprforge_look::Theme::default() };
        assert!(theme.drawable_font_size() >= 6.0);
    }
}
