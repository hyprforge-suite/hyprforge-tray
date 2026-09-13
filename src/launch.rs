//! Handing a right-click's menu to `hyprforge-traymenu` and reading back
//! what was chosen.
//!
//! This is the whole reason `hyprforge-tray` no longer serves
//! `com.canonical.dbusmenu` — see this crate's own module doc. The menu
//! this daemon already builds every poll tick is drawn by a *sibling
//! process* instead of by whatever bar is running, over the shortest-lived
//! IPC there is: a pipe to a child this process just spawned. The menu
//! goes out as JSON on that child's stdin; the chosen row's own
//! [`crate::menu::MenuItem::action`] string comes back on its stdout, one
//! line, nothing more — there is no id-indirection to resolve, because
//! `hyprforge-traymenu` is not a foreign dbusmenu client that could leak
//! that string anywhere this daemon doesn't already trust.
//!
//! Nothing here is awaited from a D-Bus method handler. `sni.rs`'s
//! `context_menu` spawns the whole sequence below on its own task and
//! returns immediately — the host that asked for the menu is blocked on
//! that method call returning, and `hyprforge-traymenu` can sit open for
//! as long as the user is looking at it. Awaiting it inline would make a
//! right click hang the bar for exactly that long, and some hosts time a
//! D-Bus call like that out.

use crate::menu::Menu;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc::UnboundedSender;

/// Prefix of the event sent just before a menu is handed to
/// `hyprforge-traymenu`; the item's own id follows, as
/// `menu:opened:hyprforge-network`.
///
/// The direct descendant of the old `com.canonical.dbusmenu` protocol's
/// `AboutToShow` — same purpose, same string, moved here because there is
/// no dbusmenu object left to carry it. `hyprforge-trayd`'s
/// `handle_menu_clicks` is what actually reads this prefix; see its own
/// doc for why the Wi-Fi item is the only one anything happens for.
pub const OPENED_PREFIX: &str = "menu:opened:";

/// The binary this crate looks for on `$PATH`. A parameter of
/// [`show_with_binary`] rather than a literal there, so a test can point
/// this whole sequence at something other than the real popup — see this
/// module's own tests.
const TRAYMENU_BINARY: &str = "hyprforge-traymenu";

/// What became of one attempt to show the menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchOutcome {
    /// The popup ran to completion (chosen, cancelled, or closed some
    /// other way) and, if it chose something, that action has already
    /// been sent to `events`.
    Spawned,
    /// `hyprforge-traymenu` is not on `$PATH` — the sibling-binary-absent
    /// case CLAUDE.md is explicit a daemon must survive: keep running,
    /// keep every other icon, and let the caller fall back to *something*
    /// rather than silently doing nothing. `sni.rs::context_menu` is the
    /// caller, and its fallback is the primary action — the same one a
    /// left click already runs.
    NotInstalled,
    /// The menu couldn't be serialised, the child couldn't be waited on,
    /// or its stdin couldn't be written. Rare, and not distinguished
    /// further: every one of them means no popup appeared, so the same
    /// fallback the caller uses for `NotInstalled` applies here too.
    Failed,
}

/// Hands `menu` to `hyprforge-traymenu`, positioned at `(x, y)` (already
/// including whatever `Prefs::menu_y_offset` the caller wanted applied —
/// this function knows nothing about `tray.toml`), and forwards whatever
/// action it printed back to `events`.
///
/// `id` is only ever used for the [`OPENED_PREFIX`] event, sent before
/// anything is spawned — the same point in the sequence the old
/// `about_to_show` fired at, so a Wi-Fi menu's scan-on-open behaviour
/// (`hyprforge-trayd`'s `scan_then_refresh`) still fires at the moment a
/// user is about to see the menu, not some other time.
pub async fn show(id: &str, menu: &Menu, x: i32, y: i32, events: &UnboundedSender<String>) -> LaunchOutcome {
    show_with_binary(TRAYMENU_BINARY, id, menu, x, y, events).await
}

