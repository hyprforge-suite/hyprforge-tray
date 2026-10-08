//! Serving `org.kde.StatusNotifierItem`, and staying registered.
//!
//! An item is served one of two ways, chosen when it registers (see
//! [`crate::prefs::MenuMode`]): with **no `Menu` property at all**, so a
//! right click reaches `ItemInterface::context_menu` and opens
//! `hyprforge-traymenu`; or with a `Menu` property naming a
//! `com.canonical.dbusmenu` object ([`crate::dbusmenu`]) the bar draws
//! itself. zbus declares an interface's properties at compile time, so
//! the two are two interface types — `ItemInterface` and
//! `ItemWithMenuInterface` — and switching between them means
//! registering the item again.

use crate::item::TrayItem;
use crate::menu::Menu;
use crate::prefs::MenuServing;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use zbus::object_server::SignalEmitter;
use zbus::Connection;

/// Watcher calls. A bar that has stopped answering must not hang the
/// daemon — same rule as every other cross-process call in this suite.
pub const TIMEOUT: Duration = Duration::from_secs(5);

pub const ITEM_PATH: &str = "/StatusNotifierItem";

/// Where the `com.canonical.dbusmenu` object lives, for an item served
/// that way. The same path this crate used before the popup existed, and
/// the one most items in the wild use.
pub const MENU_PATH: &str = "/StatusNotifierItem/Menu";

/// Whether an item declares a `Menu` property at all.
///
/// Only when it is served as dbusmenu *and* has rows to serve. An empty
/// layout behind a declared `Menu` is the worst answer there is: a bar
/// builds a menu from it, draws an empty four-pixel GTK menu at the
/// pointer, and — believing it handled the click — never calls
/// `ContextMenu`, so not even the item's primary action runs. An item
/// with nothing to show stays on the no-`Menu` path, where a right click
/// at least reaches `ItemInterface::context_menu`.
pub fn declares_menu(serving: MenuServing, menu: Option<&Menu>) -> bool {
    serving == MenuServing::Dbusmenu && menu.is_some_and(|m| !m.items.is_empty())
}

