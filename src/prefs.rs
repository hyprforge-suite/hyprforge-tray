//! Which tray icons the user wants.
//!
//! Its own file rather than a section of something larger, because two
//! processes read it: the Settings app writes it, and `hyprforge-trayd`
//! re-reads it on every poll so a toggle takes effect without restarting
//! anything.
//!
//! Defaults are "show both" for the original two icons — a tray icon the
//! user never asked to hide is the reason they installed a tray daemon.
//!
//! `keep_awake` and `night_light` default to **off**, unlike `network`
//! and `bluetooth`. Those two were the whole reason this daemon exists;
//! a third and fourth icon appearing in someone's bar with no action of
//! theirs is a surprise these were not, and `#[serde(default)]` per
//! field is exactly what lets an old `tray.toml` that only ever named
//! `network`/`bluetooth` keep meaning the same thing after this file
//! grows two more fields.
//!
//! `menu_y_offset` is unrelated to which icons show: it is how far below
//! the bar's own reserved area `hyprforge-traymenu` opens the popup (see
//! `hyprforge_popup::place_below_bar`) — `hyprctl monitors -j`'s own
//! `reserved` array says how tall the *exclusive zone* is, but a bar can
//! reserve less than it visually occupies (padding, a border), so this
//! is the user's own answer to the rest of that gap. Read by
//! `hyprforge-traymenu` itself, fresh on every right click, the same way
//! every other field here is re-read on every poll tick — not by
//! `hyprforge-trayd`, even though this file lives in that daemon's own
//! crate: `ItemInterface::context_menu` (`sni.rs`) used to add this to
//! the click's own Y before spawning the popup, which made the popup's
//! position depend on where on the icon the click landed. Anchoring to
//! the bar instead of the pointer meant moving the read to whichever
//! process actually places the popup.
//!
//! `menu` is which of two ways a right click is served — see
//! [`MenuMode`]. The answer depends on the machine as well as the file,
//! so the file holds the user's *choice* and [`MenuMode::serving_here`]
//! turns it into what actually happens.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    pub network: bool,
    pub bluetooth: bool,
    pub keep_awake: bool,
    pub night_light: bool,
    /// Battery level and power profile, one icon — off by default for
    /// the reason `keep_awake` and `night_light` are.
    pub power: bool,
    /// Saved display layouts, and the keep-or-revert prompt after
    /// switching one. Off by default like the others added after the
    /// first two.
    pub displays: bool,
    /// Logical pixels below the bar's own reserved area at which the tray
    /// menu opens — see this module's own doc. (It was once added to the
    /// click's Y instead; that made the menu's position depend on where
    /// on the icon the click landed.) A bar that reserves less than it
    /// visually occupies is what this field is for.
    pub menu_y_offset: i32,
    /// Whether clicking outside an open tray menu dismisses it.
    ///
    /// Not a cosmetic preference: it decides how the popup asks the
    /// compositor for the keyboard, because those are the same question
    /// (see `hyprforge_popup::Dismissal`). `true` — the default, and how
    /// every other menu on the desktop behaves — means the menu takes
    /// the keyboard only while it is being used, so a click elsewhere
    /// both closes it *and* reaches whatever it landed on. `false` means
    /// the menu holds the keyboard until it is dismissed deliberately,
    /// which is what somebody driving it entirely from the keyboard
    /// would want, at the cost of a stray click doing nothing.
    pub menu_closes_on_click_outside: bool,
    /// How a right click is served: Hyprforge's own popup, or a
    /// `com.canonical.dbusmenu` menu the bar draws itself. See
    /// [`MenuMode`].
    pub menu: MenuMode,
}