/// [`show`], with the binary to run as a parameter — see
/// [`TRAYMENU_BINARY`]'s own doc for why.
pub async fn show_with_binary(
    binary: &str,
    id: &str,
    menu: &Menu,
    x: i32,
    y: i32,
    events: &UnboundedSender<String>,
) -> LaunchOutcome {
    // Sent unconditionally, before anything can fail below: a menu that
    // is about to be shown is about to be shown even if the popup then
    // turns out to be missing, and a Wi-Fi list that never refreshes
    // because the popup binary is absent is a worse failure than a scan
    // nobody's about to look at.
    let _ = events.send(format!("{OPENED_PREFIX}{id}"));

    let json = match serde_json::to_vec(menu) {
        Ok(json) => json,
        Err(e) => {
            // Never a menu label in this line — a serialisation failure
            // is a shape problem (a signature drift like the one
            // `dbusmenu.rs` used to warn about), not something with
            // content worth naming.
            tracing::warn!(error = %e, "couldn't serialise the tray menu; not showing it");
            return LaunchOutcome::Failed;
        }
    };

    let mut child = match tokio::process::Command::new(binary)
        .arg("--x")
        .arg(x.to_string())
        .arg("--y")
        .arg(y.to_string())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        // Same reasoning as `spawn_settings` in trayd.rs: a GUI's own
        // renderer chatter has no business in this daemon's log.
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::warn!(
                "hyprforge-traymenu is not installed; falling back to the icon's primary action"
            );
            return LaunchOutcome::NotInstalled;
        }
        Err(e) => {
            tracing::warn!(error = %e, "couldn't launch hyprforge-traymenu");
            return LaunchOutcome::Failed;
        }
    };

    if let Some(mut stdin) = child.stdin.take() {
        if let Err(e) = stdin.write_all(&json).await {
            tracing::warn!(error = %e, "couldn't hand the menu to hyprforge-traymenu");
        }
        // `stdin` drops here, closing the pipe — `hyprforge-traymenu`
        // reads to EOF, so this is what lets it stop waiting for more.
    }

    // No bound on this wait: the popup is waiting on a *person*, not on
    // another program that is supposed to answer quickly — see this
    // module's own doc for why that is not the same "never wait without
    // a bound" hazard `hyprforge_core::command::output` guards against.
    // This runs on its own spawned task (`sni.rs::context_menu`), never on
    // a D-Bus dispatch path, so however long a user takes costs nothing
    // this daemon's event loop needs.
    let output = match child.wait_with_output().await {
        Ok(output) => output,
        Err(e) => {
            tracing::warn!(error = %e, "hyprforge-traymenu did not exit cleanly");
            return LaunchOutcome::Failed;
        }
    };

    let action = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !action.is_empty() {
        let _ = events.send(action);
    }
    LaunchOutcome::Spawned
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::menu::MenuItem;

    fn menu() -> Menu {
        Menu::new(vec![MenuItem::standard("home", "wifi:connect:home")])
    }

    /// The fallback path a missing sibling binary has to take — CLAUDE.md
    /// is explicit that this must be a state with a message, never a
    /// crash and never silence.
    #[tokio::test]
    async fn a_missing_binary_is_reported_as_not_installed_rather_than_panicking() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let outcome =
            show_with_binary("hyprforge-traymenu-does-not-exist-xyz", "hyprforge-network", &menu(), 10, 20, &tx)
                .await;
        assert_eq!(outcome, LaunchOutcome::NotInstalled);
        // The opened event still fires even though nothing could be shown.
        assert_eq!(rx.recv().await.unwrap(), "menu:opened:hyprforge-network");
        assert!(rx.try_recv().is_err(), "no action was chosen, so nothing else should arrive");
    }

    /// The plumbing end to end, standing in a tiny throwaway script for
    /// `hyprforge-traymenu`: whatever a child prints on its stdout must
    /// reach `events` unchanged, and the JSON handed to its stdin must be
    /// exactly what was asked to be shown.
    ///
    /// Plain `cat` cannot play this part: [`show_with_binary`] always
    /// passes `--x`/`--y`, and GNU `cat` treats an unrecognised `--x` as
    /// an error and exits without ever reading stdin — silently, since
    /// this function nulls the child's stderr the same way
    /// `spawn_settings` does. That produced empty output rather than an
    /// echo, which is not a failure this function reports (an empty
    /// action is a legitimate "nothing was chosen") — it is instead a
    /// second `rx.recv().await` in *this test* blocking forever on a
    /// message that was never going to arrive. A `#!/bin/sh -c 'cat'`
    /// script ignores its own arguments and only ever touches stdin, so
    /// it echoes regardless of what `--x`/`--y` this test passes.
    #[tokio::test]
    async fn whatever_the_child_prints_on_stdout_reaches_the_events_channel() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("echo-stdin.sh");
        std::fs::write(&script, "#!/bin/sh\ncat\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let outcome = show_with_binary(script.to_str().unwrap(), "hyprforge-network", &menu(), 10, 20, &tx).await;
        assert_eq!(outcome, LaunchOutcome::Spawned);

        assert_eq!(rx.recv().await.unwrap(), "menu:opened:hyprforge-network");
        let echoed = rx.recv().await.unwrap();
        let restored: Menu = serde_json::from_str(&echoed).expect("the script must have echoed valid JSON back");
        assert_eq!(restored, menu(), "the menu handed to stdin must be exactly what was echoed back");
    }
}
