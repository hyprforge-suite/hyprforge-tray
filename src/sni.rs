//! Serving `org.kde.StatusNotifierItem`, and staying registered.

use crate::item::TrayItem;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use zbus::object_server::SignalEmitter;
use zbus::Connection;

/// Watcher calls. A bar that has stopped answering must not hang the
/// daemon — same rule as every other cross-process call in this suite.
pub const TIMEOUT: Duration = Duration::from_secs(5);

pub const ITEM_PATH: &str = "/StatusNotifierItem";

#[derive(Debug, thiserror::Error)]
pub enum TrayError {
    /// No bar is offering a tray. An ordinary state, not a failure: a
    /// user may run no bar at all, or start one later. Callers wait and
    /// retry rather than exiting.
    #[error("no StatusNotifierWatcher is running, so there is no tray to appear in")]
    NoWatcher,
    #[error("the tray host didn't answer within {0:?}")]
    TimedOut(Duration),
    #[error("{0}")]
    Refused(String),
}

fn classify(e: zbus::Error) -> TrayError {
    if let zbus::Error::MethodError(name, _, _) = &e {
        let name = name.as_str();
        if name.ends_with(".ServiceUnknown") || name.ends_with(".NameHasNoOwner") {
            return TrayError::NoWatcher;
        }
        return TrayError::Refused(e.to_string());
    }
    TrayError::Refused(e.to_string())
}

async fn bounded<T>(
    call: impl std::future::Future<Output = zbus::Result<T>>,
) -> Result<T, TrayError> {
    match tokio::time::timeout(TIMEOUT, call).await {
        Err(_) => Err(TrayError::TimedOut(TIMEOUT)),
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(classify(e)),
    }
}

#[zbus::proxy(
    interface = "org.kde.StatusNotifierWatcher",
    default_service = "org.kde.StatusNotifierWatcher",
    default_path = "/StatusNotifierWatcher"
)]
trait StatusNotifierWatcher {
    fn register_status_notifier_item(&self, service: &str) -> zbus::Result<()>;
    #[zbus(property)]
    fn is_status_notifier_host_registered(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn registered_status_notifier_items(&self) -> zbus::Result<Vec<String>>;
}

/// The object a host reads. One per icon.
struct ItemInterface {
    state: Arc<Mutex<TrayItem>>,
    /// `None` for an item registered with no menu at all (plain
    /// [`TrayIcon::register`], never reached by anything in this crate
    /// today). `Some` is never advertised to a host any more — see
    /// [`Self::menu`] — it exists only so [`Self::context_menu`] has
    /// something to hand `hyprforge-traymenu`.
    menu: Option<Arc<Mutex<crate::menu::Menu>>>,
    /// Left-clicks, as the Settings screen name to open. Unbounded
    /// because a click must never block the D-Bus handler it arrives on:
    /// the host is waiting for the method to return, and spawning the
    /// process inline would make the bar stutter.
    clicks: tokio::sync::mpsc::UnboundedSender<String>,
    /// Where a menu click's action string lands — see
    /// [`Self::context_menu`]. `None` alongside `menu: None`; always
    /// `Some` when `menu` is.
    menu_clicks: Option<tokio::sync::mpsc::UnboundedSender<String>>,
}

#[zbus::interface(name = "org.kde.StatusNotifierItem")]
impl ItemInterface {
    #[zbus(property)]
    async fn category(&self) -> String {
        self.state.lock().await.category.as_str().to_string()
    }

    #[zbus(property)]
    async fn id(&self) -> String {
        self.state.lock().await.id.clone()
    }

    #[zbus(property)]
    async fn title(&self) -> String {
        self.state.lock().await.title.clone()
    }

    #[zbus(property)]
    async fn status(&self) -> String {
        self.state.lock().await.status.as_str().to_string()
    }

    #[zbus(property)]
    async fn icon_name(&self) -> String {
        self.state.lock().await.icon_name.clone()
    }

    /// No pixmaps, ever. The icon is a *name*, resolved by the host from
    /// the user's own icon theme — which is the whole reason this crate
    /// needs no toolkit and owns no images.
    #[zbus(property)]
    async fn icon_pixmap(&self) -> Vec<(i32, i32, Vec<u8>)> {
        Vec::new()
    }

