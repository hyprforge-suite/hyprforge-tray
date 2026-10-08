//! Tray icons for the Hyprforge suite, over `org.kde.StatusNotifierItem`.
//!
//! A tray icon is a D-Bus object, not a toolkit widget. That is what makes
//! this possible inside a suite that forbids GTK and Qt everywhere: the
//! icon is a *name*, resolved by whichever bar is running against the
//! user's own icon theme, and this crate owns no pixels at all.
//!
//! The library half knows nothing about Wi-Fi or Bluetooth — it is the
//! protocol and the item model. `src/bin/trayd.rs` is what joins it to
//! `hyprforge-network`, `hyprforge-bluetooth`, `hyprforge-power`,
//! hyprsunset and `hyprforge-displayd` — see its own module doc.
//!
//! # Two ways to serve a right-click menu
//!
//! The menu is drawn by `hyprforge-traymenu`, a sibling `PopupApp`
//! binary that this crate's `ItemInterface::context_menu` (in `sni.rs`)
//! spawns and hands the menu to over a pipe — see `launch`'s own module
//! doc for the whole sequence. That is what makes the menu themed like
//! the rest of the suite, anchored below the bar at the icon that was
//! clicked, with a configurable offset — none of which
//! `com.canonical.dbusmenu` gives a daemon any say over, since drawing
//! and positioning are entirely the host's job under that protocol.
//!
//! The popup needs Hyprland (it reads the monitors through `hyprctl`) and
//! a bar that calls `ContextMenu` on an item that declares no `Menu`
//! property. Where either is missing, this crate serves
//! `com.canonical.dbusmenu` instead ([`dbusmenu`]), and the bar draws the
//! menu itself, styled and placed however its tray module chooses. Which
//! one is a `tray.toml` choice, `auto` by default — see
//! [`prefs::MenuMode`]. An earlier version served only the popup, so a bar
//! on another compositor showed icons with no menu at all; every
//! component of this suite has to work with its siblings, and its
//! compositor, absent.
//!
//! `menu` and `item` are the plain-data model of what a menu and an item
//! are, with no D-Bus in sight. Both paths serve the same `Menu`, with the
//! same row ids and the same action strings, and `hyprforge-traymenu`
//! depends on this crate for exactly those two modules.

pub mod dbusmenu;
pub mod item;
pub mod launch;
pub mod menu;
pub mod prefs;
pub mod sni;

pub use launch::OPENED_PREFIX;
pub use prefs::{MenuMode, MenuServing, Prefs};
pub use item::{Category, Status, TrayItem};
pub use sni::{watcher_present, TrayError, TrayIcon};
