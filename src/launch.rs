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
use std::sync::LazyLock;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Child;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::Mutex;

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

/// The one tray menu that may be open at a time, and the handle that
/// closes it.
///
/// Right-clicking a second icon while the first icon's menu was still
/// open used to do nothing at all: `hyprforge-traymenu` takes an `flock`
/// single-instance lock (`hyprforge_popup::singleton`), the second copy
/// found it held and exited quietly and successfully. That is the right
/// answer for a keybind pressed twice — which is what the lock was
/// written for — and the wrong one for a click on a *different* icon,
/// where the user is asking for a different menu and gets no sign that
/// they asked for anything. It read as the tray being broken: press
/// escape, then right click, and only then does the other menu appear.
///
/// So this daemon closes the menu it opened before opening another. It
/// keeps the whole [`Child`] rather than just a pid, because killing is
/// only half of it: the `flock` is released when the kernel closes the
/// dead process's descriptors, and a zombie nobody has waited on still
/// holds them. [`Child::kill`] signals *and* reaps, so when it returns
/// the lock is genuinely free for the process about to ask for it.
/// Signalling a bare pid and trusting the owning task to reap it in
/// time is the version of this that races the new popup's own `acquire`.
#[derive(Default)]
pub struct OpenMenu(Mutex<Option<(u32, Child)>>);

/// The slot [`show`] uses. Process-wide because what it models is: there
/// is one `hyprforge-traymenu` lock per session, so there is one open
/// menu per daemon, whichever of the four items was clicked. It is a
/// parameter of [`show_with_binary`] rather than reached for directly,
/// so tests get their own slot instead of racing each other through
/// this one.
static OPEN_MENU: LazyLock<OpenMenu> = LazyLock::new(OpenMenu::default);

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