    /// `false`: a left-click calls [`Self::activate`] rather than
    /// opening a menu. The spec's own wording for `true` is that the
    /// item *only* supports the context menu — not the case here, since
    /// every icon has a primary action (open the Settings screen it is
    /// about) that a left click should reach directly, menu or no menu.
    /// The menu is reserved for the secondary, right-click gesture; see
    /// [`Self::context_menu`] for how it is shown now that this crate no
    /// longer serves `com.canonical.dbusmenu` for a host to draw itself.
    #[zbus(property)]
    async fn item_is_menu(&self) -> bool {
        false
    }

    /// `(icon_name, icon_pixmap, title, description)`.
    #[zbus(property)]
    async fn tool_tip(&self) -> (String, Vec<(i32, i32, Vec<u8>)>, String, String) {
        let state = self.state.lock().await;
        (
            state.icon_name.clone(),
            Vec::new(),
            state.tooltip_title.clone(),
            state.tooltip_body.clone(),
        )
    }

    // There is deliberately **no `Menu` property on this interface at
    // all**, and that is not the same as answering `/`.
    //
    // `/` was the first attempt, on the reading that an object path
    // cannot be null so the root path must mean "nothing here". A bar
    // does not read it that way. waybar sees the property exists, builds
    // a `com.canonical.dbusmenu` client against whatever path it names,
    // gets no layout back from `/`, and draws an **empty GTK menu** — a
    // four-pixel line at the pointer — and, believing it has handled the
    // click itself, never calls `ContextMenu` at all. So the popup this
    // crate exists to show never opened.
    //
    // Omitting the property entirely is what leaves a host with nothing
    // to build from and sends it to `context_menu`, which is where
    // `hyprforge-traymenu` gets launched. The property is optional in
    // the StatusNotifierItem spec precisely so an item can say it has no
    // menu of its own.
    //
    // This one is only settleable against a real bar: every version of
    // it type-checks, and the difference between them is what somebody
    // else's code does with the answer.

    /// Zero: these items have no window of their own. A host uses this
    /// only to associate an item with an X11 window, which is meaningless
    /// for a daemon that has none.
    #[zbus(property)]
    async fn window_id(&self) -> i32 {
        0
    }

    /// Left-click, and this item's primary action: open the Settings
    /// screen it is about.
    async fn activate(&self, _x: i32, _y: i32) {
        let screen = self.state.lock().await.activate_screen();
        if let Some(screen) = screen {
            // Send, never spawn, for the reason on `clicks`.
            let _ = self.clicks.send(screen.to_string());
        }
    }

    /// Middle-click. No icon here defines a distinct secondary action —
    /// there is nothing a radio or a toggle gains from the "less
    /// important" activation the spec describes this as — so it runs
    /// the same primary action as a left click rather than doing
    /// nothing, which would read as a stuck button.
    async fn secondary_activate(&self, x: i32, y: i32) {
        self.activate(x, y).await;
    }

