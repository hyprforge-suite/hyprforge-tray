//! Do this daemon's icon names resolve against a **real** icon theme?
//!
//! The unit tests check each name against an allow-list of names this
//! project believes are standard. That catches a typo and nothing else:
//! a name can be perfectly standard, perfectly spelled, and still absent
//! from the theme the user actually has.
//!
//! When that happens the bar draws a blank gap. No error is raised at any
//! layer — not by the daemon, not by the host, not by the icon loader —
//! so the only way to find out is to resolve the names against the themes
//! installed on a real machine. That is what this does.
//!
//! It found four: GNOME's `night-light-symbolic` and the `dialog-*-symbolic`
//! fallbacks exist only in Adwaita, and `bluetooth-disabled` is shipped by
//! neither Adwaita nor Breeze. On a Breeze-derived theme all four were
//! invisible, one of them in code that had already shipped.
//!
//! A missing name is not the only way this goes wrong. Wi-Fi once asked
//! for a full-colour name while Bluetooth, keep awake and night light all
//! asked for `-symbolic` ones — every one of those names resolved, so the
//! test above passed throughout, and the bar still drew one colourful
//! icon beside three monochrome ones. `every_icon_this_daemon_can_emit_is_
//! symbolic_unless_it_is_a_shared_fallback` catches that class without
//! needing a live theme at all, and
//! `every_symbolic_icon_this_daemon_can_emit_resolves_in_the_same_
//! installed_theme` goes one step further live: two names can each
//! resolve and still fall through inheritance to different actual
//! themes, which is the same mismatch by a subtler route.
//!
//! Read-only: it looks at files, and touches nothing.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

const SKIP_MARKER: &str = "HYPRFORGE-SKIP:";

/// Every icon name `hyprforge-trayd` can put on the bus.
///
/// Kept here by hand rather than imported, on purpose: the binary's
/// allow-lists are `const`s inside its own `mod tests`, and a test that
/// reads the same list the code reads proves only that a list equals
/// itself. This is the second opinion.
const EVERY_ICON: &[&str] = &[
    // Network
    "network-wireless-signal-excellent-symbolic",
    "network-wireless-signal-good-symbolic",
    "network-wireless-signal-ok-symbolic",
    "network-wireless-signal-weak-symbolic",
    "network-wireless-signal-none-symbolic",
    "network-wireless-disconnected-symbolic",
    // Bluetooth. `network-bluetooth-symbolic` (on, nothing connected)
    // was missing from this list while the daemon emitted it, so the
    // one Bluetooth state people see most was never checked here.
    "network-bluetooth-activated-symbolic",
    "network-bluetooth-symbolic",
    "network-bluetooth-inactive-symbolic",
    // Keep awake
    "changes-prevent-symbolic",
    "changes-allow-symbolic",
    // Night light
    "redshift-status-on-symbolic",
    "redshift-status-off-symbolic",
    // Power: the battery at every step Breeze draws, charged, and the
    // three profiles for a machine with no battery.
    "battery-000-symbolic",
    "battery-000-charging-symbolic",
    "battery-010-symbolic",
    "battery-010-charging-symbolic",
    "battery-020-symbolic",
    "battery-020-charging-symbolic",
    "battery-030-symbolic",
    "battery-030-charging-symbolic",
    "battery-040-symbolic",
    "battery-040-charging-symbolic",
    "battery-050-symbolic",
    "battery-050-charging-symbolic",
    "battery-060-symbolic",
    "battery-060-charging-symbolic",
    "battery-070-symbolic",
    "battery-070-charging-symbolic",
    "battery-080-symbolic",
    "battery-080-charging-symbolic",
    "battery-090-symbolic",
    "battery-090-charging-symbolic",
    "battery-100-symbolic",
    "battery-100-charging-symbolic",
    "battery-full-charged-symbolic",
    "battery-profile-powersave-symbolic",
    "battery-profile-balanced-symbolic",
    "battery-profile-performance-symbolic",
    // Displays: normal, and a layout waiting to be kept.
    "monitor-symbolic",
    "preferences-desktop-display-randr-symbolic",
    // Shared fallbacks. These two are the one deliberate exception to
    // "every name is symbolic" (see `SHARED_FALLBACK_ICONS` below): they
    // are the identical error state on every item, not one item's
    // own look, and neither Breeze nor hicolor ships a `-symbolic`
    // variant of either that this machine's configured theme can reach.
    "dialog-warning",
    "dialog-information",
];

