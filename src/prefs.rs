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
    /// Logical pixels added to a click's own Y position before the tray
    /// menu opens there — see this module's own doc. `32` clears a
    /// waybar-height bar (that bar defaults to about 34px tall) without
    /// needing configuration on the common case; a taller or shorter bar
    /// is what this field is for.
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
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs {
            network: true,
            bluetooth: true,
            keep_awake: false,
            night_light: false,
            power: false,
            menu_y_offset: 32,
            menu_closes_on_click_outside: true,
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
    let text = toml::to_string_pretty(prefs).expect("Prefs is two bools and always serialises");
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
            menu_y_offset: 50,
            menu_closes_on_click_outside: false,
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