    /// Right-click.
    ///
    /// [`Self::menu`] always answers `/` now, so a host never draws a
    /// menu of its own for this call to be a fallback from — this *is*
    /// the menu, for every host. What used to be waybar's own
    /// `com.canonical.dbusmenu` fallback path (calling this only when the
    /// dbusmenu it asked for came back with no layout) is now the only
    /// path: this spawns `hyprforge-traymenu`, hands it the menu this
    /// daemon already built, and forwards whatever it prints back to
    /// `hyprforge-trayd`'s own action pipeline — see
    /// `crate::launch::show`'s doc for the whole sequence, and this
    /// crate's own module doc for why a host can no longer draw this
    /// itself.
    ///
    /// Spawned onto its own task rather than awaited here: the host is
    /// blocked on this D-Bus method returning, and the popup can stay
    /// open for as long as the user is looking at it. An item with no
    /// menu at all (plain [`TrayIcon::register`], never reached by
    /// anything in this crate today) falls back to [`Self::activate`],
    /// since doing nothing there would be indistinguishable from a hung
    /// daemon — and a missing `hyprforge-traymenu` binary takes the same
    /// fallback, once spawning it has actually been tried and failed,
    /// for the reason CLAUDE.md gives for `spawn_settings`: a sibling
    /// binary that isn't installed is a click served the next-best way,
    /// never a daemon that stops working.
    async fn context_menu(&self, x: i32, y: i32) {
        let (Some(menu), Some(events)) = (self.menu.clone(), self.menu_clicks.clone()) else {
            self.activate(x, y).await;
            return;
        };

        let id = self.state.lock().await.id.clone();
        // Never collapsed with "menu_y_offset is unreadable" turning into
        // silence — an unreadable `tray.toml` is already warned about by
        // `hyprforge-trayd`'s own poll loop every time it changes; this
        // is just the one place that also needs a number out of it right
        // now, and the default is the least surprising thing to use
        // rather than refusing to open the menu at all.
        let offset = match crate::prefs::load() {
            Ok(prefs) => prefs.menu_y_offset,
            Err(e) => {
                tracing::warn!(error = %e, "could not read tray.toml for the menu's Y offset; using the default");
                crate::prefs::Prefs::default().menu_y_offset
            }
        };

        let state = self.state.clone();
        let clicks = self.clicks.clone();
        tokio::spawn(async move {
            let snapshot = menu.lock().await.clone();
            if let crate::launch::LaunchOutcome::NotInstalled =
                crate::launch::show(&id, &snapshot, x, y.saturating_add(offset), &events).await
            {
                let screen = state.lock().await.activate_screen();
                if let Some(screen) = screen {
                    let _ = clicks.send(screen.to_string());
                }
            }
        });
    }

    async fn scroll(&self, _delta: i32, _orientation: String) {}