/// Which way the right-click menus are served — the user's choice, as
/// `tray.toml` holds it.
///
/// The popup (`hyprforge-traymenu`) is the suite's own menu: themed like
/// the clipboard and emoji popups, anchored below the bar at the icon
/// that was clicked. It needs two things this daemon cannot assume. It
/// reads the monitors through `hyprctl`, so it only works on Hyprland.
/// And the bar has to call `ContextMenu` on an item that declares no
/// `Menu` property, which a bar that only draws dbusmenu never does —
/// on such a bar the popup is never even asked for.
///
/// `com.canonical.dbusmenu` is the fallback that works on any
/// spec-compliant bar and any compositor, drawn and placed however that
/// bar's tray module chooses. It is the menu this crate served before the
/// popup existed, and it comes back as a choice rather than the default
/// because the popup is the better menu wherever it can run.
///
/// `Auto`, the default, picks the popup where it can run and dbusmenu
/// everywhere else, so installing the tray on sway gives right-click
/// menus with nothing configured. `Popup` and `Dbusmenu` force one —
/// `Dbusmenu` is for a bar on Hyprland that never calls `ContextMenu`,
/// which nothing here can detect from the outside.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MenuMode {
    #[default]
    Auto,
    Popup,
    Dbusmenu,
}

/// What a right click is actually served by, once [`MenuMode::Auto`] has
/// been decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuServing {
    /// No `Menu` property; `ContextMenu` spawns `hyprforge-traymenu`.
    Popup,
    /// A `Menu` property naming a `com.canonical.dbusmenu` object.
    Dbusmenu,
}

impl MenuMode {
    /// The decision, as a pure function of what the machine offers.
    ///
    /// `on_hyprland` and `traymenu_installed` are passed in rather than
    /// looked up so every combination is testable without a compositor
    /// or a `$PATH` — [`Self::serving_here`] is the one place that asks.
    pub fn serving(self, on_hyprland: bool, traymenu_installed: bool) -> MenuServing {
        match self {
            MenuMode::Popup => MenuServing::Popup,
            MenuMode::Dbusmenu => MenuServing::Dbusmenu,
            MenuMode::Auto if on_hyprland && traymenu_installed => MenuServing::Popup,
            MenuMode::Auto => MenuServing::Dbusmenu,
        }
    }

    /// [`Self::serving`], asked of this machine.
    ///
    /// Hyprland sets `HYPRLAND_INSTANCE_SIGNATURE` for everything it
    /// starts, and `hyprctl` — which the popup needs — reads the same
    /// variable to find its socket, so its absence is exactly "the popup
    /// cannot place itself". Cheap enough for every poll tick: one
    /// environment read and a `stat` per `$PATH` entry.
    pub fn serving_here(self) -> MenuServing {
        let on_hyprland = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some_and(|v| !v.is_empty());
        self.serving(on_hyprland, on_path(crate::launch::TRAYMENU_BINARY))
    }
}