/// Names this daemon can emit that are *not* held to the "must be
/// symbolic" rule below.
///
/// Both are the shared "this backend isn't answering" fallback, used
/// identically across all four items — not a per-item style choice, so
/// one of them being non-symbolic is not the defect the owner reported
/// (one item's normal, everyday icon drawn in a different style from the
/// other three's). See the long comment beside `KEEP_AWAKE_ICON_NAMES` in
/// `trayd.rs` for why no symbolic replacement is reachable here: Breeze
/// and hicolor ship neither `dialog-warning-symbolic` nor
/// `dialog-information-symbolic`, and Adwaita — which does — is not in
/// this machine's actual inheritance chain.
const SHARED_FALLBACK_ICONS: &[&str] = &["dialog-warning", "dialog-information"];

/// The user's theme, from gsettings — the same source
/// `hyprforge-appearance` resolves the rest of the look from.
fn configured_theme() -> Option<String> {
    let out = std::process::Command::new("gsettings")
        .args(["get", "org.gnome.desktop.interface", "icon-theme"])
        .output()
        .ok()?;
    let name = String::from_utf8_lossy(&out.stdout)
        .trim()
        .trim_matches('\'')
        .to_string();
    (!name.is_empty()).then_some(name)
}

fn theme_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        roots.push(PathBuf::from(&home).join(".icons"));
        roots.push(PathBuf::from(&home).join(".local/share/icons"));
    }
    roots.push(PathBuf::from("/usr/share/icons"));
    roots
}

fn theme_dir(name: &str) -> Option<PathBuf> {
    theme_roots()
        .into_iter()
        .map(|r| r.join(name))
        .find(|p| p.is_dir())
}

/// A theme's `Inherits=` line, which is what makes a name resolvable
/// through a theme that does not carry it.
fn inherits(theme: &Path) -> Vec<String> {
    let Ok(index) = std::fs::read_to_string(theme.join("index.theme")) else {
        return Vec::new();
    };
    index
        .lines()
        .find_map(|l| l.strip_prefix("Inherits="))
        .map(|v| v.split(',').map(|s| s.trim().to_string()).collect())
        .unwrap_or_default()
}

/// Every icon basename reachable from `theme`, following inheritance.
///
/// Only themes that are actually installed count. A theme may inherit
/// from half a dozen it does not have — this machine's inherits six that
/// are absent — and a name that resolves only through a missing theme
/// does not resolve at all.
fn reachable_names(theme_name: &str) -> HashSet<String> {
    let mut names = HashSet::new();
    let mut queue = vec![theme_name.to_string()];
    let mut visited = HashSet::new();
    // hicolor is the end of every chain by specification.
    queue.push("hicolor".to_string());

    while let Some(name) = queue.pop() {
        if !visited.insert(name.clone()) {
            continue;
        }
        let Some(dir) = theme_dir(&name) else { continue };
        for entry in walkdir(&dir) {
            if let Some(stem) = entry.file_stem().and_then(|s| s.to_str()) {
                names.insert(stem.to_string());
            }
        }
        queue.extend(inherits(&dir));
    }
    names
}

fn walkdir(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out
}

/// Every icon basename `theme` itself carries — no inheritance, unlike
/// [`reachable_names`]. Used to say *which* installed theme actually
/// supplied a name, not just whether one somewhere in the chain did.
fn own_names(theme_name: &str) -> HashSet<String> {
    let Some(dir) = theme_dir(theme_name) else { return HashSet::new() };
    walkdir(&dir)
        .iter()
        .filter_map(|p| p.file_stem().and_then(|s| s.to_str()).map(str::to_string))
        .collect()
}

/// The installed themes `root` actually resolves through, in the order a
/// real icon loader would search them: `root` first, then each of its
/// `Inherits=` entries depth-first, then `hicolor` last regardless of
/// whether anything named it — a theme that is listed in `Inherits=` but
/// not installed (this machine's Dracula names six such themes) is
/// simply absent from this list, exactly as it is absent from lookup.
fn installed_theme_chain(root: &str) -> Vec<String> {
    fn visit(name: &str, order: &mut Vec<String>, visited: &mut HashSet<String>) {
        if !visited.insert(name.to_string()) {
            return;
        }
        let Some(dir) = theme_dir(name) else { return };
        order.push(name.to_string());
        for parent in inherits(&dir) {
            visit(&parent, order, visited);
        }
    }
    let mut order = Vec::new();
    let mut visited = HashSet::new();
    visit(root, &mut order, &mut visited);
    if !order.iter().any(|n| n == "hicolor") && theme_dir("hicolor").is_some() {
        order.push("hicolor".to_string());
    }
    order
}