    #[zbus(signal)]
    async fn new_icon(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn new_status(emitter: &SignalEmitter<'_>, status: &str) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn new_tool_tip(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn new_title(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

/// A live tray icon: its own bus name, its own connection, its own
/// registration.
///
/// One connection per item rather than one shared: the spec identifies an
/// item *by its bus name*, so two items on one connection would be one
/// item to every host.
pub struct TrayIcon {
    connection: Connection,
    bus_name: String,
    state: Arc<Mutex<TrayItem>>,
    last: Mutex<TrayItem>,
    /// `None` for an item registered without one. Never advertised over
    /// D-Bus any more (see [`ItemInterface::menu`]) — this is purely this
    /// daemon's own record of the menu's current content, for
    /// [`ItemInterface::context_menu`] to hand to `hyprforge-traymenu`
    /// the next time it is spawned.
    menu: Option<Arc<Mutex<crate::menu::Menu>>>,
}

impl TrayIcon {
    /// Claims a bus name, serves the item on it, and registers with the
    /// watcher.
    ///
    /// `index` distinguishes several items from one process; the spec's
    /// convention is `org.kde.StatusNotifierItem-<pid>-<n>`.
    pub async fn register(
        item: TrayItem,
        index: u32,
        clicks: tokio::sync::mpsc::UnboundedSender<String>,
    ) -> Result<Self, TrayError> {
        Self::build(item, None, index, clicks, None).await
    }

    /// Registers an item that also has a right-click menu.
    ///
    /// `menu_clicks` carries the [`crate::menu::MenuItem`] actions chosen
    /// in `hyprforge-traymenu`, kept separate from `clicks` because the
    /// two mean different things: a click on the icon says which settings
    /// screen to open, a click in the menu says which operation to
    /// perform.
    pub async fn register_with_menu(
        item: TrayItem,
        menu: crate::menu::Menu,
        index: u32,
        clicks: tokio::sync::mpsc::UnboundedSender<String>,
        menu_clicks: tokio::sync::mpsc::UnboundedSender<String>,
    ) -> Result<Self, TrayError> {
        Self::build(item, Some(menu), index, clicks, Some(menu_clicks)).await
    }

    async fn build(
        item: TrayItem,
        menu: Option<crate::menu::Menu>,
        index: u32,
        clicks: tokio::sync::mpsc::UnboundedSender<String>,
        menu_clicks: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    ) -> Result<Self, TrayError> {
        let bus_name = format!("org.kde.StatusNotifierItem-{}-{}", std::process::id(), index);
        let state = Arc::new(Mutex::new(item.clone()));
        let menu_state = menu.map(|m| Arc::new(Mutex::new(m)));

        // One object served at `ITEM_PATH` and nothing else — there used
        // to be a second one, `com.canonical.dbusmenu` at `MENU_PATH`;
        // see this crate's own module doc for why that no longer exists.
        let builder = zbus::connection::Builder::session()
            .map_err(classify)?
            .name(bus_name.as_str())
            .map_err(classify)?
            .serve_at(
                ITEM_PATH,
                ItemInterface {
                    state: state.clone(),
                    menu: menu_state.clone(),
                    clicks,
                    menu_clicks,
                },
            )
            .map_err(classify)?;

        let connection = builder.build().await.map_err(classify)?;

        let icon = TrayIcon {
            connection,
            bus_name,
            state,
            last: Mutex::new(item),
            menu: menu_state,
        };
        icon.announce().await?;
        Ok(icon)
    }

    /// Replaces this daemon's own record of the menu's content.
    ///
    /// Used to bump a revision and signal a host that its cached layout
    /// was stale, back when a host could cache one at all. There is
    /// nothing left to notify: `hyprforge-traymenu` is spawned fresh on
    /// every right click and reads whatever this holds at that moment, so
    /// replacing it here is the entire update. Still skips the write when
    /// nothing changed, the same as before, since a `Mutex` a poll tick
    /// never needs to touch is a poll tick that never contends with a
    /// menu click reading it.
    pub async fn update_menu(&self, next: crate::menu::Menu) -> Result<(), TrayError> {
        let Some(menu) = &self.menu else {
            return Ok(());
        };
        let mut current = menu.lock().await;
        if *current != next {
            *current = next;
        }
        Ok(())
    }

    /// Tells the watcher this item exists.
    ///
    /// Called again whenever the watcher reappears — a bar restart takes
    /// every registration with it, and an item that does not re-announce
    /// is simply gone until the daemon is restarted too. That is the
    /// failure mode every hand-rolled tray implementation has.
    pub async fn announce(&self) -> Result<(), TrayError> {
        let watcher = bounded(StatusNotifierWatcherProxy::new(&self.connection)).await?;
        bounded(watcher.register_status_notifier_item(&self.bus_name)).await
    }

    /// Replaces the visible state, emitting only the signals that
    /// correspond to something that actually changed.
    ///
    /// Emitting all four every tick would make a bar redraw every icon
    /// every few seconds for no reason, which is visible as flicker on
    /// some hosts.
    pub async fn update(&self, next: TrayItem) -> Result<(), TrayError> {
        let mut last = self.last.lock().await;
        if *last == next {
            return Ok(());
        }
        let emitter = SignalEmitter::new(&self.connection, ITEM_PATH).map_err(classify)?;

        let icon_changed = last.icon_name != next.icon_name;
        let status_changed = last.status != next.status;
        let title_changed = last.title != next.title;
        let tooltip_changed =
            last.tooltip_title != next.tooltip_title || last.tooltip_body != next.tooltip_body;

        *self.state.lock().await = next.clone();
        *last = next.clone();
        drop(last);

        if icon_changed {
            ItemInterface::new_icon(&emitter).await.map_err(classify)?;
        }
        if status_changed {
            ItemInterface::new_status(&emitter, next.status.as_str())
                .await
                .map_err(classify)?;
        }
        if title_changed {
            ItemInterface::new_title(&emitter).await.map_err(classify)?;
        }
        if tooltip_changed {
            ItemInterface::new_tool_tip(&emitter).await.map_err(classify)?;
        }
        Ok(())
    }

    pub fn bus_name(&self) -> &str {
        &self.bus_name
    }
}

/// Whether a tray host is currently available to register with.
///
/// Separate from registering so a daemon can wait for a bar to start
/// rather than failing at boot — `hl.exec_cmd` starts things before much
/// of the session exists, and a tray daemon losing that race would
/// otherwise be permanently invisible.
pub async fn watcher_present(connection: &Connection) -> bool {
    match bounded(StatusNotifierWatcherProxy::new(connection)).await {
        Ok(watcher) => bounded(watcher.is_status_notifier_host_registered())
            .await
            .unwrap_or(false),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item::{Category, Status};

    fn item() -> TrayItem {
        TrayItem {
            id: "hyprforge-network".to_string(),
            category: Category::Hardware,
            status: Status::Active,
            title: "Network".to_string(),
            icon_name: "network-wireless-signal-good".to_string(),
            tooltip_title: "Connected".to_string(),
            tooltip_body: "home".to_string(),
        }
    }

    /// The spec identifies an item by its bus name, so the shape of that
    /// name is not cosmetic — a host parses the pid out of it.
    #[test]
    fn the_bus_name_follows_the_convention_hosts_expect() {
        let name = format!("org.kde.StatusNotifierItem-{}-{}", std::process::id(), 1);
        assert!(name.starts_with("org.kde.StatusNotifierItem-"));
        assert_eq!(name.matches('-').count(), 2);
    }

    /// No bar running is an ordinary state — plenty of people run none —
    /// so it has to be distinguishable from the bar being broken, or a
    /// daemon cannot decide whether to wait or to complain.
    #[test]
    fn no_bar_running_is_its_own_state_rather_than_a_failure() {
        let message = TrayError::NoWatcher.to_string();
        assert!(message.contains("no StatusNotifierWatcher"));
        assert!(!matches!(TrayError::NoWatcher, TrayError::Refused(_)));
    }

    /// An item whose state has not changed emits nothing. A bar that is
    /// told its icon changed redraws it, and being told so every tick is
    /// flicker.
    #[test]
    fn an_unchanged_item_is_equal_to_itself_so_update_can_skip_it() {
        assert_eq!(item(), item());
        let mut changed = item();
        changed.icon_name = "network-wireless-signal-weak".to_string();
        assert_ne!(item(), changed);
    }

    // --- `menu()`: the whole point of retiring `com.canonical.dbusmenu`
    // is that a host never sees a path to ask about, whether or not this
    // item actually has one recorded for `context_menu` to use.

    fn interface(menu: Option<crate::menu::Menu>) -> ItemInterface {
        let (clicks, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (menu_clicks, _mrx) = tokio::sync::mpsc::unbounded_channel();
        ItemInterface {
            state: Arc::new(Mutex::new(item())),
            menu: menu.map(|m| Arc::new(Mutex::new(m))),
            clicks,
            menu_clicks: Some(menu_clicks),
        }
    }

    // The two tests that used to sit here asserted `menu()` answered
    // `/`. There is no `menu()` any more — see the comment where the
    // property used to be declared — and its absence is enforced by the
    // compiler rather than by a test.
    //
    // What a *host* makes of that absence is the part worth checking,
    // and it is not checkable here: it needs a real bar on a real bus.
    // `tests/live_tray.rs` asserts the introspected interface carries no
    // `Menu` property at all, which is the claim this crate makes about
    // somebody else's code.

    /// `item_is_menu` must stay `false` regardless of whether a menu is
    /// recorded — this item's own primary action is still what a left
    /// click should reach, menu or no menu (see the property's own doc).
    #[tokio::test]
    async fn item_is_menu_stays_false_whether_or_not_there_is_a_menu() {
        assert!(!interface(None).item_is_menu().await);
        assert!(
            !interface(Some(crate::menu::Menu::default())).item_is_menu().await
        );
    }

    /// An item with no menu at all falls back to its primary action on a
    /// right click, the same as it always has — this is the one
    /// `context_menu` path that needs no `hyprforge-traymenu` on `$PATH`
    /// to test deterministically; the "menu present but the binary is
    /// missing" fallback is covered live by `launch`'s own tests instead,
    /// since it would otherwise depend on whatever happens to be on this
    /// test's `$PATH`.
    #[tokio::test]
    async fn context_menu_with_no_menu_at_all_falls_back_to_the_primary_action() {
        let (clicks, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let iface = ItemInterface { state: Arc::new(Mutex::new(item())), menu: None, clicks, menu_clicks: None };
        iface.context_menu(0, 0).await;
        assert_eq!(rx.recv().await.unwrap(), "network", "hyprforge-network opens the network screen");
    }
}