/// Whether `name` is an executable file in some `$PATH` directory — the
/// lookup `Command::new` will do when it is spawned.
fn on_path(name: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        std::fs::metadata(dir.join(name))
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    })
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs {
            network: true,
            bluetooth: true,
            keep_awake: false,
            night_light: false,
            power: false,
            displays: false,
            menu_y_offset: 32,
            menu_closes_on_click_outside: true,
            menu: MenuMode::Auto,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PrefsError {
    /// The file exists and will not parse. **Not** the same as absent —
    /// see [`load_from`].
    #[error("{path} could not be read as tray settings: {source}")]
    Unreadable {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("{path} could not be opened: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} could not be written: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Where it lives. `hyprforge-paths` owns this, the way it owns
/// `lock.toml` and `appearance.toml` — one crate knows the layout of the
/// config directory, and no other crate joins path segments to guess it.
pub fn path() -> PathBuf {
    hyprforge_paths::tray_toml_path()
}

pub fn load() -> Result<Prefs, PrefsError> {
    load_from(&path())
}

/// Reads the preferences, or says why it could not.
///
/// A **missing** file is first run and yields the defaults. A file that
/// **exists and will not parse** is an error the user has to hear about,
/// and must never be silently replaced with defaults — doing that would
/// turn a typo into "you have configured nothing" and then overwrite what
/// they wrote on the next save. This suite has paid for that mistake
/// once already, in `hlconfig::storage`, and the rule is in CLAUDE.md.
pub fn load_from(path: &Path) -> Result<Prefs, PrefsError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Prefs::default()),
        Err(source) => {
            return Err(PrefsError::Io {
                path: path.to_path_buf(),
                source,
            })
        }
    };
    toml::from_str(&text).map_err(|source| PrefsError::Unreadable {
        path: path.to_path_buf(),
        source,
    })
}

pub fn save(prefs: &Prefs) -> Result<(), PrefsError> {
    save_to(&path(), prefs)
}

pub fn save_to(path: &Path, prefs: &Prefs) -> Result<(), PrefsError> {
    let text = toml::to_string_pretty(prefs).expect("Prefs is plain fields and always serialises");
    hyprforge_paths::write_atomic(path, &text).map_err(|source| PrefsError::Write {
        path: path.to_path_buf(),
        source,
    })
}

/// Read-modify-write, and the one thing every writer of this file should
/// call instead of saving a copy it loaded earlier.
///
/// This file has more than one writer inside a single `hyprforge-settings`
/// process — the Network and Bluetooth screens each flip their own icon's
/// bit, and the Tray screen this function was written for flips all four
/// plus [`Prefs::menu_y_offset`] — and each of them loads `Prefs` once, at
/// its own construction. Two screens open at once (or one screen left
/// open while another is visited) hold two independently stale copies of
/// the same struct, and a save that writes back "the whole struct as I
/// last saw it" from either one **silently undoes** whatever the other
/// screen wrote in between. Reloading right before every write is the
/// only form of the save that is correct regardless of who else touched
/// the file since — another screen in this process, `hyprforge-trayd`
/// re-reading it (this crate never writes it), or the user's own editor.
///
/// It also composes with the rule in [`load_from`] for free: a `tray.toml`
/// that exists and will not parse makes the reload fail, which returns
/// here before `f` ever runs and before anything is written — the same
/// "refuse and report, never silently overwrite" this module already
/// promises for a plain load, now also true of every write.
pub fn update(f: impl FnOnce(&mut Prefs)) -> Result<Prefs, PrefsError> {
    update_at(&path(), f)
}

/// [`update`], against an arbitrary path — the seam a test uses to point
/// this at a throwaway `tray.toml` instead of the real one.
pub fn update_at(path: &Path, f: impl FnOnce(&mut Prefs)) -> Result<Prefs, PrefsError> {
    let mut prefs = load_from(path)?;
    f(&mut prefs);
    save_to(path, &prefs)?;
    Ok(prefs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_icons_are_shown_until_someone_says_otherwise() {
        let prefs = Prefs::default();
        assert!(prefs.network);
        assert!(prefs.bluetooth);
    }

    #[test]
    fn a_missing_file_is_first_run_not_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("tray.toml");
        assert_eq!(load_from(&missing).unwrap(), Prefs::default());
    }

    /// The rule this file is most likely to break. A tray.toml with a
    /// typo in it must be reported, not silently treated as "nothing
    /// configured" — because the next save would then overwrite what the
    /// user actually wrote.
    #[test]
    fn a_file_that_exists_and_will_not_parse_is_reported_rather_than_defaulted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tray.toml");
        std::fs::write(&path, "network = yes please\n").unwrap();

        let err = load_from(&path).expect_err("a malformed file is an error, not defaults");
        assert!(matches!(err, PrefsError::Unreadable { .. }));
        assert!(err.to_string().contains("tray.toml"));
    }

    /// A file naming only one icon leaves the other at its default,
    /// rather than switching it off — `#[serde(default)]` per field is
    /// what makes adding a third icon later not break existing files.
    #[test]
    fn a_partial_file_leaves_the_icons_it_does_not_mention_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tray.toml");
        std::fs::write(&path, "network = false\n").unwrap();

        let prefs = load_from(&path).unwrap();
        assert!(!prefs.network);
        assert!(prefs.bluetooth, "an unmentioned icon keeps its default");
    }

    #[test]
    fn what_is_saved_is_what_comes_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tray.toml");
        // `keep_awake`/`night_light` given explicitly (not `..Default::default()`)
        // so this test still proves a round trip of a value that differs
        // from the default in every field, not just the original two.
        let prefs = Prefs {
            network: false,
            bluetooth: true,
            keep_awake: true,
            night_light: true,
            power: true,
            displays: true,
            menu_y_offset: 50,
            menu_closes_on_click_outside: false,
            menu: MenuMode::Dbusmenu,
        };
        save_to(&path, &prefs).unwrap();
        assert_eq!(load_from(&path).unwrap(), prefs);
    }

    /// The two new icons are opt-in: unlike `network`/`bluetooth`, which
    /// default to shown, `keep_awake` and `night_light` default to
    /// hidden, so installing this daemon does not put an icon in the bar
    /// nobody asked for.
    #[test]
    fn the_two_new_icons_default_to_off_unlike_the_original_two() {
        let prefs = Prefs::default();
        assert!(prefs.network);
        assert!(prefs.bluetooth);
        assert!(!prefs.keep_awake);
        assert!(!prefs.night_light);
    }

    /// The property `#[serde(default)]` per field exists to guarantee:
    /// a `tray.toml` written before `keep_awake`/`night_light` existed —
    /// naming only the original two — must not be read as "hide
    /// everything else" or fail to parse. It leaves the new icons at
    /// their own default, off.
    #[test]
    fn a_tray_toml_naming_only_the_original_two_icons_leaves_the_new_ones_off() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tray.toml");
        std::fs::write(&path, "network = true\nbluetooth = false\n").unwrap();

        let prefs = load_from(&path).unwrap();
        assert!(prefs.network);
        assert!(!prefs.bluetooth);
        assert!(!prefs.keep_awake, "an icon added later than this file defaults off");
        assert!(!prefs.night_light, "an icon added later than this file defaults off");
        assert!(!prefs.power, "an icon added later than this file defaults off");
        assert!(!prefs.displays, "an icon added later than this file defaults off");
    }

    /// A `tray.toml` written before `menu_y_offset` existed must still
    /// parse and get the default — the same backward-compatibility
    /// property the two icon fields above already have to hold, now
    /// extended to a field that is not a bool.
    #[test]
    fn a_tray_toml_naming_no_offset_gets_the_default_that_clears_a_typical_bar() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tray.toml");
        std::fs::write(&path, "network = true\nbluetooth = true\n").unwrap();

        let prefs = load_from(&path).unwrap();
        assert_eq!(prefs.menu_y_offset, 32);
    }

    /// The same backward-compatibility property every field here has to
    /// hold, for the field most likely to be missing: a `tray.toml`
    /// written before clicking away meant anything must read as the
    /// behaviour every other menu on the desktop already has, rather
    /// than as "off".
    #[test]
    fn a_tray_toml_written_before_this_setting_existed_closes_on_a_click_outside() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tray.toml");
        std::fs::write(&path, "network = true\nmenu_y_offset = 40\n").unwrap();

        let prefs = load_from(&path).unwrap();
        assert_eq!(prefs.menu_y_offset, 40);
        assert!(prefs.menu_closes_on_click_outside, "a file that predates the field gets the default, not false");
    }

    #[test]
    fn a_configured_offset_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tray.toml");
        let prefs = Prefs { menu_y_offset: 48, ..Prefs::default() };
        save_to(&path, &prefs).unwrap();
        assert_eq!(load_from(&path).unwrap().menu_y_offset, 48);
    }

    // --- `update`: the read-modify-write every writer shares -------------

    /// The property this function exists to guarantee, pinned directly:
    /// two independent "screens" (here, just two closures) each holding
    /// nothing but the path, one flips `network` and the other flips
    /// `menu_y_offset`, in either order — both must survive. A plain
    /// load-mutate-save built on a copy loaded once, before either write,
    /// would let the second write silently erase the first.
    #[test]
    fn two_writers_changing_different_fields_both_survive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tray.toml");
        // Both "screens" load their own starting copy up front, the way
        // `NetworkModule::new`/`BluetoothModule::new`/`TrayModule::new`
        // each do in `hyprforge-settings` — before either has written
        // anything.
        let screen_a_initial = load_from(&path).unwrap();
        let screen_b_initial = load_from(&path).unwrap();
        assert_eq!(screen_a_initial, screen_b_initial, "both start from the same defaults");

        // Screen A flips its own icon off.
        update_at(&path, |p| p.network = false).unwrap();
        // Screen B, still only holding what it loaded before A's write,
        // flips a completely different field.
        update_at(&path, |p| p.menu_y_offset = 60).unwrap();

        let on_disk = load_from(&path).unwrap();
        assert!(!on_disk.network, "screen A's write must not be undone by screen B's");
        assert_eq!(on_disk.menu_y_offset, 60, "screen B's own write must have landed");
    }

    /// The composed half of the guarantee: a file that has gone bad
    /// between load and write must refuse the write entirely, the same
    /// as a plain load would refuse to hand back defaults for it.
    #[test]
    fn update_refuses_to_write_over_a_file_it_cannot_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tray.toml");
        std::fs::write(&path, "network = yes please\n").unwrap();

        let before = std::fs::read_to_string(&path).unwrap();
        let err = update_at(&path, |p| p.network = false)
            .expect_err("a file that won't parse must refuse the write, not overwrite it");
        assert!(matches!(err, PrefsError::Unreadable { .. }));
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(before, after, "the mutation must never have been applied or saved");
    }

    // --- `menu`: which way a right click is served ----------------------

    /// A `tray.toml` written before this field existed must keep the menu
    /// people already had where it can still run — `auto`, never a forced
    /// mode they did not choose.
    #[test]
    fn a_tray_toml_written_before_the_menu_mode_existed_reads_as_auto() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tray.toml");
        std::fs::write(&path, "network = true\nmenu_y_offset = 40\n").unwrap();
        assert_eq!(load_from(&path).unwrap().menu, MenuMode::Auto);
    }

    /// The spelling a person writes by hand, and the spelling this file
    /// writes, are the same lowercase words.
    #[test]
    fn the_menu_mode_is_written_and_read_as_a_lowercase_word() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tray.toml");
        std::fs::write(&path, "menu = \"dbusmenu\"\n").unwrap();
        assert_eq!(load_from(&path).unwrap().menu, MenuMode::Dbusmenu);

        save_to(&path, &Prefs { menu: MenuMode::Popup, ..Prefs::default() }).unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().contains("menu = \"popup\""));
    }

    /// A misspelt mode is a file that will not parse, reported like any
    /// other — not quietly read as `auto`.
    #[test]
    fn an_unknown_menu_mode_is_reported_rather_than_defaulted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tray.toml");
        std::fs::write(&path, "menu = \"gtk\"\n").unwrap();
        assert!(matches!(load_from(&path), Err(PrefsError::Unreadable { .. })));
    }

    /// `auto` picks the popup only where it can run: on Hyprland, with
    /// the binary installed. Anywhere else it is dbusmenu, so a tray on
    /// another compositor has right-click menus with nothing configured.
    #[test]
    fn auto_serves_the_popup_only_on_hyprland_with_traymenu_installed() {
        assert_eq!(MenuMode::Auto.serving(true, true), MenuServing::Popup);
        assert_eq!(MenuMode::Auto.serving(true, false), MenuServing::Dbusmenu);
        assert_eq!(MenuMode::Auto.serving(false, true), MenuServing::Dbusmenu);
        assert_eq!(MenuMode::Auto.serving(false, false), MenuServing::Dbusmenu);
    }

    /// A forced mode is what the user asked for, whatever the machine
    /// offers — `dbusmenu` exists for a bar on Hyprland that never calls
    /// `ContextMenu`, which looks identical from here to one that does.
    #[test]
    fn a_forced_mode_wins_over_what_the_machine_offers() {
        for (hyprland, installed) in [(true, true), (true, false), (false, true), (false, false)] {
            assert_eq!(MenuMode::Popup.serving(hyprland, installed), MenuServing::Popup);
            assert_eq!(MenuMode::Dbusmenu.serving(hyprland, installed), MenuServing::Dbusmenu);
        }
    }

    #[test]
    fn a_binary_counts_as_installed_only_when_it_is_an_executable_file_on_path() {
        assert!(on_path("sh"), "every machine this runs on has a shell on $PATH");
        assert!(!on_path("hyprforge-no-such-binary-anywhere"));
    }

    /// `update` on a first run (no file yet) still works, starting from
    /// the same defaults `load_from` would hand back.
    #[test]
    fn update_on_a_missing_file_starts_from_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tray.toml");
        let updated = update_at(&path, |p| p.keep_awake = true).unwrap();
        assert!(updated.keep_awake);
        assert!(updated.network, "everything else stays at its default");
        assert_eq!(load_from(&path).unwrap(), updated);
    }
}