/// The claim: every name this daemon can emit draws something.
#[test]
#[ignore]
fn every_icon_this_daemon_can_emit_resolves_in_the_configured_theme() {
    let Some(theme) = configured_theme() else {
        eprintln!("{SKIP_MARKER} no icon theme configured in gsettings to resolve against");
        return;
    };
    if theme_dir(&theme).is_none() {
        eprintln!("{SKIP_MARKER} the configured icon theme {theme:?} is not installed");
        return;
    }

    let reachable = reachable_names(&theme);
    if reachable.is_empty() {
        eprintln!("{SKIP_MARKER} {theme:?} resolved to no icons at all; nothing to check against");
        return;
    }

    let missing: Vec<&str> = EVERY_ICON
        .iter()
        .copied()
        .filter(|name| !reachable.contains(*name))
        .collect();

    println!(
        "{} icon names checked against {theme:?} ({} names reachable)",
        EVERY_ICON.len(),
        reachable.len()
    );
    assert!(
        missing.is_empty(),
        "these icon names do not resolve in {theme:?} and would each draw a blank gap \
         in the bar, with nothing logged anywhere: {missing:?}"
    );
}

/// The class of bug the owner actually reported: not a missing icon, but
/// a *mismatched* one. Wi-Fi asked for a full-colour name while the other
/// three items asked for `-symbolic` ones, so the bar drew one colourful
/// icon in an otherwise monochrome row — nothing failed to resolve, and
/// the allow-list test above would have passed throughout.
///
/// The rule this project settled on: every icon name is `-symbolic`,
/// except the two-item [`SHARED_FALLBACK_ICONS`] allow-list documented
/// above them, which is the identical "this backend isn't answering"
/// state on all four items rather than one item's everyday look. This
/// needs no live theme and no `#[ignore]` — it is a property of the
/// names themselves — so it runs on every `cargo test`, unlike the two
/// tests either side of it.
#[test]
fn every_icon_this_daemon_can_emit_is_symbolic_unless_it_is_a_shared_fallback() {
    let not_symbolic: Vec<&str> = EVERY_ICON
        .iter()
        .copied()
        .filter(|name| !SHARED_FALLBACK_ICONS.contains(name))
        .filter(|name| !name.ends_with("-symbolic"))
        .collect();
    assert!(
        not_symbolic.is_empty(),
        "these names break the one-family rule that fixed the mismatched Wi-Fi icon \
         (full-colour among otherwise-symbolic siblings): {not_symbolic:?}"
    );
}

/// Not just "does this name resolve somewhere in the chain" — which
/// theme actually supplied it, and is it the *same* theme for every
/// name. Two icons can each individually resolve — one falling through
/// three levels of inheritance to hicolor, its sibling drawn directly by
/// the configured theme — and still end up visually mismatched, because
/// nothing requires a theme's fallback art to match its own style.
///
/// This turned out to be checkable, not merely aspirational: on this
/// machine, Dracula's `Inherits=` names six themes that are not
/// installed, and `breeze-dark` — the one that is — happens to carry
/// every non-fallback name directly, with no fall-through to `breeze` or
/// `hicolor` needed at all. That is a real, verified property today, not
/// a coincidence this test assumes going forward: if a future icon
/// choice only resolves through a deeper fallback, this fails and says
/// which theme actually answered for it.
#[test]
#[ignore]
fn every_symbolic_icon_this_daemon_can_emit_resolves_in_the_same_installed_theme() {
    let Some(theme) = configured_theme() else {
        eprintln!("{SKIP_MARKER} no icon theme configured in gsettings to resolve against");
        return;
    };
    if theme_dir(&theme).is_none() {
        eprintln!("{SKIP_MARKER} the configured icon theme {theme:?} is not installed");
        return;
    }

    let chain = installed_theme_chain(&theme);
    if chain.is_empty() {
        eprintln!(
            "{SKIP_MARKER} none of {theme:?}'s inheritance chain is installed; nothing to check against"
        );
        return;
    }

    // For each name, the first theme in lookup order that actually
    // carries it — the same thing a real icon loader would pick.
    let mut answered_by: Vec<(&str, Option<&str>)> = Vec::new();
    let per_theme_names: Vec<(String, HashSet<String>)> =
        chain.iter().map(|t| (t.clone(), own_names(t))).collect();

    for &name in EVERY_ICON {
        if SHARED_FALLBACK_ICONS.contains(&name) {
            continue;
        }
        let found = per_theme_names
            .iter()
            .find(|(_, names)| names.contains(name))
            .map(|(t, _)| t.as_str());
        answered_by.push((name, found));
    }

    let unresolved: Vec<&str> = answered_by.iter().filter(|(_, t)| t.is_none()).map(|(n, _)| *n).collect();
    assert!(
        unresolved.is_empty(),
        "these names are not carried by any installed theme in {theme:?}'s own chain \
         ({chain:?}), which the allow-list test above would not catch on its own: {unresolved:?}"
    );

    let themes_used: HashSet<&str> = answered_by.iter().filter_map(|(_, t)| *t).collect();
    assert_eq!(
        themes_used.len(),
        1,
        "these icon names are meant to look like one family, but they resolve through \
         different themes in {theme:?}'s own inheritance chain, which is exactly how the \
         Wi-Fi icon ended up looking different from the other three: {answered_by:?}"
    );
}
