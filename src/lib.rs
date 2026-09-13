//! Tray icons for the Hyprforge suite, over `org.kde.StatusNotifierItem`.
//!
//! A tray icon is a D-Bus object, not a toolkit widget. That is what makes
//! this possible inside a suite that forbids GTK and Qt everywhere: the
//! icon is a *name*, resolved by whichever bar is running against the
//! user's own icon theme, and this crate owns no pixels at all.
//!
//! The library half knows nothing about Wi-Fi or Bluetooth — it is the
//! protocol and the item model. `src/bin/trayd.rs` is what joins it to
//! `hyprforge-network` and `hyprforge-bluetooth`.
//!
//! # What this does not do any more: `com.canonical.dbusmenu`
//!
//! This crate used to serve a right-click menu as a second protocol,
//! `com.canonical.dbusmenu`, alongside `org.kde.StatusNotifierItem` — any
//! spec-compliant bar could draw it, styled and positioned however that
//! bar's own tray module chose to. It no longer does. The menu is now
//! drawn by `hyprforge-traymenu`, a sibling `PopupApp` binary this crate's
//! own `ItemInterface::context_menu` (in `sni.rs`) spawns directly and
//! hands the menu to over a pipe — see `launch`'s own module doc for the
//! whole sequence. `menu` and `item` are unchanged: they are still the
//! plain-data model of what a menu and an item are, with no D-Bus in
//! sight, and `hyprforge-traymenu` depends on this crate for exactly
//! those two modules.
//!
//! **The cost, stated plainly**: before this change, any tray host that
//! implements `com.canonical.dbusmenu` could show these menus, styled as
//! that host chose. After it, only `hyprforge-traymenu` can — a bar with
//! no Hyprforge installed sees an icon with no menu at all (its
//! `ContextMenu` fallback runs the icon's primary action instead, the
//! same as an item that never had a menu). That narrowing is deliberate:
//! it is what makes the menu themed like the rest of the suite, anchored
//! below the bar at the icon that was clicked, with a configurable
//! offset — none of which `com.canonical.dbusmenu` gives a daemon any
//! say over, since drawing and positioning are entirely the host's job
//! under that protocol.

pub mod item;
pub mod launch;
pub mod menu;
pub mod prefs;
pub mod sni;

pub use launch::OPENED_PREFIX;
pub use prefs::Prefs;
pub use item::{Category, Status, TrayItem};
pub use sni::{watcher_present, TrayError, TrayIcon};