/// Whether a menu update may replace what is served.
///
/// Refused only for the case [`declares_menu`] exists to rule out, met
/// later: an item already advertising a `Menu` must never be left serving
/// an empty layout behind it. Keeping the last real menu is stale for one
/// poll at worst; an empty one is a menu that cannot be opened at all.
pub fn takes_menu_update(declared: bool, next: &Menu) -> bool {
    !(declared && next.items.is_empty())
}

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
    /// The menu is reserved for the secondary, right-click gesture: drawn
    /// by `hyprforge-traymenu` from [`Self::context_menu`] on this
    /// interface, or by the bar from `com.canonical.dbusmenu` on
    /// `ItemWithMenuInterface`.
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
    // all**, and that is not the same as answering `/`. (An item served
    // as dbusmenu is a different interface type, `ItemWithMenuInterface`,
    // whose `Menu` names a real object with rows behind it.)
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
    /// This interface declares no `Menu` property, so a host has nothing
    /// to draw a menu of its own from and calls this instead — this *is*
    /// the menu on the popup path. It spawns `hyprforge-traymenu`, hands
    /// it the menu this daemon already built, and forwards whatever it
    /// prints back to `hyprforge-trayd`'s own action pipeline — see
    /// `crate::launch::show`'s doc for the whole sequence. (On
    /// `ItemWithMenuInterface` the bar draws the menu from dbusmenu, and
    /// reaches this only if it calls `ContextMenu` anyway.)
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
        // `x`/`y` are forwarded exactly as the host handed them — no
        // `menu_y_offset` added here any more. Adding it to a click's own
        // Y is what made the popup's vertical position depend on where
        // on the icon the user happened to click, which is the bug this
        // crate exists not to have: `hyprforge-traymenu` anchors the
        // popup to the bar's own reserved area instead (reading the
        // offset itself, from the same `tray.toml`), so this daemon's
        // only remaining job is telling it which icon was clicked and on
        // which monitor — see `hyprforge_popup::place_below_bar`'s own
        // doc for the whole mechanism.
        let state = self.state.clone();
        let clicks = self.clicks.clone();
        tokio::spawn(async move {
            let snapshot = menu.lock().await.clone();
            if let crate::launch::LaunchOutcome::NotInstalled =
                crate::launch::show(&id, &snapshot, x, y, &events).await
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

/// The same item, served with a `Menu` property — the dbusmenu path.
///
/// A separate type rather than a flag on `ItemInterface`, because a
/// zbus interface declares its properties at compile time and the whole
/// point of the popup path is that the property is not declared at all
/// (see the comment where it would sit on `ItemInterface`). Everything
/// else is the same item, so every member delegates to `inner`.
///
/// Only ever built by [`TrayIcon`] for an item [`declares_menu`] allows,
/// so the path it names always has rows behind it.
struct ItemWithMenuInterface {
    inner: ItemInterface,
}

#[zbus::interface(name = "org.kde.StatusNotifierItem")]
impl ItemWithMenuInterface {
    #[zbus(property)]
    async fn category(&self) -> String {
        self.inner.category().await
    }

    #[zbus(property)]
    async fn id(&self) -> String {
        self.inner.id().await
    }

    #[zbus(property)]
    async fn title(&self) -> String {
        self.inner.title().await
    }

    #[zbus(property)]
    async fn status(&self) -> String {
        self.inner.status().await
    }

    #[zbus(property)]
    async fn icon_name(&self) -> String {
        self.inner.icon_name().await
    }

    #[zbus(property)]
    async fn icon_pixmap(&self) -> Vec<(i32, i32, Vec<u8>)> {
        self.inner.icon_pixmap().await
    }

    #[zbus(property)]
    async fn item_is_menu(&self) -> bool {
        self.inner.item_is_menu().await
    }

    #[zbus(property)]
    async fn tool_tip(&self) -> (String, Vec<(i32, i32, Vec<u8>)>, String, String) {
        self.inner.tool_tip().await
    }

    /// The `com.canonical.dbusmenu` object this item's menu is served at.
    #[zbus(property)]
    async fn menu(&self) -> zbus::zvariant::OwnedObjectPath {
        zbus::zvariant::OwnedObjectPath::try_from(MENU_PATH).expect("MENU_PATH is a valid object path")
    }

    #[zbus(property)]
    async fn window_id(&self) -> i32 {
        self.inner.window_id().await
    }

    async fn activate(&self, x: i32, y: i32) {
        self.inner.activate(x, y).await;
    }

    async fn secondary_activate(&self, x: i32, y: i32) {
        self.inner.secondary_activate(x, y).await;
    }

    /// A bar that draws dbusmenu does not call this; one that calls it
    /// anyway gets the popup path's answer — `hyprforge-traymenu` where
    /// it is installed, the primary action where it is not.
    async fn context_menu(&self, x: i32, y: i32) {
        self.inner.context_menu(x, y).await;
    }

    async fn scroll(&self, delta: i32, orientation: String) {
        self.inner.scroll(delta, orientation).await;
    }

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
    /// `None` for an item registered without one. The menu's current
    /// content: what `ItemInterface::context_menu` hands to
    /// `hyprforge-traymenu` the next time it is spawned, and — for an
    /// item served as dbusmenu — what the `com.canonical.dbusmenu` object
    /// answers `GetLayout` from.
    menu: Option<Arc<Mutex<crate::menu::Menu>>>,
    /// What this item was asked to be served as. Kept as asked, not as
    /// it ended up ([`Self::menu_declared`]), so a daemon comparing it
    /// with the mode it wants now re-registers only when the *choice*
    /// changed — not every tick for an item that had no rows to declare.
    serving: MenuServing,
    /// Whether a `Menu` property and a dbusmenu object were actually
    /// served — see [`declares_menu`].
    menu_declared: bool,
    /// dbusmenu's layout revision. Has to go **up** on every change: a
    /// host that sees the same revision assumes nothing changed and keeps
    /// showing the menu it already has.
    menu_revision: Arc<Mutex<u32>>,
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
        Self::build(item, None, MenuServing::Popup, index, clicks, None).await
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
        Self::register_serving(item, menu, MenuServing::Popup, index, clicks, menu_clicks).await
    }

    /// [`Self::register_with_menu`], served the way `serving` says — the
    /// popup, or a `com.canonical.dbusmenu` object the bar draws. See
    /// [`crate::prefs::MenuMode`] for which to ask for.
    pub async fn register_serving(
        item: TrayItem,
        menu: crate::menu::Menu,
        serving: MenuServing,
        index: u32,
        clicks: tokio::sync::mpsc::UnboundedSender<String>,
        menu_clicks: tokio::sync::mpsc::UnboundedSender<String>,
    ) -> Result<Self, TrayError> {
        Self::build(item, Some(menu), serving, index, clicks, Some(menu_clicks)).await
    }

    async fn build(
        item: TrayItem,
        menu: Option<crate::menu::Menu>,
        serving: MenuServing,
        index: u32,
        clicks: tokio::sync::mpsc::UnboundedSender<String>,
        menu_clicks: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    ) -> Result<Self, TrayError> {
        let bus_name = format!("org.kde.StatusNotifierItem-{}-{}", std::process::id(), index);
        let menu_declared = declares_menu(serving, menu.as_ref());
        let state = Arc::new(Mutex::new(item.clone()));
        let menu_state = menu.map(|m| Arc::new(Mutex::new(m)));
        let menu_revision = Arc::new(Mutex::new(1u32));

        let inner = ItemInterface {
            state: state.clone(),
            menu: menu_state.clone(),
            clicks,
            menu_clicks: menu_clicks.clone(),
        };
        let builder = zbus::connection::Builder::session()
            .map_err(classify)?
            .name(bus_name.as_str())
            .map_err(classify)?;
        // The popup path serves one object, the item with no `Menu`. The
        // dbusmenu path serves the item with a `Menu` and the menu object
        // it names — on the same connection, because a host resolves the
        // path against the item's own bus name.
        let builder = match (&menu_state, &menu_clicks) {
            (Some(menu), Some(events)) if menu_declared => builder
                .serve_at(ITEM_PATH, ItemWithMenuInterface { inner })
                .map_err(classify)?
                .serve_at(
                    MENU_PATH,
                    crate::dbusmenu::MenuInterface::new(
                        item.id.clone(),
                        menu.clone(),
                        menu_revision.clone(),
                        events.clone(),
                    ),
                )
                .map_err(classify)?,
            _ => builder.serve_at(ITEM_PATH, inner).map_err(classify)?,
        };

        let connection = builder.build().await.map_err(classify)?;

        let icon = TrayIcon {
            connection,
            bus_name,
            state,
            last: Mutex::new(item),
            menu: menu_state,
            serving,
            menu_declared,
            menu_revision,
        };
        icon.announce().await?;
        Ok(icon)
    }

    /// Gives up this item's bus name now, rather than whenever the last
    /// reference to its connection is dropped.
    ///
    /// For an item about to be registered again under the same name — a
    /// change of [`MenuServing`] — where a name still held by the old
    /// connection would make the new registration fail.
    pub async fn release(&self) {
        if let Err(e) = self.connection.release_name(self.bus_name.as_str()).await {
            tracing::warn!(error = %e, item = %self.bus_name, "could not release a tray item's bus name");
        }
    }

    /// What this item was asked to be served as — see the field's doc.
    pub fn serving(&self) -> MenuServing {
        self.serving
    }

    /// Whether this item actually declares a `Menu` property.
    pub fn menu_declared(&self) -> bool {
        self.menu_declared
    }

    /// Replaces the menu's content.
    ///
    /// On the popup path that is the entire update: `hyprforge-traymenu`
    /// is spawned fresh on every right click and reads whatever this
    /// holds at that moment. On the dbusmenu path a bar caches the layout,
    /// so a change also bumps the revision and emits `LayoutUpdated`, or
    /// the bar keeps showing the menu it already has. Either way nothing
    /// is written when nothing changed, since a poll tick calls this
    /// every time.
    ///
    /// An empty menu never replaces a served dbusmenu layout — see
    /// [`takes_menu_update`].
    pub async fn update_menu(&self, next: crate::menu::Menu) -> Result<(), TrayError> {
        let Some(menu) = &self.menu else {
            return Ok(());
        };
        if !takes_menu_update(self.menu_declared, &next) {
            tracing::warn!(
                item = %self.bus_name,
                "kept the last menu rather than serve an empty dbusmenu layout"
            );
            return Ok(());
        }
        let mut current = menu.lock().await;
        if *current == next {
            return Ok(());
        }
        *current = next;
        drop(current);

        if self.menu_declared {
            let revision = {
                let mut revision = self.menu_revision.lock().await;
                *revision += 1;
                *revision
            };
            let emitter = SignalEmitter::new(&self.connection, MENU_PATH).map_err(classify)?;
            crate::dbusmenu::MenuInterface::layout_updated(&emitter, revision, 0)
                .await
                .map_err(classify)?;
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

        // The two interface types carry the same interface name, so either
        // emits the same signal; each is emitted through the type actually
        // served, so neither declares a signal nothing sends.
        let declared = self.menu_declared;
        if icon_changed {
            if declared {
                ItemWithMenuInterface::new_icon(&emitter).await
            } else {
                ItemInterface::new_icon(&emitter).await
            }
            .map_err(classify)?;
        }
        if status_changed {
            let status = next.status.as_str();
            if declared {
                ItemWithMenuInterface::new_status(&emitter, status).await
            } else {
                ItemInterface::new_status(&emitter, status).await
            }
            .map_err(classify)?;
        }
        if title_changed {
            if declared {
                ItemWithMenuInterface::new_title(&emitter).await
            } else {
                ItemInterface::new_title(&emitter).await
            }
            .map_err(classify)?;
        }
        if tooltip_changed {
            if declared {
                ItemWithMenuInterface::new_tool_tip(&emitter).await
            } else {
                ItemInterface::new_tool_tip(&emitter).await
            }
            .map_err(classify)?;
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

    // --- the popup path: no `Menu` property, whether or not this item
    // has a menu recorded for `context_menu` to use.

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

    // `ItemInterface` has no `menu()` at all — see the comment where the
    // property would sit — so its absence on the popup path is enforced
    // by the compiler rather than by a test. What a *host* makes of it
    // needs a real bar on a real bus: `tests/live_tray.rs` asserts the
    // property cannot be read on the popup path and names a menu with
    // rows on the dbusmenu path.

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

    /// The trap this module's `declares_menu` exists for, pinned: a
    /// `Menu` property is declared only with rows behind it. An empty
    /// layout behind a declared `Menu` is an empty four-pixel menu at the
    /// pointer, and a bar that drew it never calls `ContextMenu`, so not
    /// even the primary action runs.
    #[test]
    fn a_menu_property_is_declared_only_with_rows_behind_it() {
        let rows = crate::menu::Menu::new(vec![crate::menu::MenuItem::standard("Settings…", "settings")]);
        let empty = crate::menu::Menu::default();

        assert!(declares_menu(MenuServing::Dbusmenu, Some(&rows)));
        assert!(!declares_menu(MenuServing::Dbusmenu, Some(&empty)), "no rows, no Menu property");
        assert!(!declares_menu(MenuServing::Dbusmenu, None), "no menu, no Menu property");
        assert!(!declares_menu(MenuServing::Popup, Some(&rows)), "the popup path never declares one");
    }

    /// The same trap, met after registration: a served dbusmenu layout is
    /// never replaced by an empty one. The popup path has no layout for a
    /// bar to cache, so it takes whatever it is given.
    #[test]
    fn an_empty_menu_never_replaces_a_served_dbusmenu_layout() {
        let rows = crate::menu::Menu::new(vec![crate::menu::MenuItem::standard("Settings…", "settings")]);
        let empty = crate::menu::Menu::default();

        assert!(!takes_menu_update(true, &empty));
        assert!(takes_menu_update(true, &rows));
        assert!(takes_menu_update(false, &empty), "nothing declared, nothing for a bar to draw empty");
    }

    /// Every member of the dbusmenu-path item answers what the popup-path
    /// item answers — it is the same item with one more property — and
    /// that property names the menu object.
    #[tokio::test]
    async fn the_dbusmenu_item_is_the_same_item_with_a_menu_path() {
        let with_menu = ItemWithMenuInterface { inner: interface(Some(crate::menu::Menu::default())) };
        let plain = interface(None);
        assert_eq!(with_menu.id().await, plain.id().await);
        assert_eq!(with_menu.icon_name().await, plain.icon_name().await);
        assert_eq!(with_menu.tool_tip().await, plain.tool_tip().await);
        assert!(!with_menu.item_is_menu().await);
        assert_eq!(with_menu.menu().await.as_str(), MENU_PATH);
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