/// Hands `menu` to `hyprforge-traymenu`, at the icon's own `(x, y)` —
/// unmodified from whatever the host handed `ItemInterface::context_menu`,
/// since this function knows nothing about `tray.toml` and no longer
/// applies `Prefs::menu_y_offset` itself (the popup reads that, and
/// anchors its own Y to the bar's reserved area — see
/// `hyprforge_popup::place_below_bar`) — and forwards whatever action it
/// printed back to `events`.
///
/// `id` is only ever used for the [`OPENED_PREFIX`] event, sent before
/// anything is spawned — the same point in the sequence the old
/// `about_to_show` fired at, so a Wi-Fi menu's scan-on-open behaviour
/// (`hyprforge-trayd`'s `scan_then_refresh`) still fires at the moment a
/// user is about to see the menu, not some other time.
pub async fn show(id: &str, menu: &Menu, x: i32, y: i32, events: &UnboundedSender<String>) -> LaunchOutcome {
    show_with_binary(TRAYMENU_BINARY, id, menu, x, y, events, &OPEN_MENU).await
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
    open: &OpenMenu,
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

    // Close whatever is already open, and do it *before* spawning: the
    // new popup refuses to start while another copy holds the lock, so
    // the order here is the whole fix. Held across the spawn below too,
    // so two right clicks arriving together cannot both get past this
    // point and race each other for the lock.
    let mut guard = open.0.lock().await;
    if let Some((_, mut previous)) = guard.take() {
        // A kill, not a polite close: there is no protocol for asking
        // the popup to go away, and anything it printed on the way out
        // would arrive here as an action the user never chose.
        if let Err(e) = previous.kill().await {
            tracing::warn!(error = %e, "couldn't close the tray menu that was already open");
        }
    }

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

    // `stdout` is read here rather than through `wait_with_output`,
    // because the `Child` itself now has somewhere else to be: the slot,
    // where the *next* right click can reach it. Reading the pipe to EOF
    // waits for the same thing waiting on the child would — the pipe
    // closes when the process exits, whether it chose a row, was
    // cancelled, or was killed by a later click replacing it.
    let mut stdout = child.stdout.take();
    let tracked = child.id();
    match tracked {
        Some(pid) => *guard = Some((pid, child)),
        // Only reachable for a child that has already been waited on,
        // which cannot have happened one statement after spawning it.
        // Handled rather than unwrapped, and the cost of being wrong is
        // one menu that a later right click replaces the slow way (the
        // user pressing escape) rather than a panic in a daemon.
        None => tracing::warn!("hyprforge-traymenu reported no pid immediately after spawn"),
    }
    drop(guard);

    // No bound on this wait: the popup is waiting on a *person*, not on
    // another program that is supposed to answer quickly — see this
    // module's own doc for why that is not the same "never wait without
    // a bound" hazard `hyprforge_core::command::output` guards against.
    // This runs on its own spawned task (`sni.rs::context_menu`), never on
    // a D-Bus dispatch path, so however long a user takes costs nothing
    // this daemon's event loop needs.
    let mut printed = Vec::new();
    if let Some(stdout) = stdout.as_mut() {
        if let Err(e) = stdout.read_to_end(&mut printed).await {
            tracing::warn!(error = %e, "couldn't read what hyprforge-traymenu chose");
            return LaunchOutcome::Failed;
        }
    }

    // Reap it, but only if it is still the menu that is open. A later
    // right click may have replaced this one while the read above was
    // waiting on a person, and that click already killed and reaped it —
    // matching on the pid is what keeps this from waiting on a child
    // somebody else has taken responsibility for.
    if let Some(pid) = tracked {
        let mut guard = open.0.lock().await;
        if guard.as_ref().is_some_and(|(open_pid, _)| *open_pid == pid) {
            if let Some((_, mut child)) = guard.take() {
                let _ = child.wait().await;
            }
        }
    }

    let action = String::from_utf8_lossy(&printed).trim().to_string();
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
            show_with_binary("hyprforge-traymenu-does-not-exist-xyz", "hyprforge-network", &menu(), 10, 20, &tx, &OpenMenu::default())
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
        // See `SPAWNING`: held for the whole body, because the
        // hazard is a fork anywhere else while this test's script
        // is still open for writing.
        let _spawning = SPAWNING.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("echo-stdin.sh");
        std::fs::write(&script, "#!/bin/sh\ncat\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let outcome =
            show_with_binary(script.to_str().unwrap(), "hyprforge-network", &menu(), 10, 20, &tx, &OpenMenu::default())
                .await;
        assert_eq!(outcome, LaunchOutcome::Spawned);

        assert_eq!(rx.recv().await.unwrap(), "menu:opened:hyprforge-network");
        let echoed = rx.recv().await.unwrap();
        let restored: Menu = serde_json::from_str(&echoed).expect("the script must have echoed valid JSON back");
        assert_eq!(restored, menu(), "the menu handed to stdin must be exactly what was echoed back");
    }

    /// Serialises "write an executable, then exec it" across the tests
    /// in this module.
    ///
    /// Without it these fail intermittently — measured at 2 runs in 12
    /// of the whole crate, and never once in 25 runs of the test alone,
    /// which is the signature of a race with a *sibling* test rather
    /// than a bug in the test itself.
    ///
    /// The race is `ETXTBSY`, and it is a real Unix one rather than
    /// anything specific to this code. When one thread has an
    /// executable open for writing and another thread forks, the child
    /// inherits that open write descriptor; exec of that same file then
    /// fails with "text file busy". Every test here writes a stand-in
    /// script and immediately runs it, and `show_with_binary` forks — so
    /// with more than one running at once, one test's fork can poison
    /// another test's exec.
    ///
    /// A `tokio::sync::Mutex` rather than a `std` one because it is held
    /// across the `.await` on the child.
    static SPAWNING: Mutex<()> = Mutex::const_new(());

    /// Writes an executable script and hands back its path. The scripts
    /// here stand in for `hyprforge-traymenu` — see the test above for
    /// why plain `cat` cannot play the part.
    fn script(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// The bug a user reported as "I have to hit escape first": right
    /// click the Wi-Fi icon, then right click Bluetooth, and nothing
    /// happens — the second popup finds the first one's single-instance
    /// lock held and exits without drawing anything.
    ///
    /// The first menu stands in as a script that never exits on its own,
    /// so the only way this test can finish is the second call actually
    /// closing it. A `timeout` rather than an unbounded await, because
    /// the failure this is written against is precisely "the first menu
    /// stays open forever" — and a test that hangs reports far worse
    /// than one that fails.
    #[tokio::test]
    async fn a_right_click_on_another_icon_replaces_the_menu_already_open() {
        // See `SPAWNING`: held for the whole body, because the
        // hazard is a fork anywhere else while this test's script
        // is still open for writing.
        let _spawning = SPAWNING.lock().await;
        let dir = tempfile::tempdir().unwrap();
        // Reads its stdin away so the write side never blocks, then sits
        // there holding its stdout open: no EOF, no exit, until killed.
        // `exec`, so the process that ends up holding that pipe is the
        // very pid this spawned — a plain `sleep 300` would leave a
        // grandchild inheriting stdout and outliving the kill, which is
        // a shape `hyprforge-traymenu` itself never has, and would make
        // this test measure the script instead of the code under it.
        let forever =
            script(dir.path(), "forever.sh", "#!/bin/sh\ncat >/dev/null\nexec sleep 300\n");
        let echo = script(dir.path(), "echo-stdin.sh", "#!/bin/sh\ncat\n");

        let open = std::sync::Arc::new(OpenMenu::default());
        let (first_tx, _first_rx) = tokio::sync::mpsc::unbounded_channel();
        let first = tokio::spawn({
            let open = open.clone();
            let forever = forever.clone();
            async move {
                show_with_binary(forever.to_str().unwrap(), "hyprforge-network", &menu(), 10, 20, &first_tx, &open)
                    .await
            }
        });

        // Wait for the first menu to actually be open before replacing
        // it — polled rather than slept, since what matters is the slot
        // being occupied, not any particular length of time.
        //
        // The bound is deliberately far longer than this can take. It
        // exists so a menu that never opens fails rather than hangs the
        // suite; it is not a claim about how quickly one does. The same
        // helper in `hyprforge-greet` gave up after a second — fifty
        // times its real figure — and still went red on a loaded CI
        // runner, which is a test reporting someone else's scheduling
        // as a bug here.
        let giving_up_at = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while std::time::Instant::now() < giving_up_at {
            if open.0.lock().await.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(open.0.lock().await.is_some(), "the first menu should be the one that is open");

        let (second_tx, mut second_rx) = tokio::sync::mpsc::unbounded_channel();
        let outcome =
            show_with_binary(echo.to_str().unwrap(), "hyprforge-bluetooth", &menu(), 30, 20, &second_tx, &open).await;
        assert_eq!(outcome, LaunchOutcome::Spawned, "the second menu must open, not be refused");
        assert_eq!(second_rx.recv().await.unwrap(), "menu:opened:hyprforge-bluetooth");
        let echoed = second_rx.recv().await.unwrap();
        serde_json::from_str::<Menu>(&echoed).expect("the second menu really ran and got its JSON");

        let first = tokio::time::timeout(std::time::Duration::from_secs(5), first)
            .await
            .expect("the first menu must have been closed, not left open")
            .unwrap();
        assert_eq!(first, LaunchOutcome::Spawned, "being replaced is not a failure of the menu that was open");
    }

    /// The other half of the same property: once a menu has closed on
    /// its own, nothing is left in the slot for the next right click to
    /// kill. A stale entry there would mean the next click spends a
    /// signal on a pid that may since belong to somebody else.
    #[tokio::test]
    async fn a_menu_that_closes_on_its_own_leaves_nothing_behind_to_kill() {
        // See `SPAWNING`: held for the whole body, because the
        // hazard is a fork anywhere else while this test's script
        // is still open for writing.
        let _spawning = SPAWNING.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let echo = script(dir.path(), "echo-stdin.sh", "#!/bin/sh\ncat\n");

        let open = OpenMenu::default();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        show_with_binary(echo.to_str().unwrap(), "hyprforge-network", &menu(), 10, 20, &tx, &open).await;

        assert!(open.0.lock().await.is_none(), "a menu that exited should not still be the open one");
    }
}
