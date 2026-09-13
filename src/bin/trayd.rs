//! `hyprforge-trayd`: four `org.kde.StatusNotifierItem`s — Wi-Fi and
//! Bluetooth joined to `hyprforge-network` and `hyprforge-bluetooth`, plus
//! keep-awake (`hyprforge-power`, over `systemd-logind`) and night light
//! (`hyprforge-ecosystem::sunset_control`, over `hyprctl hyprsunset`).
//!
//! Everything that decides *which icon* a state deserves lives in the
//! pure functions below (`network_item`, `bluetooth_item`, `keep_awake_item`,
//! `night_light_item`) — no D-Bus, no I/O — so the interesting question is
//! unit-tested without a bus, a bar, a radio, or hyprsunset, the same
//! split `crate::item` documents. Everything else here is plumbing:
//! polling the four backends, keeping four `TrayIcon`s in sync, and
//! forwarding clicks to `hyprforge-settings`.
//!
//! Whether an icon is shown at all is `hyprforge_tray::prefs`, re-read
//! every poll tick (`refresh_prefs`) so a toggle in Settings takes effect
//! within one tick without restarting this daemon. Switching an icon off
//! **drops its `TrayIcon`** — see `sync_icon` and [`IconAction`] — rather
//! than setting it `Status::Passive`, because not every tray host hides a
//! Passive item, and dropping is the only thing that reliably releases
//! the bus name a host is showing.
//!
//! Every menu (`network_menu`, `bluetooth_menu`, `keep_awake_menu`,
//! `night_light_menu`) is built through `hyprforge_tray::menu::radio_menu`,
//! which is the one shape all four follow: a toggle (or, when there is
//! nothing to toggle, a single disabled row saying why), the content that
//! toggle governs, and a settings row — never omitted, unavailable states
//! included. See `radio_menu`'s own doc for the reasoning.
//!
//! # Where every menu's settings row goes
//!
//! All four now land somewhere real. `--screen network` and
//! `--screen bluetooth` open those screens directly; `--screen idle` and
//! `--screen night-light` deep-link to the `Idle` and `NightLight` tabs
//! of the Desktop screen (`hyprforge-settings`'s `screen_from_cli`) —
//! which is also what `TrayItem::activate_screen` already sends a left
//! click on either icon to. Neither destination existed when this
//! daemon's keep-awake and night-light icons were first added; both do
//! now, so a settings row that used to have nowhere honest to send
//! anyone finally does, and the two icons stop being the one pair of the
//! four without a way out of their own menu.

use hyprforge_bluetooth::backend::{for_display as bt_for_display, BluetoothBackend};
use hyprforge_bluetooth::{Address, AdapterState, BlueZBackend, Device};
use hyprforge_bluetooth::Status as BtStatus;
use hyprforge_ecosystem::sunset_control::{Hyprsunset, SunsetBackend, SunsetControlError};
use hyprforge_network::backend::{for_display as net_for_display, NetworkBackend, SavedNetwork};
use hyprforge_network::{AccessPoint, NetworkManagerBackend, RadioState, Ssid};
use hyprforge_network::Status as NetStatus;
use hyprforge_power::{InhibitBackend, InhibitorInfo, LogindBackend, WhatSet};
use hyprforge_tray::menu::{radio_menu, Menu, MenuItem};
use hyprforge_tray::prefs::{self, Prefs};
use hyprforge_tray::{watcher_present, Category, TrayIcon, TrayItem};
use hyprforge_tray::Status as TrayStatus;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

/// Where one icon's registration lives between ticks: `None` when the
/// icon is switched off (or not yet registered), `Some` while it's up.
/// Shared between `poll_loop`, which is the only writer, and
/// `reannounce_loop`, which only ever reads it to call `announce` again.
type IconSlot = Arc<Mutex<Option<Arc<TrayIcon>>>>;

/// One managed icon's registration slot, together with the dbusmenu
/// index it was registered under.
///
/// `poll_loop`, `reannounce_loop` and `main` used to thread
/// `network_slot` and `bluetooth_slot` as separate named variables. Two
/// was already about the limit of what reads cleanly that way; adding
/// `keep_awake` and `night_light` on top as two more named parameters
/// would have meant every one of those three functions growing a pair of
/// arguments it just forwards along, and the `for slot in [&a, &b]` in
/// `reannounce_loop` growing into a four-item literal that says nothing
/// about *why* those four belong together. A `Vec` of these instead means
/// "every icon" is a single collection everywhere that question is asked,
/// and a fifth icon later is one more entry in the vec `main` builds, not
/// one more parameter threaded through three functions.
#[derive(Clone)]
struct ManagedIcon {
    slot: IconSlot,
    index: u32,
}

impl ManagedIcon {
    fn new(icon: Arc<TrayIcon>, index: u32) -> Self {
        ManagedIcon { slot: Arc::new(Mutex::new(Some(icon))), index }
    }
}

/// How often both backends are re-polled. Ten seconds is what the
/// Settings app's Network and Bluetooth screens already poll at while
/// open — matching it means the tray icon and an open Settings window
/// never visibly disagree for long.
const POLL_INTERVAL: Duration = Duration::from_secs(10);

/// How often a failed `TrayIcon::register` or a not-yet-present watcher
/// is retried. Short enough that a bar starting up is followed quickly,
/// long enough not to spin.
const RETRY_INTERVAL: Duration = Duration::from_secs(3);

/// How often the watcher's presence is polled to notice a bar restart.
///
/// A `NameOwnerChanged` subscription would need matching on
/// `org.kde.StatusNotifierWatcher` specifically and filtering out the
/// "still has an owner, just a different one during a graceful restart"
/// case — `watcher_present` (which `main` already uses to wait at
/// startup) answers the only question that matters here just as well,
/// and a tray host is not a hot path, so polling it is not a real cost.
const WATCHER_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// How often `tray.toml` is *stat*ed — not how often it is parsed.
///
/// Toggling "Show in tray" used to take ten or twelve seconds to show on
/// screen, because the preference file was only re-read on the ordinary
/// poll tick. A setting that takes that long reads as broken, and the
/// user tries again.
///
/// A stat is one syscall on a file that is always in the page cache;
/// twice a second costs nothing measurable, and only a changed stamp
/// causes an actual read. An inotify watch would be tidier still, but it
/// is a dependency and a set of edge cases (the file is replaced by
/// rename, so the watch has to follow the directory) for a saving that
/// does not exist at this size.
const PREFS_WATCH_INTERVAL: Duration = Duration::from_millis(500);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Default to `info` rather than whatever `from_default_env` alone
    // gives with RUST_LOG unset, which is nothing — a daemon with no GUI
    // that prints nothing on start is indistinguishable from a hung one.
    // RUST_LOG still overrides this when set.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // One session-bus connection, kept for the lifetime of the process,
    // used only to ask the watcher whether it's there — `TrayIcon::register`
    // opens its own connection per item, for the reason on `TrayIcon`.
    let probe = zbus::Connection::session().await?;

    if !watcher_present(&probe).await {
        // No bar is an ordinary state (`hl.exec_cmd` starts this before
        // much of the session exists), not a failure — wait rather than
        // exit, or the daemon that starts before the bar is the one that
        // never shows an icon.
        tracing::info!("no tray host yet; waiting for one to start");
        loop {
            tokio::time::sleep(WATCHER_POLL_INTERVAL).await;
            if watcher_present(&probe).await {
                break;
            }
        }
    }
    tracing::info!("tray host found; registering icons");

    let (clicks_tx, clicks_rx) = tokio::sync::mpsc::unbounded_channel();
    // Separate from `clicks_tx`: a click on the icon itself says which
    // settings screen to open, a click inside its menu says which
    // operation to perform. `sni.rs` keeps the two apart for the same
    // reason — see `TrayIcon::register_with_menu`'s doc comment.
    let (menu_clicks_tx, menu_clicks_rx) = tokio::sync::mpsc::unbounded_channel();
    // Lets a menu open pull the next poll forward, so the scan it kicks
    // off is actually reflected before the ten-second tick would.
    let (refresh_tx, refresh_rx) = tokio::sync::mpsc::unbounded_channel();

    // Registered unavailable-looking to start: the first real poll is at
    // most one `POLL_INTERVAL` away, and an icon that starts blank until
    // then would look identical to a daemon that never started. Both
    // start registered regardless of `tray.toml` — `poll_loop`'s first
    // tick (which fires immediately) reads preferences and drops
    // whichever one the user has already switched off, so there is only
    // one place that ever decides "is this icon supposed to be up".
    let network_icon = register_with_retry(
        network_item(None, None, None, true),
        network_menu(None, None, true, &[], &[]),
        0,
        clicks_tx.clone(),
        menu_clicks_tx.clone(),
    )
    .await;
    let bluetooth_icon = register_with_retry(
        bluetooth_item(None, None, true),
        bluetooth_menu(None, true, &[]),
        1,
        clicks_tx.clone(),
        menu_clicks_tx.clone(),
    )
    .await;
    let keep_awake_icon = register_with_retry(
        keep_awake_item(None, &[], true),
        keep_awake_menu(None, &[], true),
        2,
        clicks_tx.clone(),
        menu_clicks_tx.clone(),
    )
    .await;
    let night_light_icon = register_with_retry(
        night_light_item(&NightLightState::CouldNotCheck),
        night_light_menu(&NightLightState::CouldNotCheck),
        3,
        clicks_tx.clone(),
        menu_clicks_tx.clone(),
    )
    .await;

    let icons = vec![
        ManagedIcon::new(network_icon, 0),
        ManagedIcon::new(bluetooth_icon, 1),
        ManagedIcon::new(keep_awake_icon, 2),
        ManagedIcon::new(night_light_icon, 3),
    ];

    // This daemon's own memory of whether night light is on — see
    // `NightLightBelief`'s doc comment for why hyprsunset itself cannot
    // answer that question. Shared between `poll_loop`, which reads it to
    // render the icon, and `handle_menu_clicks`, the only writer.
    let night_light_belief: NightLightBelief = Arc::new(std::sync::Mutex::new(true));

    let poll_task = tokio::spawn(poll_loop(
        icons.clone(),
        night_light_belief.clone(),
        clicks_tx.clone(),
        menu_clicks_tx.clone(),
        refresh_rx,
    ));
    let click_task = tokio::spawn(handle_clicks(clicks_rx));
    let menu_click_task =
        tokio::spawn(handle_menu_clicks(menu_clicks_rx, refresh_tx.clone(), night_light_belief));
    let prefs_watch_task = tokio::spawn(prefs_watch_loop(refresh_tx));
    let reannounce_task = tokio::spawn(reannounce_loop(probe, icons));

    // None of these loops return under normal operation. If one panics,
    // that is worth knowing about rather than leaving the others running
    // silently short-handed — a dead prefs watcher, in particular, looks
    // exactly like the ten-second lag it was written to remove.
    tokio::select! {
        r = poll_task => log_task_exit("poll", r),
        r = click_task => log_task_exit("clicks", r),
        r = menu_click_task => log_task_exit("menu-clicks", r),
        r = reannounce_task => log_task_exit("reannounce", r),
        r = prefs_watch_task => log_task_exit("prefs-watch", r),
    }

    Ok(())
}

fn log_task_exit(name: &str, result: Result<(), tokio::task::JoinError>) {
    match result {
        Ok(()) => tracing::warn!(task = name, "background task exited unexpectedly"),
        Err(e) => tracing::error!(task = name, error = %e, "background task panicked"),
    }
}

/// Registers one tray icon, retrying indefinitely rather than giving up.
///
/// `main` already waited for a tray host before calling this, but
/// registration can still fail — a host that goes away between the
/// watcher check and `register_status_notifier_item`, for instance — and
/// exiting the whole daemon over that would take *both* icons down for
/// one host hiccup, which defeats the reason there are two independent
/// icons in the first place.
async fn register_with_retry(
    item: TrayItem,
    menu: Menu,
    index: u32,
    clicks: tokio::sync::mpsc::UnboundedSender<String>,
    menu_clicks: tokio::sync::mpsc::UnboundedSender<String>,
) -> Arc<TrayIcon> {
    loop {
        match TrayIcon::register_with_menu(
            item.clone(),
            menu.clone(),
            index,
            clicks.clone(),
            menu_clicks.clone(),
        )
        .await
        {
            Ok(icon) => return Arc::new(icon),
            Err(e) => {
                tracing::warn!(error = %e, icon = %item.id, "failed to register tray icon; retrying");
                tokio::time::sleep(RETRY_INTERVAL).await;
            }
        }
    }
}

/// Holds a backend connection once one succeeds, and keeps retrying on
/// every tick until it does.
///
/// Caching a *failed* connection attempt would repeat exactly the
/// mistake documented on `LazyNetworkManagerBackend` in
/// `hyprforge-settings`: a `NetworkError::Unavailable` tells the user how
/// to fix it (`systemctl start NetworkManager`), and a daemon that stops
/// checking after the first failure means following that instruction and
/// watching the tray icon say the same thing forever.
struct Reconnecting<B> {
    backend: Option<Arc<B>>,
}

impl<B> Reconnecting<B> {
    fn new() -> Self {
        Reconnecting { backend: None }
    }

    /// Returns the cached backend, or runs `connect` again. Only
    /// `Ok` is ever cached, which is the whole property this type
    /// exists to guarantee — see the struct doc.
    async fn get_or_connect<F, Fut, E>(&mut self, connect: F) -> Result<Arc<B>, E>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<B, E>>,
    {
        if let Some(backend) = &self.backend {
            return Ok(backend.clone());
        }
        let backend = Arc::new(connect().await?);
        self.backend = Some(backend.clone());
        Ok(backend)
    }
}

async fn network_backend(
    state: &mut Reconnecting<NetworkManagerBackend>,
) -> Option<Arc<NetworkManagerBackend>> {
    match state.get_or_connect(NetworkManagerBackend::connect).await {
        Ok(backend) => Some(backend),
        Err(e) => {
            tracing::warn!(error = %e, "could not connect to NetworkManager; will retry");
            None
        }
    }
}

async fn bluetooth_backend(state: &mut Reconnecting<BlueZBackend>) -> Option<Arc<BlueZBackend>> {
    match state.get_or_connect(BlueZBackend::connect).await {
        Ok(backend) => Some(backend),
        Err(e) => {
            tracing::warn!(error = %e, "could not connect to BlueZ; will retry");
            None
        }
    }
}

async fn keep_awake_backend(state: &mut Reconnecting<LogindBackend>) -> Option<Arc<LogindBackend>> {
    match state.get_or_connect(LogindBackend::connect).await {
        Ok(backend) => Some(backend),
        Err(e) => {
            tracing::warn!(error = %e, "could not connect to systemd-logind; will retry");
            None
        }
    }
}

/// This daemon's own belief about whether night light is on, shared
/// between the loop that samples it and the loop that acts on menu
/// clicks.
///
/// hyprsunset has **no request that reports whether identity mode is
/// active** — `hyprforge_ecosystem::sunset_control`'s module doc explains
/// why in full — so unlike every other piece of state this daemon shows,
/// "is night light on" is never read back from the thing it describes.
/// It is only ever this daemon's memory of the last command *it* sent.
/// Starts `true`: a restart forgets any earlier "off" and the least
/// surprising default is to assume whatever a schedule or a previous
/// session already had running is still running, rather than claim off
/// with no basis for it either.
type NightLightBelief = Arc<std::sync::Mutex<bool>>;

/// One tick's worth of the network icon.
///
/// The connection, once opened, is a handle to the *session bus name*
/// `org.freedesktop.NetworkManager` — not a socket to the daemon itself —
/// so it is kept even when a call against it fails; only the call result
/// is reported as unavailable that tick. A dead bus connection (the
/// exceedingly rare case) would fail `network_backend` itself and go
/// through the retry path above rather than this one.
/// One tick's worth of the network icon *and* its menu — fetched
/// together because both need the same access-point scan and saved-
/// network list, and asking for that twice would be a second round trip
/// against NetworkManager for no new information.
async fn sample_network(state: &mut Reconnecting<NetworkManagerBackend>) -> (TrayItem, Menu) {
    let Some(backend) = network_backend(state).await else {
        return (network_item(None, None, None, true), network_menu(None, None, true, &[], &[]));
    };
    match backend.status().await {
        Ok(status) => {
            // The connected network can briefly be missing from a fresh
            // scan; that is not "unavailable", just "nothing read this
            // tick" — an empty list here still lets the item and menu
            // render, just without a signal reading or any other rows.
            let points = backend.access_points().await.unwrap_or_default();
            let strength = status
                .connected_to
                .as_ref()
                .and_then(|ssid| points.iter().find(|ap| &ap.ssid == ssid).map(|ap| ap.strength));
            // Likewise: a failed saved-list read must not make an
            // otherwise-fine menu disappear, only omit the "already
            // saved" distinction for this tick.
            let saved = backend.saved_networks().await.unwrap_or_default();
            let item = network_item(Some(&status), status.connected_to.as_ref(), strength, false);
            let menu =
                network_menu(Some(&status), status.connected_to.as_ref(), false, &points, &saved);
            (item, menu)
        }
        Err(e) => {
            tracing::warn!(error = %e, "NetworkManager status call failed");
            (network_item(None, None, None, true), network_menu(None, None, true, &[], &[]))
        }
    }
}

async fn sample_bluetooth(state: &mut Reconnecting<BlueZBackend>) -> (TrayItem, Menu) {
    let Some(backend) = bluetooth_backend(state).await else {
        return (bluetooth_item(None, None, true), bluetooth_menu(None, true, &[]));
    };
    match backend.status().await {
        Ok(status) => {
            let devices = backend.devices().await.unwrap_or_default();
            let connected = devices.iter().find(|d| d.connected);
            let item = bluetooth_item(Some(&status), connected, false);
            let menu = bluetooth_menu(Some(&status), false, &devices);
            (item, menu)
        }
        Err(e) => {
            tracing::warn!(error = %e, "BlueZ status call failed");
            (bluetooth_item(None, None, true), bluetooth_menu(None, true, &[]))
        }
    }
}

/// One tick's worth of the keep-awake icon and its menu.
///
/// `list_inhibitors` is asked every tick regardless of whether *we* are
/// currently holding one — the whole point of showing other holders (see
/// `keep_awake_menu`) is answering "why won't this machine sleep"
/// independent of whether this daemon is the reason, so it would be
/// wrong to skip that call just because our own inhibit is off.
async fn sample_keep_awake(state: &mut Reconnecting<LogindBackend>) -> (TrayItem, Menu) {
    let Some(backend) = keep_awake_backend(state).await else {
        return (keep_awake_item(None, &[], true), keep_awake_menu(None, &[], true));
    };
    match backend.held().await {
        Ok(held) => {
            // A failed `list_inhibitors` must not blank an otherwise-fine
            // icon — same reasoning as a failed saved-network read in
            // `sample_network` — so it degrades to "no other holders
            // known this tick" rather than "keep awake unavailable".
            let others = backend.list_inhibitors().await.unwrap_or_default();
            let others = other_inhibitors(others);
            (
                keep_awake_item(Some(held.is_some()), &others, false),
                keep_awake_menu(Some(held.is_some()), &others, false),
            )
        }
        Err(e) => {
            tracing::warn!(error = %e, "logind held() call failed");
            (keep_awake_item(None, &[], true), keep_awake_menu(None, &[], true))
        }
    }
}

/// Where night light stands, as far as this daemon can honestly claim to
/// know — see [`NightLightBelief`] for why "on" is never a direct read.
#[derive(Debug, Clone, Copy, PartialEq)]
enum NightLightState {
    /// Confirmed absent: `pgrep -x hyprsunset` found nothing.
    NotRunning,
    /// The check itself failed (`pgrep` didn't start or answer), or
    /// `hyprctl` refused/timed out — not evidence of anything about
    /// hyprsunset, so this is never rendered as "off".
    CouldNotCheck,
    /// hyprsunset answered. `temperature` is what it is currently holding
    /// (meaningful only when `on`); `on` is this daemon's own belief, not
    /// something hyprsunset reported.
    Known { temperature: i64, on: bool },
}

/// One tick's worth of the night-light icon and its menu.
///
/// `Hyprsunset::current_temperature` shells out to `hyprctl` via
/// `hyprforge_core::command::output`, which is bounded but still
/// synchronous — run inside `spawn_blocking` so a slow `hyprctl` stalls
/// only this one poll, not every other task on the runtime. The same
/// pattern `hyprforge-settings::modules::desktop` already uses for
/// `apply_sunset`.
async fn sample_night_light(believed_on: &NightLightBelief) -> (TrayItem, Menu) {
    let believed_on = *believed_on.lock().unwrap_or_else(|e| e.into_inner());
    let state = tokio::task::spawn_blocking(move || match Hyprsunset.current_temperature() {
        Ok(temperature) => NightLightState::Known { temperature, on: believed_on },
        Err(SunsetControlError::NotRunning) => NightLightState::NotRunning,
        Err(e) => {
            tracing::warn!(error = %e, "hyprsunset temperature check failed");
            NightLightState::CouldNotCheck
        }
    })
    .await
    .unwrap_or_else(|e| {
        tracing::warn!(error = %e, "night light check panicked");
        NightLightState::CouldNotCheck
    });
    (night_light_item(&state), night_light_menu(&state))
}

/// What to do with one icon's registration, given whether the user wants
/// it shown and whether it is currently up.
///
/// Pure — no D-Bus, no I/O — so the on/off transition is unit-tested
/// directly, the same split `network_item`/`bluetooth_item` already use
/// for "what should this icon say". `currently_up` is passed in rather
/// than read from the `IconSlot` itself, so a test needs no `Mutex`, no
/// `TrayIcon`, and no async runtime to exercise every case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IconAction {
    /// Already in the state the preference wants; nothing to do besides
    /// (if it's up) pushing this tick's new content to it.
    Keep,
    /// Wanted, and not currently registered.
    Register,
    /// Currently registered, and no longer wanted. The `TrayIcon` itself
    /// must be dropped — see the module doc for why `Status::Passive`
    /// alone does not reliably hide an item.
    Drop,
}

fn icon_action(wanted: bool, currently_up: bool) -> IconAction {
    match (wanted, currently_up) {
        (true, false) => IconAction::Register,
        (false, true) => IconAction::Drop,
        (true, true) | (false, false) => IconAction::Keep,
    }
}

/// Re-reads `tray.toml`, updating `current` only on success.
///
/// A failure here — the file exists and will not parse — must never
/// change what is currently shown: that is the "typo hides the whole
/// tray" failure mode the task exists to rule out. `warned` suppresses
/// repeat warnings for as long as the same failure persists, so a broken
/// file logs once rather than once every `POLL_INTERVAL` forever; it
/// resets as soon as a read succeeds, so a *later* failure warns again.
fn refresh_prefs(current: &mut Prefs, warned: &mut bool) {
    match prefs::load() {
        Ok(loaded) => {
            *current = loaded;
            *warned = false;
        }
        Err(e) => {
            if !*warned {
                tracing::warn!(
                    error = %e,
                    "could not read tray.toml; leaving tray icons as they are"
                );
                *warned = true;
            }
        }
    }
}

/// Brings one icon's `IconSlot` in line with `wanted` and pushes this
/// tick's content to it if it ends up (or stays) registered.
async fn sync_icon(
    slot: &IconSlot,
    wanted: bool,
    item: TrayItem,
    menu: Menu,
    index: u32,
    clicks: &tokio::sync::mpsc::UnboundedSender<String>,
    menu_clicks: &tokio::sync::mpsc::UnboundedSender<String>,
) {
    let id = item.id.clone();
    let mut guard = slot.lock().await;
    match icon_action(wanted, guard.is_some()) {
        IconAction::Keep => {
            if let Some(icon) = guard.as_ref() {
                if let Err(e) = icon.update(item).await {
                    tracing::warn!(error = %e, icon = %id, "failed to update tray icon");
                }
                // `update_menu` already skips the work (and the revision
                // bump) when the menu is unchanged, so calling it every
                // tick costs nothing extra when nothing changed.
                if let Err(e) = icon.update_menu(menu).await {
                    tracing::warn!(error = %e, icon = %id, "failed to update tray menu");
                }
            }
        }
        IconAction::Drop => {
            tracing::info!(icon = %id, "tray icon switched off; releasing its bus name");
            // Dropping the `Arc<TrayIcon>` (assuming this is the last
            // reference, which it is — nothing else holds one) drops its
            // `zbus::Connection`, which is what actually releases the bus
            // name. Setting `Status::Passive` instead would leave the
            // name — and the item — registered, which is the mistake
            // this whole feature exists to avoid; see the module doc.
            *guard = None;
        }
        IconAction::Register => {
            match TrayIcon::register_with_menu(item, menu, index, clicks.clone(), menu_clicks.clone())
                .await
            {
                Ok(icon) => *guard = Some(Arc::new(icon)),
                Err(e) => {
                    tracing::warn!(error = %e, icon = %id, "failed to register tray icon; will retry next tick");
                }
            }
        }
    }
}

/// Something that changes whenever the file does.
///
/// `None` for a file that is not there, which is a state rather than an
/// error: deleting `tray.toml` is a reset to the defaults, and has to
/// take effect like any other edit. Length is included alongside the
/// modification time so an edit inside one timestamp's resolution is
/// still seen.
fn prefs_stamp(path: &std::path::Path) -> Option<(std::time::SystemTime, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// Pulls the next poll forward the moment the preference file changes.
async fn prefs_watch_loop(refresh: tokio::sync::mpsc::UnboundedSender<()>) {
    let path = hyprforge_tray::prefs::path();
    let mut seen = prefs_stamp(&path);
    let mut ticker = tokio::time::interval(PREFS_WATCH_INTERVAL);
    loop {
        ticker.tick().await;
        let now = prefs_stamp(&path);
        if now == seen {
            continue;
        }
        seen = now;
        // A closed channel means the poll loop is gone, so there is
        // nothing left to tell.
        if refresh.send(()).is_err() {
            return;
        }
    }
}

async fn poll_loop(
    icons: Vec<ManagedIcon>,
    night_light_belief: NightLightBelief,
    clicks: tokio::sync::mpsc::UnboundedSender<String>,
    menu_clicks: tokio::sync::mpsc::UnboundedSender<String>,
    mut refresh_rx: tokio::sync::mpsc::UnboundedReceiver<()>,
) {
    let mut net_state = Reconnecting::new();
    let mut bt_state = Reconnecting::new();
    let mut power_state = Reconnecting::new();
    // Every icon starts registered (see `main`), so the preferences this
    // loop starts from must match that — otherwise an icon the user
    // already switched off would flash on screen for up to one
    // `POLL_INTERVAL` before this loop notices.
    let mut prefs = Prefs::default();
    let mut prefs_load_failed = false;
    // `interval` fires its first tick immediately, which is wanted here:
    // every icon started out saying "unavailable" in `main`, and the
    // first real read (of every backend and `tray.toml`) should not wait
    // a further `POLL_INTERVAL`.
    let mut ticker = tokio::time::interval(POLL_INTERVAL);
    loop {
        // Either the ordinary tick, or someone opening a menu asked for
        // the next one early. A refresh that arrives while this loop is
        // mid-sample is not lost: the channel holds it and the next
        // `select!` takes it immediately.
        tokio::select! {
            _ = ticker.tick() => {}
            _ = refresh_rx.recv() => {}
        }

        refresh_prefs(&mut prefs, &mut prefs_load_failed);

        let (net_item, net_menu) = sample_network(&mut net_state).await;
        let (bt_item, bt_menu) = sample_bluetooth(&mut bt_state).await;
        let (ka_item, ka_menu) = sample_keep_awake(&mut power_state).await;
        let (nl_item, nl_menu) = sample_night_light(&night_light_belief).await;

        // One row per icon: `(wanted, item, menu)` lined up against
        // `icons` in the same fixed order `main` registered them in — see
        // `ManagedIcon`'s doc comment for why this is a loop rather than
        // four repeated `sync_icon` calls.
        let ticks: [(bool, TrayItem, Menu); 4] = [
            (prefs.network, net_item, net_menu),
            (prefs.bluetooth, bt_item, bt_menu),
            (prefs.keep_awake, ka_item, ka_menu),
            (prefs.night_light, nl_item, nl_menu),
        ];
        for (icon, (wanted, item, menu)) in icons.iter().zip(ticks) {
            sync_icon(&icon.slot, wanted, item, menu, icon.index, &clicks, &menu_clicks).await;
        }
    }
}

/// Forwards left-clicks (and the button-agnostic activations `sni.rs`
/// maps onto them) to `hyprforge-settings --screen <name>`.
///
/// Spawned and never waited on. The click arrived over `activate`, whose
/// D-Bus caller — the bar — is blocked on the method returning; waiting
/// here for the child to exit would make the whole bar stutter on every
/// click, and a launcher that fails to start is a warning, not a reason
/// to stop handling further clicks.
async fn handle_clicks(mut clicks: tokio::sync::mpsc::UnboundedReceiver<String>) {
    while let Some(screen) = clicks.recv().await {
        spawn_settings(&screen);
    }
}

/// Launches `hyprforge-settings --screen <screen>`, never waited on.
///
/// Shared by an icon click (`handle_clicks`) and a menu row that opens
/// Settings instead of acting directly — a network needing a passphrase
/// with no saved connection, for instance, since a tray menu has nowhere
/// to type one.
fn spawn_settings(screen: &str) {
    match tokio::process::Command::new("hyprforge-settings")
        .arg("--screen")
        .arg(screen)
        // The child's output goes nowhere rather than inheriting this
        // daemon's. A GUI started from here prints its own renderer
        // chatter — wgpu alone logs its adapter and surface formats at
        // startup — and inheriting it puts all of that in the tray
        // daemon's log, once per click, for as long as the session
        // lasts. The daemon's log is then unreadable and unbounded, and
        // the one thing it exists to record — a failed registration — is
        // buried. The child keeps its own logging; it does not need
        // ours.
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(error = %e, screen = %screen, "failed to launch hyprforge-settings")
        }
    }
}

/// Re-announces both icons whenever the tray host reappears.
///
/// A bar restart takes every registration on the old connection with it;
/// `TrayIcon::announce`'s doc comment covers why re-registering is the
/// only way back. This only fires on the *transition* into "present" —
/// re-announcing every poll while a host is already there would ask it to
/// re-add an icon it already has for no reason.
async fn reannounce_loop(probe: zbus::Connection, icons: Vec<ManagedIcon>) {
    // `main` only reaches this point after `watcher_present` returned
    // true, so the host is presumed present until proven otherwise.
    let mut present = true;
    let mut ticker = tokio::time::interval(WATCHER_POLL_INTERVAL);
    loop {
        ticker.tick().await;
        let now_present = watcher_present(&probe).await;
        if now_present && !present {
            tracing::info!("tray host reappeared; re-announcing icons");
            // An icon currently switched off has no `TrayIcon` to
            // re-announce — `poll_loop` will register it fresh (which
            // announces on its own) if and when the user turns it back
            // on, so there is nothing missing here to re-announce.
            for icon in &icons {
                if let Some(tray_icon) = icon.slot.lock().await.as_ref() {
                    if let Err(e) = tray_icon.announce().await {
                        tracing::warn!(error = %e, "failed to re-announce a tray icon");
                    }
                }
            }
        }
        present = now_present;
    }
}

/// Freedesktop icon names below the boundary in [`u8`] percent, matching
/// the row NetworkManager applets themselves use: 80/60/40/20.
///
/// `-symbolic`, not the plain name most NetworkManager applets use: the
/// plain form is a full-colour icon, and the other three items in this
/// tray (Bluetooth, keep awake, night light) are all symbolic already.
/// One colourful icon among three monochrome ones was the whole
/// complaint this rule exists to prevent. Confirmed present at both
/// `status/22` and `status/24` in `breeze` and `breeze-dark` on this
/// machine — the family every name in this file now shares.
fn wifi_signal_icon(strength: u8) -> &'static str {
    match strength {
        80..=u8::MAX => "network-wireless-signal-excellent-symbolic",
        60..=79 => "network-wireless-signal-good-symbolic",
        40..=59 => "network-wireless-signal-ok-symbolic",
        20..=39 => "network-wireless-signal-weak-symbolic",
        _ => "network-wireless-signal-none-symbolic",
    }
}

/// What the Wi-Fi icon should say, from plain data — no D-Bus, no I/O.
///
/// `connected_ssid` and `strength` are passed separately from `status`
/// rather than derived from it here, so a test can assert the tooltip and
/// icon for a specific signal reading without constructing a full access
/// point list. `unavailable` is checked before `status` is even looked
/// at, since a `None` status on its own already means the same thing:
/// nothing was read this tick, so say so rather than guessing.
fn network_item(
    status: Option<&NetStatus>,
    connected_ssid: Option<&Ssid>,
    strength: Option<u8>,
    unavailable: bool,
) -> TrayItem {
    let id = NETWORK_ITEM_ID.to_string();
    let title = "Wi-Fi".to_string();

    let Some(status) = status.filter(|_| !unavailable) else {
        return TrayItem {
            id,
            category: Category::Hardware,
            // Visible, not hidden — an icon that disappears exactly when
            // the daemon behind it can't be reached looks identical to
            // "nothing to report", which is the one thing pillar 3
            // forbids.
            status: TrayStatus::Active,
            title,
            icon_name: "dialog-warning".to_string(),
            tooltip_title: "Wi-Fi unavailable".to_string(),
            tooltip_body: "NetworkManager isn't running.".to_string(),
        };
    };

    match status.radio {
        // Off (switched off in software) and HardwareOff (rfkill) both
        // mean the radio is not on — the one case `item.rs` says to hide.
        RadioState::Off | RadioState::HardwareOff => TrayItem {
            id,
            category: Category::Hardware,
            status: TrayStatus::Passive,
            title,
            icon_name: "network-wireless-disconnected-symbolic".to_string(),
            tooltip_title: "Wi-Fi is off".to_string(),
            tooltip_body: String::new(),
        },
        RadioState::On => match connected_ssid {
            Some(ssid) => TrayItem {
                id,
                category: Category::Hardware,
                status: TrayStatus::Active,
                title,
                icon_name: wifi_signal_icon(strength.unwrap_or(0)).to_string(),
                tooltip_title: ssid.to_display_string(),
                tooltip_body: match strength {
                    Some(s) => format!("Signal {s}%"),
                    None => "Connected".to_string(),
                },
            },
            // On and unconnected is exactly when someone goes looking for
            // this icon, so — unlike the radio-off case — it stays
            // visible rather than hiding.
            None => TrayItem {
                id,
                category: Category::Hardware,
                status: TrayStatus::Active,
                title,
                icon_name: "network-wireless-disconnected-symbolic".to_string(),
                tooltip_title: "Not connected".to_string(),
                tooltip_body: "Wi-Fi is on".to_string(),
            },
        },
    }
}

/// What the Bluetooth icon should say, from plain data — no D-Bus, no
/// I/O. Same shape and same reasoning as [`network_item`].
fn bluetooth_item(
    status: Option<&BtStatus>,
    connected_device: Option<&Device>,
    unavailable: bool,
) -> TrayItem {
    let id = "hyprforge-bluetooth".to_string();
    let title = "Bluetooth".to_string();

    let Some(status) = status.filter(|_| !unavailable) else {
        return TrayItem {
            id,
            category: Category::Hardware,
            status: TrayStatus::Active,
            title,
            icon_name: "dialog-warning".to_string(),
            tooltip_title: "Bluetooth unavailable".to_string(),
            tooltip_body: "bluetoothd isn't running.".to_string(),
        };
    };

    match status.state {
        // HardwareBlocked is rfkill, not "off" in software, but both
        // mean the radio is not on — the case worth hiding for.
        AdapterState::Off | AdapterState::HardwareBlocked => TrayItem {
            id,
            category: Category::Hardware,
            status: TrayStatus::Passive,
            title,
            icon_name: "network-bluetooth-inactive-symbolic".to_string(),
            tooltip_title: "Bluetooth is off".to_string(),
            tooltip_body: String::new(),
        },
        AdapterState::On | AdapterState::Changing => match connected_device {
            Some(device) => TrayItem {
                id,
                category: Category::Hardware,
                status: TrayStatus::Active,
                title,
                icon_name: "network-bluetooth-activated-symbolic".to_string(),
                // `alias` always exists (falls back to `Name`, then the
                // address) — never the raw address alone, and never
                // anything derived from pairing secrets, of which this
                // crate exposes none anyway.
                tooltip_title: device.alias.clone(),
                tooltip_body: device.kind.label().to_string(),
            },
            None => TrayItem {
                id,
                category: Category::Hardware,
                status: TrayStatus::Active,
                title,
                icon_name: "network-bluetooth-activated-symbolic".to_string(),
                tooltip_title: "Not connected".to_string(),
                tooltip_body: "Bluetooth is on".to_string(),
            },
        },
    }
}

/// How many networks the Wi-Fi menu shows before it starts truncating.
///
/// A scan in a dense apartment building can return dozens of access
/// points; a menu that lists all of them stops being a menu. Eight is
/// enough to show a home network plus a handful of neighbours without the
/// popup running off the bottom of the screen.
const MAX_NETWORKS_SHOWN: usize = 8;

/// Renders a byte slice as lowercase hex.
///
/// [`Ssid`] is arbitrary bytes, not necessarily UTF-8 (see its doc
/// comment), so it cannot be embedded in an action string as text without
/// either losing information or risking a `:` inside the name being
/// mistaken for a field separator. Hex side-steps both: every byte
/// round-trips, and the result contains none of the vocabulary's own
/// delimiters.
fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks_exact(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push(((hi << 4) | lo) as u8);
    }
    Some(out)
}

/// Why [`Security::Enterprise`] is disabled, shortened for a menu row
/// rather than the full sentence [`hyprforge_network::Security::unsupported_reason`]
/// gives a whole screen to.
const ENTERPRISE_ROW_REASON: &str = "enterprise sign-in not supported";

/// What the Wi-Fi menu should contain, from plain data — no D-Bus, no I/O.
///
/// `access_points` is expected in scan order; this calls
/// [`hyprforge_network::backend::for_display`] itself, the same
/// dedup-and-sort the Network settings screen uses, so the two never
/// disagree about which of several BSSIDs sharing an SSID is "the"
/// network. `saved` decides whether a passphrase-protected network gets a
/// connect action or a Settings one — see the module-level task doc: a
/// tray menu has nowhere to type a passphrase, so a network this daemon
/// has no stored credential for can only be joined from Settings.
fn network_menu(
    status: Option<&NetStatus>,
    connected_ssid: Option<&Ssid>,
    unavailable: bool,
    access_points: &[AccessPoint],
    saved: &[SavedNetwork],
) -> Menu {
    let settings_row = MenuItem::standard("Network settings…", "wifi:settings");

    let Some(status) = status.filter(|_| !unavailable) else {
        return radio_menu(vec![MenuItem::disabled("Wi-Fi unavailable")], Vec::new(), settings_row);
    };

    let top = match status.radio {
        RadioState::Off => {
            return radio_menu(
                vec![MenuItem::checkmark("Wi-Fi", false, "wifi:radio:on")],
                Vec::new(),
                settings_row,
            );
        }
        // rfkill: no amount of D-Bus flips this back on, so the row is
        // shown but not offered as a toggle — the same call
        // `network_item` makes for the icon itself.
        RadioState::HardwareOff => {
            return radio_menu(
                vec![MenuItem::disabled_checkmark("Wi-Fi (blocked by hardware switch)", false)],
                Vec::new(),
                settings_row,
            );
        }
        RadioState::On => vec![MenuItem::checkmark("Wi-Fi", true, "wifi:radio:off")],
    };

    let mut content = Vec::new();
    let displayed = net_for_display(access_points.to_vec());
    let total = displayed.len();
    for ap in displayed.into_iter().take(MAX_NETWORKS_SHOWN) {
        let is_connected = connected_ssid == Some(&ap.ssid);
        let label = ap.ssid.to_display_string();

        if ap.security.unsupported_reason().is_some() {
            content.push(MenuItem::disabled(format!("{label} ({ENTERPRISE_ROW_REASON})")));
            continue;
        }

        let has_saved = saved.iter().any(|s| s.ssid == ap.ssid);
        let action = if ap.security.needs_passphrase() && !has_saved {
            // Nowhere to type a passphrase in a tray menu — see the
            // module doc.
            "wifi:settings".to_string()
        } else {
            format!("wifi:connect:{}", hex_encode(ap.ssid.as_bytes()))
        };

        if is_connected {
            content.push(MenuItem::checkmark(label, true, action));
        } else {
            content.push(MenuItem::standard(label, action));
        }
    }
    if total > MAX_NETWORKS_SHOWN {
        content.push(MenuItem::disabled(format!(
            "+{} more — see Settings",
            total - MAX_NETWORKS_SHOWN
        )));
    }

    radio_menu(top, content, settings_row)
}

/// What the Bluetooth menu should contain, from plain data — no D-Bus, no
/// I/O. Same shape and reasoning as [`network_menu`].
fn bluetooth_menu(status: Option<&BtStatus>, unavailable: bool, devices: &[Device]) -> Menu {
    let settings_row = MenuItem::standard("Bluetooth settings…", "bt:settings");

    let Some(status) = status.filter(|_| !unavailable) else {
        return radio_menu(vec![MenuItem::disabled("Bluetooth unavailable")], Vec::new(), settings_row);
    };

    let top = match status.state {
        AdapterState::Off => {
            return radio_menu(
                vec![MenuItem::checkmark("Bluetooth", false, "bt:radio:on")],
                Vec::new(),
                settings_row,
            );
        }
        AdapterState::HardwareBlocked => {
            return radio_menu(
                vec![MenuItem::disabled_checkmark("Bluetooth (blocked by hardware switch)", false)],
                Vec::new(),
                settings_row,
            );
        }
        AdapterState::On | AdapterState::Changing => {
            vec![MenuItem::checkmark("Bluetooth", true, "bt:radio:off")]
        }
    };

    let mut content = Vec::new();
    for device in bt_for_display(devices.to_vec()) {
        if !device.paired {
            // A menu has nowhere to show six digits and ask whether they
            // match, so pairing is handed to the screen that does — the
            // same call as a Wi-Fi network whose passphrase we do not
            // have. It used to say "pairing not supported yet", which
            // stopped being true the moment the agent landed.
            content.push(MenuItem::standard(
                format!("{} — pair in Settings", device.alias),
                "bt:settings",
            ));
            continue;
        }

        let action = if device.connected {
            format!("bt:disconnect:{}", device.address)
        } else {
            format!("bt:connect:{}", device.address)
        };

        if device.connected {
            content.push(MenuItem::checkmark(device.alias.clone(), true, action));
        } else {
            content.push(MenuItem::standard(device.alias.clone(), action));
        }
    }

    radio_menu(top, content, settings_row)
}

/// The id `hyprctl`/logind knows this daemon's own inhibit by. Used both
/// when taking one ([`perform_menu_action`]) and to recognise — and
/// exclude — it in [`other_inhibitors`], so our own toggle never shows up
/// a second time, disabled, in its own menu.
const KEEP_AWAKE_WHO: &str = "hyprforge-trayd";
const KEEP_AWAKE_WHY: &str = "user requested via the tray";
const KEEP_AWAKE_ITEM_ID: &str = "hyprforge-keep-awake";
const NIGHT_LIGHT_ITEM_ID: &str = "hyprforge-night-light";

/// How many other inhibitors the keep-awake menu shows before truncating.
/// A busy session can rack up several (a video call, a download manager,
/// a game); a menu that lists all of them stops being a menu — same
/// reasoning as [`MAX_NETWORKS_SHOWN`].
const MAX_OTHER_INHIBITORS_SHOWN: usize = 5;

/// Everyone else's inhibitor — [`InhibitBackend::list_inhibitors`]
/// reports every one logind holds, ours included, and this is what keeps
/// our own toggle from appearing a second time in the list of *other*
/// holders right next to the checkmark that already represents it.
fn other_inhibitors(all: Vec<InhibitorInfo>) -> Vec<InhibitorInfo> {
    all.into_iter().filter(|i| i.who != KEEP_AWAKE_WHO).collect()
}

/// What the keep-awake icon should say, from plain data — no D-Bus, no
/// I/O. `others` is expected already filtered by [`other_inhibitors`].
///
/// `Status::Active` only while this daemon is actually holding the
/// inhibit, `Status::Passive` otherwise. This is *not* the network/
/// Bluetooth split (radio off is passive, radio on-but-idle is still
/// active because someone might come looking for it) — keep awake has no
/// state of its own to monitor when it is off. "Not currently keeping
/// this machine awake" is the ordinary condition for nearly every minute
/// of a session, and showing it permanently would be exactly the noise a
/// radio icon that never hides would be.
fn keep_awake_item(held: Option<bool>, others: &[InhibitorInfo], unavailable: bool) -> TrayItem {
    let id = KEEP_AWAKE_ITEM_ID.to_string();
    let title = "Keep awake".to_string();

    let Some(held) = held.filter(|_| !unavailable) else {
        return TrayItem {
            id,
            category: Category::SystemServices,
            status: TrayStatus::Active,
            title,
            icon_name: "dialog-warning".to_string(),
            tooltip_title: "Keep awake unavailable".to_string(),
            tooltip_body: "systemd-logind isn't answering.".to_string(),
        };
    };

    let plural = if others.len() == 1 { "" } else { "es" };
    if held {
        TrayItem {
            id,
            category: Category::SystemServices,
            status: TrayStatus::Active,
            title,
            icon_name: "changes-prevent-symbolic".to_string(),
            tooltip_title: "Keeping this machine awake".to_string(),
            tooltip_body: if others.is_empty() {
                String::new()
            } else {
                format!("Also held by {} other process{plural}", others.len())
            },
        }
    } else {
        TrayItem {
            id,
            category: Category::SystemServices,
            // Active, not Passive, even though nothing is being held.
            //
            // `Passive` asks the host to hide the item, and hosts obey —
            // so switching this icon on in Settings made it appear and
            // then immediately vanish, because "not currently keeping
            // the machine awake" is the state it starts in. An icon you
            // can only see once it is already on is a toggle you cannot
            // reach.
            //
            // The radios are the case Passive is right for: they hide
            // when the *hardware* is off, which is a state the user chose
            // elsewhere and cannot act on from the bar anyway. This is a
            // switch, and a switch has to be visible in both positions.
            status: TrayStatus::Active,
            title,
            icon_name: "changes-allow-symbolic".to_string(),
            tooltip_title: "Not keeping this machine awake".to_string(),
            tooltip_body: if others.is_empty() {
                "Nothing is preventing sleep".to_string()
            } else {
                format!("{} other process{plural} preventing sleep", others.len())
            },
        }
    }
}

/// What the keep-awake menu should contain, from plain data — no D-Bus,
/// no I/O. Same expectation on `others` as [`keep_awake_item`].
///
/// The checkmark is the only actionable row. Every other holder listed
/// beneath it is disabled: this daemon only ever manages its own
/// inhibit, never someone else's — `InhibitBackend::list_inhibitors`'s
/// doc comment says as much — so the value of listing them is answering
/// "why won't this machine sleep" from the tray, not offering to end
/// them.
fn keep_awake_menu(held: Option<bool>, others: &[InhibitorInfo], unavailable: bool) -> Menu {
    let settings_row = MenuItem::standard("Keep awake settings…", "keepawake:settings");

    let Some(held) = held.filter(|_| !unavailable) else {
        return radio_menu(vec![MenuItem::disabled("Keep awake unavailable")], Vec::new(), settings_row);
    };

    let toggle = MenuItem::checkmark(
        "Keep awake",
        held,
        if held { "keepawake:off" } else { "keepawake:on" },
    );

    let mut content = Vec::new();
    if !others.is_empty() {
        content.push(MenuItem::disabled("Also preventing sleep:"));
        for other in others.iter().take(MAX_OTHER_INHIBITORS_SHOWN) {
            content.push(MenuItem::disabled(format!("{} — {}", other.who, other.why)));
        }
        if others.len() > MAX_OTHER_INHIBITORS_SHOWN {
            content.push(MenuItem::disabled(format!(
                "+{} more",
                others.len() - MAX_OTHER_INHIBITORS_SHOWN
            )));
        }
    }

    radio_menu(vec![toggle], content, settings_row)
}

/// The warm presets offered as menu rows — the one thing a tray menu can
/// do that a plain on/off toggle cannot. 4500K matches
/// `sunset_control::DEFAULT_WARM_TEMPERATURE` deliberately, so turning
/// night light on from the checkmark and picking the first preset are
/// the same action.
const NIGHT_LIGHT_PRESETS: &[i64] = &[4500, 3500, 2700];

/// What the night-light icon should say, from plain data — no D-Bus, no
/// I/O.
///
/// Three distinct rows for three distinct facts, none of them collapsed
/// into another: [`NightLightState::NotRunning`] is hyprsunset confirmed
/// absent, [`NightLightState::CouldNotCheck`] is the check itself
/// failing (evidence about `pgrep`/`hyprctl`, not about hyprsunset), and
/// [`NightLightState::Known`] is an actual reading. Rendering
/// `NotRunning` with the same icon as "off" would be exactly the mistake
/// the task this module was built against calls out by name: a daemon
/// that isn't there must never look like a night light setting.
fn night_light_item(state: &NightLightState) -> TrayItem {
    let id = NIGHT_LIGHT_ITEM_ID.to_string();
    let title = "Night light".to_string();
    match *state {
        NightLightState::NotRunning => TrayItem {
            id,
            category: Category::SystemServices,
            status: TrayStatus::Active,
            title,
            icon_name: "dialog-warning".to_string(),
            tooltip_title: "Night light unavailable".to_string(),
            tooltip_body: "hyprsunset isn't running.".to_string(),
        },
        NightLightState::CouldNotCheck => TrayItem {
            id,
            category: Category::SystemServices,
            status: TrayStatus::Active,
            title,
            icon_name: "dialog-information".to_string(),
            tooltip_title: "Night light status unknown".to_string(),
            tooltip_body: "Couldn't tell whether hyprsunset is running.".to_string(),
        },
        NightLightState::Known { temperature, on: true } => TrayItem {
            id,
            category: Category::SystemServices,
            status: TrayStatus::Active,
            title,
            icon_name: "redshift-status-on-symbolic".to_string(),
            tooltip_title: "Night light is on".to_string(),
            tooltip_body: format!("{temperature}K"),
        },
        NightLightState::Known { on: false, .. } => TrayItem {
            id,
            category: Category::SystemServices,
            status: TrayStatus::Passive,
            title,
            icon_name: "redshift-status-off-symbolic".to_string(),
            tooltip_title: "Night light is off".to_string(),
            tooltip_body: String::new(),
        },
    }
}

/// What the night-light menu should contain, from plain data — no D-Bus,
/// no I/O. Same three-states rule as [`night_light_item`]: the
/// unavailable rows say which of the two distinct reasons applies rather
/// than sharing one "can't do anything" row.
fn night_light_menu(state: &NightLightState) -> Menu {
    let settings_row = MenuItem::standard("Night light settings…", "nightlight:settings");

    let (temperature, on) = match *state {
        // Names the feature, like the other three unavailable rows, but
        // keeps the detail that makes this one actionable: hyprsunset not
        // running is expected until something starts it, not a bug.
        NightLightState::NotRunning => {
            return radio_menu(
                vec![MenuItem::disabled("Night light unavailable — hyprsunset isn't running")],
                Vec::new(),
                settings_row,
            );
        }
        // Distinct from `NotRunning`: this is the check itself failing,
        // not evidence that hyprsunset is absent — see `NightLightState`'s
        // own doc. Collapsing the two into one row would lose exactly the
        // distinction CLAUDE.md calls out.
        NightLightState::CouldNotCheck => {
            return radio_menu(
                vec![MenuItem::disabled("Night light status unknown")],
                Vec::new(),
                settings_row,
            );
        }
        NightLightState::Known { temperature, on } => (temperature, on),
    };

    let toggle_action = if on {
        "nightlight:off".to_string()
    } else {
        format!("nightlight:set:{}", NIGHT_LIGHT_PRESETS[0])
    };
    let toggle = MenuItem::checkmark("Night light", on, toggle_action);

    let mut content = Vec::new();
    for &kelvin in NIGHT_LIGHT_PRESETS {
        let is_current = on && temperature == kelvin;
        let action = format!("nightlight:set:{kelvin}");
        if is_current {
            content.push(MenuItem::checkmark(format!("{kelvin}K"), true, action));
        } else {
            content.push(MenuItem::standard(format!("{kelvin}K"), action));
        }
    }

    radio_menu(vec![toggle], content, settings_row)
}

/// The typed operation behind an action string a menu click sends back.
///
/// Parsed in exactly one place ([`parse_menu_action`]) so every route a
/// click can take is visible in one match, and an unknown string — a
/// typo'd prefix, or a stale action from a menu revision that no longer
/// exists — resolves to nothing rather than to whichever arm happens to
/// match a prefix of it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum MenuAction {
    /// Opens `hyprforge-settings --screen <screen>`.
    OpenSettings(&'static str),
    WifiRadio(bool),
    /// The SSID's raw bytes, decoded from the hex in the action string.
    WifiConnect(Vec<u8>),
    BtRadio(bool),
    BtConnect(Address),
    BtDisconnect(Address),
    /// `true` to take the inhibit, `false` to release it.
    KeepAwake(bool),
    /// Turns night light off (`hyprctl hyprsunset identity`).
    NightLightOff,
    /// Sets a specific colour temperature, turning night light on if it
    /// was off.
    NightLightSet(i64),
}

fn parse_menu_action(action: &str) -> Option<MenuAction> {
    match action {
        "wifi:settings" => Some(MenuAction::OpenSettings("network")),
        "wifi:radio:on" => Some(MenuAction::WifiRadio(true)),
        "wifi:radio:off" => Some(MenuAction::WifiRadio(false)),
        "bt:settings" => Some(MenuAction::OpenSettings("bluetooth")),
        "bt:radio:on" => Some(MenuAction::BtRadio(true)),
        "bt:radio:off" => Some(MenuAction::BtRadio(false)),
        "keepawake:on" => Some(MenuAction::KeepAwake(true)),
        "keepawake:off" => Some(MenuAction::KeepAwake(false)),
        "keepawake:settings" => Some(MenuAction::OpenSettings("idle")),
        "nightlight:off" => Some(MenuAction::NightLightOff),
        "nightlight:settings" => Some(MenuAction::OpenSettings("night-light")),
        _ => {
            if let Some(hex) = action.strip_prefix("wifi:connect:") {
                hex_decode(hex).map(MenuAction::WifiConnect)
            } else if let Some(addr) = action.strip_prefix("bt:connect:") {
                Some(MenuAction::BtConnect(Address::new(addr)))
            } else if let Some(addr) = action.strip_prefix("bt:disconnect:") {
                Some(MenuAction::BtDisconnect(Address::new(addr)))
            } else {
                action
                    .strip_prefix("nightlight:set:")
                    .and_then(|kelvin| kelvin.parse::<i64>().ok())
                    .map(MenuAction::NightLightSet)
            }
        }
    }
}

/// Receives menu clicks and carries out the operation behind each one.
///
/// Runs on its own task, never on the D-Bus dispatch path — `dbusmenu.rs`
/// already hands `event`/`event_group` an unbounded sender and returns,
/// which is what makes that safe to do here: a slow or hanging backend
/// call blocks this loop, not the host waiting on the D-Bus method call.
/// Keeps its own [`Reconnecting`] state, independent of `poll_loop`'s —
/// a second connection to NetworkManager or BlueZ is unremarkable, and
/// sharing one would need a lock held across every backend call this loop
/// makes, for no benefit worth that cost.
/// Asks NetworkManager to scan, then has the menu rebuilt once the
/// results have had time to land.
///
/// The rebuild is spawned rather than awaited: this runs on the task that
/// reads menu events, and sleeping here would stall every click that
/// arrived during the wait.
async fn scan_then_refresh(
    net_state: &mut Reconnecting<hyprforge_network::NetworkManagerBackend>,
    refresh: tokio::sync::mpsc::UnboundedSender<()>,
) {
    let Some(backend) = network_backend(net_state).await else {
        return;
    };
    if let Err(e) = backend.request_scan().await {
        // Not worth a warning at every menu open on a machine with no
        // Wi-Fi; the menu simply shows what is already known.
        tracing::debug!(error = %e, "scan on menu open was refused");
        return;
    }
    tokio::spawn(async move {
        tokio::time::sleep(SCAN_SETTLE).await;
        let _ = refresh.send(());
    });
}

/// How long to wait after asking for a scan before rebuilding the menu.
///
/// NetworkManager answers `RequestScan` immediately and finds access
/// points over the next few seconds, so refreshing straight away would
/// rebuild from the same stale list. This is long enough for results to
/// land and short enough that the menu is fresh by the time someone
/// closes it and opens it again.
/// The ids the two items register under. `TrayItem::activate_screen`
/// matches on these too, so they are not free-form strings.
const NETWORK_ITEM_ID: &str = "hyprforge-network";

const SCAN_SETTLE: std::time::Duration = std::time::Duration::from_secs(3);

async fn handle_menu_clicks(
    mut actions: tokio::sync::mpsc::UnboundedReceiver<String>,
    refresh: tokio::sync::mpsc::UnboundedSender<()>,
    night_light_belief: NightLightBelief,
) {
    let mut net_state = Reconnecting::new();
    let mut bt_state = Reconnecting::new();
    let mut power_state = Reconnecting::new();
    while let Some(action) = actions.recv().await {
        // A menu being opened is an event, not a click, and only the
        // Wi-Fi one wants anything done about it.
        if let Some(id) = action.strip_prefix(hyprforge_tray::dbusmenu::OPENED_PREFIX) {
            if id == NETWORK_ITEM_ID {
                scan_then_refresh(&mut net_state, refresh.clone()).await;
            }
            // Bluetooth deliberately does nothing: starting discovery
            // costs battery on every device in range, and a popup opening
            // is not someone asking for that. Keep awake and night light
            // have nothing analogous to refresh on open either.
            continue;
        }
        let Some(parsed) = parse_menu_action(&action) else {
            tracing::warn!(action = %action, "unknown tray menu action; ignoring");
            continue;
        };
        if let Err(e) =
            perform_menu_action(parsed, &mut net_state, &mut bt_state, &mut power_state, &night_light_belief)
                .await
        {
            tracing::warn!(error = %e, "tray menu action failed");
        }
    }
}

async fn perform_menu_action(
    action: MenuAction,
    net_state: &mut Reconnecting<NetworkManagerBackend>,
    bt_state: &mut Reconnecting<BlueZBackend>,
    power_state: &mut Reconnecting<LogindBackend>,
    night_light_belief: &NightLightBelief,
) -> anyhow::Result<()> {
    match action {
        MenuAction::OpenSettings(screen) => {
            spawn_settings(screen);
            Ok(())
        }
        MenuAction::WifiRadio(on) => {
            let backend = network_backend(net_state)
                .await
                .ok_or_else(|| anyhow::anyhow!("NetworkManager unavailable"))?;
            backend.set_radio(on).await?;
            Ok(())
        }
        MenuAction::WifiConnect(ssid_bytes) => {
            let backend = network_backend(net_state)
                .await
                .ok_or_else(|| anyhow::anyhow!("NetworkManager unavailable"))?;
            let ssid = Ssid::new(ssid_bytes);
            let points = backend.access_points().await?;
            let Some(ap) = points.into_iter().find(|ap| ap.ssid == ssid) else {
                // Out of range since the menu was built, most likely.
                anyhow::bail!("network no longer in range");
            };
            if ap.security.needs_passphrase() {
                let saved = backend.saved_networks().await.unwrap_or_default();
                let Some(saved) = saved.iter().find(|s| s.ssid == ap.ssid) else {
                    // The menu does not offer a `wifi:connect:` for this
                    // case, so reaching it means the network changed out
                    // from under a click already in flight. A tray menu
                    // has nowhere to type a passphrase, so hand it to the
                    // screen that has.
                    spawn_settings("network");
                    return Ok(());
                };
                // `connect` *adds* a connection and needs a passphrase to
                // build one with; it refuses without one. Rejoining a
                // known network is a different operation, on the secret
                // NetworkManager already stores — which is the only way
                // this can work at all, since the daemon never sees a
                // passphrase and must not.
                backend.connect_saved(&saved.id).await?;
                return Ok(());
            }
            // Open and OWE: nothing to type, nothing stored.
            backend.connect(&ap, None).await?;
            Ok(())
        }
        MenuAction::BtRadio(on) => {
            let backend = bluetooth_backend(bt_state)
                .await
                .ok_or_else(|| anyhow::anyhow!("BlueZ unavailable"))?;
            backend.set_powered(on).await?;
            Ok(())
        }
        MenuAction::BtConnect(address) => {
            let backend = bluetooth_backend(bt_state)
                .await
                .ok_or_else(|| anyhow::anyhow!("BlueZ unavailable"))?;
            backend.connect(&address).await?;
            Ok(())
        }
        MenuAction::BtDisconnect(address) => {
            let backend = bluetooth_backend(bt_state)
                .await
                .ok_or_else(|| anyhow::anyhow!("BlueZ unavailable"))?;
            backend.disconnect(&address).await?;
            Ok(())
        }
        MenuAction::KeepAwake(on) => {
            let backend = keep_awake_backend(power_state)
                .await
                .ok_or_else(|| anyhow::anyhow!("systemd-logind unavailable"))?;
            if on {
                backend.take(WhatSet::keep_awake(), KEEP_AWAKE_WHO, KEEP_AWAKE_WHY).await?;
            } else {
                backend.release().await?;
            }
            Ok(())
        }
        MenuAction::NightLightOff => {
            // `hyprctl`/`pgrep` are both synchronous and bounded (see
            // `command::output`'s own timeout) — `spawn_blocking` keeps a
            // slow one from stalling the runtime, not from running
            // forever; nothing here waits without a bound of its own.
            tokio::task::spawn_blocking(|| Hyprsunset.turn_off()).await??;
            *night_light_belief.lock().unwrap_or_else(|e| e.into_inner()) = false;
            Ok(())
        }
        MenuAction::NightLightSet(kelvin) => {
            tokio::task::spawn_blocking(move || Hyprsunset.set_temperature(kelvin)).await??;
            *night_light_belief.lock().unwrap_or_else(|e| e.into_inner()) = true;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyprforge_bluetooth::DeviceKind;
    use hyprforge_tray::menu::ItemKind;

    // The exact freedesktop names this daemon is allowed to hand to a
    // tray host. Not every name the doc comment lists has to appear here
    // — only every name the functions below can actually *return* has to
    // be a member — which is what catches an invented name before it
    // ships: an unresolvable icon name renders as a blank gap, with
    // nothing logged anywhere to say why.
    const NETWORK_ICON_NAMES: &[&str] = &[
        "network-wireless-signal-excellent-symbolic",
        "network-wireless-signal-good-symbolic",
        "network-wireless-signal-ok-symbolic",
        "network-wireless-signal-weak-symbolic",
        "network-wireless-signal-none-symbolic",
        "network-wireless-disconnected-symbolic",
        "dialog-warning",
    ];
    const BLUETOOTH_ICON_NAMES: &[&str] = &["network-bluetooth-activated-symbolic", "network-bluetooth-inactive-symbolic", "dialog-warning"];
    // Every name in this file is resolved against a real installed icon
    // theme by `live_icons.rs`, because an unresolvable name is the
    // quietest failure in this daemon: the bar draws a blank gap, and
    // nothing logs anything at any layer.
    //
    // All four items now draw from the same family on purpose: a
    // `-symbolic` name under `status/` in KDE's Breeze icon set, which
    // this machine's configured theme (Dracula) actually reaches —
    // `Dracula`'s `Inherits=` line names `Papirus-Dark`, `breeze-dark`,
    // `Zafiro`, `ubuntu-mono-dark`, `Mint-X`, `elementary` and `gnome`,
    // but only `breeze-dark` (and, through it, `breeze`) is actually
    // installed here. Adwaita is not in that chain at all, no matter how
    // standard a name of its sounds — it was the wrong place to look for
    // any of these four, which is why picking a name because it "sounds
    // right" is exactly the mistake this whole file's live-checked
    // approach exists to catch.
    //
    // Before this, Wi-Fi alone used the plain (full-colour) name —
    // `network-wireless-signal-*` rather than `-*-symbolic` — which is
    // the one colourful icon the rest of this tray's monochrome row was
    // complained about. It is now `-symbolic` throughout, matching
    // Bluetooth's `network-bluetooth-*-symbolic`, which was already
    // right.
    //
    // `redshift-status-on`/`-off` reads better as GNOME's
    // `night-light-symbolic`, but that name exists only in Adwaita and
    // is unreachable from this chain, exactly like the Adwaita names
    // above; `redshift-status-*-symbolic` is a real file (a symlink to
    // the plain SVG, itself already monochrome enough that Breeze never
    // bothered to draw a separate symbolic version) in both `breeze` and
    // `breeze-dark`, so it is what stays.
    //
    // `changes-prevent-symbolic`/`changes-allow-symbolic` needed no
    // change — already symbolic, already present in `breeze-dark`.
    //
    // `dialog-warning`/`dialog-information` are the shared error
    // fallback all four items use identically when their backend is
    // unavailable, so a full-colour dialog icon appearing on top of an
    // otherwise-symbolic row is not the same defect as one item's normal
    // icon permanently differing from the other three's — it is a rare,
    // uniform, attention-getting state, and Breeze (like hicolor, the
    // end of this chain) ships no `-symbolic` variant of either to
    // switch to. Adwaita has both — `symbolic/status/dialog-warning-
    // symbolic.svg` — but is unreachable here, so naming it would trade
    // a real icon for a blank gap. Left as-is.
    const KEEP_AWAKE_ICON_NAMES: &[&str] =
        &["changes-prevent-symbolic", "changes-allow-symbolic", "dialog-warning"];
    const NIGHT_LIGHT_ICON_NAMES: &[&str] = &[
        "redshift-status-on-symbolic",
        "redshift-status-off-symbolic",
        "dialog-warning",
        "dialog-information",
    ];

    fn net_status(radio: RadioState, connected: Option<&str>) -> NetStatus {
        NetStatus { radio, connected_to: connected.map(Ssid::new) }
    }

    fn bt_status(state: AdapterState) -> BtStatus {
        BtStatus { state, discovering: false, alias: "mock-adapter".to_string() }
    }

    fn device(alias: &str, connected: bool) -> Device {
        Device {
            address: hyprforge_bluetooth::Address::new("AA:BB:CC:DD:EE:FF"),
            alias: alias.to_string(),
            name: Some(alias.to_string()),
            kind: DeviceKind::Headset,
            paired: true,
            trusted: true,
            connected,
            rssi: Some(-50),
        }
    }

    // --- radio off vs on-but-unconnected -----------------------------

    #[test]
    fn a_switched_off_wifi_radio_is_passive_but_on_and_unconnected_is_active() {
        let off = network_item(Some(&net_status(RadioState::Off, None)), None, None, false);
        assert_eq!(off.status, TrayStatus::Passive);

        let hw_off =
            network_item(Some(&net_status(RadioState::HardwareOff, None)), None, None, false);
        assert_eq!(hw_off.status, TrayStatus::Passive, "rfkill also means the radio is not on");

        let on_unconnected = network_item(Some(&net_status(RadioState::On, None)), None, None, false);
        assert_eq!(
            on_unconnected.status,
            TrayStatus::Active,
            "on and unconnected is exactly when someone looks for the icon"
        );
    }

    #[test]
    fn a_switched_off_bluetooth_adapter_is_passive_but_on_and_unconnected_is_active() {
        let off = bluetooth_item(Some(&bt_status(AdapterState::Off)), None, false);
        assert_eq!(off.status, TrayStatus::Passive);

        let blocked = bluetooth_item(Some(&bt_status(AdapterState::HardwareBlocked)), None, false);
        assert_eq!(blocked.status, TrayStatus::Passive);

        let on_unconnected = bluetooth_item(Some(&bt_status(AdapterState::On)), None, false);
        assert_eq!(on_unconnected.status, TrayStatus::Active);
    }

    // --- signal-strength boundaries ------------------------------------

    #[test]
    fn signal_strength_maps_to_the_right_icon_at_its_boundaries() {
        let cases: &[(u8, &str)] = &[
            (100, "network-wireless-signal-excellent-symbolic"),
            (80, "network-wireless-signal-excellent-symbolic"),
            (79, "network-wireless-signal-good-symbolic"),
            (60, "network-wireless-signal-good-symbolic"),
            (59, "network-wireless-signal-ok-symbolic"),
            (40, "network-wireless-signal-ok-symbolic"),
            (39, "network-wireless-signal-weak-symbolic"),
            (20, "network-wireless-signal-weak-symbolic"),
            (19, "network-wireless-signal-none-symbolic"),
            (0, "network-wireless-signal-none-symbolic"),
        ];
        for &(strength, expected) in cases {
            assert_eq!(
                wifi_signal_icon(strength),
                expected,
                "strength {strength} should map to {expected}"
            );
        }
    }

    // --- unavailable daemons say so, distinctly from "nothing connected" --

    #[test]
    fn an_unavailable_network_daemon_produces_an_icon_and_tooltip_that_say_so() {
        let item = network_item(None, None, None, true);
        assert_eq!(item.status, TrayStatus::Active, "must stay visible to be seen at all");
        assert_eq!(item.icon_name, "dialog-warning");
        assert!(item.tooltip_title.to_lowercase().contains("unavailable"));

        let nothing_connected = network_item(Some(&net_status(RadioState::On, None)), None, None, false);
        assert_ne!(
            item.tooltip_title, nothing_connected.tooltip_title,
            "unavailable and merely unconnected must not read the same"
        );
        assert_ne!(item.icon_name, nothing_connected.icon_name);
    }

    #[test]
    fn a_missing_status_reads_the_same_as_an_explicit_unavailable_flag() {
        // Defensive: a caller that forgets to set `unavailable` but has
        // no status at all (nothing was ever polled) must not fall
        // through to a default that looks like "off" or "connected".
        let item = network_item(None, None, None, false);
        assert!(item.tooltip_title.to_lowercase().contains("unavailable"));
    }

    #[test]
    fn an_unavailable_bluetooth_daemon_produces_an_icon_and_tooltip_that_say_so() {
        let item = bluetooth_item(None, None, true);
        assert_eq!(item.status, TrayStatus::Active);
        assert_eq!(item.icon_name, "dialog-warning");
        assert!(item.tooltip_title.to_lowercase().contains("unavailable"));

        let nothing_connected = bluetooth_item(Some(&bt_status(AdapterState::On)), None, false);
        assert_ne!(item.tooltip_title, nothing_connected.tooltip_title);
        assert_ne!(item.icon_name, nothing_connected.icon_name);
    }

    // --- a connected network's tooltip names it ------------------------

    #[test]
    fn a_connected_networks_tooltip_names_it() {
        let ssid = Ssid::new("Coffee Shop");
        let item = network_item(
            Some(&net_status(RadioState::On, Some("Coffee Shop"))),
            Some(&ssid),
            Some(72),
            false,
        );
        assert_eq!(item.tooltip_title, "Coffee Shop");
        assert!(item.tooltip_body.contains("72"));
        assert_eq!(item.icon_name, "network-wireless-signal-good-symbolic");
    }

    #[test]
    fn a_connected_bluetooth_devices_tooltip_names_it() {
        let d = device("WH-1000XM4", true);
        let item = bluetooth_item(Some(&bt_status(AdapterState::On)), Some(&d), false);
        assert_eq!(item.tooltip_title, "WH-1000XM4");
        assert_eq!(item.tooltip_body, "Headset");
    }

    // --- no invented icon names ------------------------------------------

    #[test]
    fn every_network_icon_the_function_can_return_is_a_documented_freedesktop_name() {
        let ssid = Ssid::new("home");
        let items = [
            network_item(None, None, None, true),
            network_item(Some(&net_status(RadioState::Off, None)), None, None, false),
            network_item(Some(&net_status(RadioState::HardwareOff, None)), None, None, false),
            network_item(Some(&net_status(RadioState::On, None)), None, None, false),
            network_item(Some(&net_status(RadioState::On, Some("home"))), Some(&ssid), Some(0), false),
            network_item(Some(&net_status(RadioState::On, Some("home"))), Some(&ssid), Some(19), false),
            network_item(Some(&net_status(RadioState::On, Some("home"))), Some(&ssid), Some(20), false),
            network_item(Some(&net_status(RadioState::On, Some("home"))), Some(&ssid), Some(39), false),
            network_item(Some(&net_status(RadioState::On, Some("home"))), Some(&ssid), Some(40), false),
            network_item(Some(&net_status(RadioState::On, Some("home"))), Some(&ssid), Some(59), false),
            network_item(Some(&net_status(RadioState::On, Some("home"))), Some(&ssid), Some(60), false),
            network_item(Some(&net_status(RadioState::On, Some("home"))), Some(&ssid), Some(79), false),
            network_item(Some(&net_status(RadioState::On, Some("home"))), Some(&ssid), Some(80), false),
            network_item(Some(&net_status(RadioState::On, Some("home"))), Some(&ssid), Some(100), false),
            network_item(Some(&net_status(RadioState::On, Some("home"))), Some(&ssid), None, false),
        ];
        for item in items {
            assert!(
                NETWORK_ICON_NAMES.contains(&item.icon_name.as_str()),
                "invented icon name: {}",
                item.icon_name
            );
        }
    }

    #[test]
    fn every_bluetooth_icon_the_function_can_return_is_a_documented_freedesktop_name() {
        let connected = device("Headset", true);
        let items = [
            bluetooth_item(None, None, true),
            bluetooth_item(Some(&bt_status(AdapterState::Off)), None, false),
            bluetooth_item(Some(&bt_status(AdapterState::HardwareBlocked)), None, false),
            bluetooth_item(Some(&bt_status(AdapterState::On)), None, false),
            bluetooth_item(Some(&bt_status(AdapterState::Changing)), None, false),
            bluetooth_item(Some(&bt_status(AdapterState::On)), Some(&connected), false),
        ];
        for item in items {
            assert!(
                BLUETOOTH_ICON_NAMES.contains(&item.icon_name.as_str()),
                "invented icon name: {}",
                item.icon_name
            );
        }
    }

    // --- click-to-screen mapping, with no process spawned ----------------

    #[test]
    fn a_click_on_either_icon_maps_to_its_own_settings_screen() {
        let network = network_item(Some(&net_status(RadioState::On, None)), None, None, false);
        assert_eq!(network.activate_screen(), Some("network"));

        let bluetooth = bluetooth_item(Some(&bt_status(AdapterState::On)), None, false);
        assert_eq!(bluetooth.activate_screen(), Some("bluetooth"));
    }

    // --- Reconnecting never caches a failure -----------------------------

    #[tokio::test]
    async fn a_failed_connect_is_retried_on_the_next_tick_rather_than_cached() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let attempts = AtomicUsize::new(0);
        let mut state: Reconnecting<u32> = Reconnecting::new();

        // First tick: the backend isn't there yet.
        let first = state
            .get_or_connect(|| async {
                attempts.fetch_add(1, Ordering::SeqCst);
                Err::<u32, &'static str>("not running")
            })
            .await;
        assert!(first.is_err());
        assert!(
            state.backend.is_none(),
            "a failed connect must leave nothing cached, or a later tick would never try again"
        );

        // Second tick: it comes up. This must actually run the connect
        // closure again — a cached-failure design (`Option<Result<..>>`)
        // would skip straight to returning the same error without ever
        // calling it.
        let second = state
            .get_or_connect(|| async {
                attempts.fetch_add(1, Ordering::SeqCst);
                Ok::<u32, &'static str>(42)
            })
            .await;
        assert_eq!(*second.unwrap(), 42);
        assert_eq!(attempts.load(Ordering::SeqCst), 2, "both ticks must have called connect");

        // Third tick: a successful connect *is* cached, so this must not
        // call connect again.
        let calls_before_third = attempts.load(Ordering::SeqCst);
        let third = state
            .get_or_connect(|| async {
                attempts.fetch_add(1, Ordering::SeqCst);
                Ok::<u32, &'static str>(99)
            })
            .await;
        assert_eq!(*third.unwrap(), 42, "a cached success is reused, not replaced");
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            calls_before_third,
            "a cached success must not reconnect"
        );
    }

    // --- icon_action: the pure on/off decision ---------------------------

    #[test]
    fn a_wanted_icon_that_is_not_up_yet_should_be_registered() {
        assert_eq!(icon_action(true, false), IconAction::Register);
    }

    #[test]
    fn an_unwanted_icon_that_is_currently_up_should_be_dropped_not_left_registered() {
        assert_eq!(icon_action(false, true), IconAction::Drop);
    }

    #[test]
    fn an_icon_already_matching_the_preference_is_left_alone_either_way() {
        assert_eq!(icon_action(true, true), IconAction::Keep);
        assert_eq!(icon_action(false, false), IconAction::Keep);
    }

    // --- refresh_prefs: a bad file never changes what's already shown ----

    /// Serialises every test in this module that repoints
    /// `$XDG_CONFIG_HOME` — it's process-global, so two such tests running
    /// concurrently could read each other's temp directory. Same reasoning
    /// as `hyprforge-settings::modules::CONFIG_ENV_LOCK`.
    static CONFIG_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_temp_config_home<T>(f: impl FnOnce(&std::path::Path) -> T) -> T {
        let _lock = CONFIG_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("XDG_CONFIG_HOME");
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", dir.path());
        }
        let out = f(dir.path());
        match previous {
            Some(p) => unsafe { std::env::set_var("XDG_CONFIG_HOME", p) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }
        out
    }

    #[test]
    fn a_missing_tray_toml_refreshes_to_the_defaults() {
        with_temp_config_home(|_dir| {
            let mut current = Prefs { network: false, bluetooth: false, keep_awake: false, night_light: false };
            let mut warned = false;
            refresh_prefs(&mut current, &mut warned);
            assert!(current.network, "a missing file is first run: both icons shown");
            assert!(current.bluetooth);
            assert!(!warned);
        });
    }

    /// The property this whole feature is built around: a `tray.toml` a
    /// typo has broken must not be treated as "hide everything" (or
    /// "show everything") — whatever was already showing keeps showing.
    #[test]
    fn an_unreadable_tray_toml_leaves_the_current_preferences_untouched() {
        with_temp_config_home(|dir| {
            let tray_toml = dir.join("hyprforge").join("tray.toml");
            std::fs::create_dir_all(tray_toml.parent().unwrap()).unwrap();
            std::fs::write(&tray_toml, "network = yes please\n").unwrap();

            let mut current = Prefs { network: true, bluetooth: false, keep_awake: false, night_light: false };
            let mut warned = false;
            refresh_prefs(&mut current, &mut warned);
            assert_eq!(
                current,
                Prefs { network: true, bluetooth: false, keep_awake: false, night_light: false },
                "a failed read must not change what is currently shown"
            );
            assert!(warned, "the failure is reported");
        });
    }

    /// A persistent parse error must warn once, not once per tick — a
    /// daemon that logs the same line every ten seconds forever makes the
    /// one thing worth noticing (a *new* failure) impossible to spot.
    #[test]
    fn a_persistent_parse_failure_warns_only_once() {
        with_temp_config_home(|dir| {
            let tray_toml = dir.join("hyprforge").join("tray.toml");
            std::fs::create_dir_all(tray_toml.parent().unwrap()).unwrap();
            std::fs::write(&tray_toml, "network = yes please\n").unwrap();

            let mut current = Prefs::default();
            let mut warned = false;
            refresh_prefs(&mut current, &mut warned);
            assert!(warned);

            // A second tick against the same broken file must not flip
            // `warned` back to logging again.
            let warned_before = warned;
            refresh_prefs(&mut current, &mut warned);
            assert_eq!(warned, warned_before, "still warned, but not re-logged");

            // Once the file is fixed, a later failure must be able to
            // warn again — `warned` is reset on success, not stuck `true`
            // forever.
            std::fs::write(&tray_toml, "network = false\n").unwrap();
            refresh_prefs(&mut current, &mut warned);
            assert!(!warned, "a successful read clears the suppression");
        });
    }

    // --- menu content: network_menu / bluetooth_menu ---------------------

    use hyprforge_network::Security;

    fn access_point(ssid: &str, security: Security) -> AccessPoint {
        AccessPoint {
            ssid: Ssid::new(ssid),
            bssid: "00:11:22:33:44:55".to_string(),
            strength: 70,
            frequency_mhz: 5180,
            security,
        }
    }

    fn saved(ssid: &str) -> SavedNetwork {
        SavedNetwork { ssid: Ssid::new(ssid), id: format!("conn-{ssid}"), autoconnect: true }
    }

    fn bt_device(alias: &str, paired: bool, connected: bool) -> Device {
        Device {
            address: Address::new("AA:BB:CC:DD:EE:FF"),
            alias: alias.to_string(),
            name: Some(alias.to_string()),
            kind: DeviceKind::Headset,
            paired,
            trusted: paired,
            connected,
            rssi: Some(-50),
        }
    }

    #[test]
    fn the_connected_network_appears_as_a_checked_row() {
        let ssid = Ssid::new("home");
        let menu = network_menu(
            Some(&net_status(RadioState::On, Some("home"))),
            Some(&ssid),
            false,
            &[access_point("home", Security::Wpa2Personal), access_point("cafe", Security::Open)],
            &[saved("home")],
        );
        let row = menu.flatten().into_iter().find(|i| i.label == "home").unwrap();
        assert_eq!(row.kind, ItemKind::Checkmark);
        assert_eq!(row.toggle, Some(true));

        let other = menu.flatten().into_iter().find(|i| i.label == "cafe").unwrap();
        assert_ne!(other.toggle, Some(true));
    }

    #[test]
    fn an_enterprise_network_is_listed_but_disabled_and_says_why() {
        let menu = network_menu(
            Some(&net_status(RadioState::On, None)),
            None,
            false,
            &[access_point("corp-wifi", Security::Enterprise)],
            &[],
        );
        let row = menu.flatten().into_iter().find(|i| i.label.contains("corp-wifi")).unwrap();
        assert!(!row.enabled);
        assert!(row.action.is_none());
        assert!(
            row.label.to_lowercase().contains("enterprise"),
            "row must say why it can't be joined: {}",
            row.label
        );
    }

    #[test]
    fn an_unpaired_device_is_listed_and_hands_pairing_to_the_screen_that_can_do_it() {
        let menu = bluetooth_menu(
            Some(&bt_status(AdapterState::On)),
            false,
            &[bt_device("New Headphones", false, false)],
        );
        let row = menu
            .flatten()
            .into_iter()
            .find(|i| i.label.contains("New Headphones"))
            .expect("an unpaired device is still listed");

        // It was disabled once, when nothing could pair at all. Now
        // something can — just not a menu, which has nowhere to show six
        // digits and ask whether they match.
        assert!(row.enabled, "a row that can do something must be clickable");
        assert_eq!(
            row.action.as_deref(),
            Some("bt:settings"),
            "pairing is handed to the screen, never attempted from the menu"
        );
        assert!(
            row.label.contains("Settings"),
            "the row says where it goes rather than looking like a connect: {}",
            row.label
        );
    }

    #[test]
    fn a_radio_that_is_off_still_produces_a_usable_wifi_menu_with_a_settings_entry() {
        let menu = network_menu(Some(&net_status(RadioState::Off, None)), None, false, &[], &[]);
        assert!(!menu.items.is_empty(), "an empty popup reads as broken");
        assert!(menu.flatten().iter().any(|i| i.label.contains("Network settings")));
    }

    #[test]
    fn a_radio_that_is_off_still_produces_a_usable_bluetooth_menu_with_a_settings_entry() {
        let menu = bluetooth_menu(Some(&bt_status(AdapterState::Off)), false, &[]);
        assert!(!menu.items.is_empty());
        assert!(menu.flatten().iter().any(|i| i.label.contains("Bluetooth settings")));
    }

    #[test]
    fn an_unavailable_network_daemon_produces_a_menu_that_says_so_rather_than_an_empty_one() {
        let menu = network_menu(None, None, true, &[], &[]);
        assert!(!menu.items.is_empty());
        assert!(menu.flatten().iter().any(|i| i.label.to_lowercase().contains("unavailable")));
        assert!(menu.flatten().iter().any(|i| i.label.contains("Network settings")));
    }

    #[test]
    fn an_unavailable_bluetooth_daemon_produces_a_menu_that_says_so_rather_than_an_empty_one() {
        let menu = bluetooth_menu(None, true, &[]);
        assert!(!menu.items.is_empty());
        assert!(menu.flatten().iter().any(|i| i.label.to_lowercase().contains("unavailable")));
        assert!(menu.flatten().iter().any(|i| i.label.contains("Bluetooth settings")));
    }

    /// The one case this whole feature exists to route away from a join
    /// attempt: a network that needs a passphrase this daemon has never
    /// been given anywhere to type.
    #[test]
    fn a_passphrase_network_with_no_saved_connection_routes_to_settings_not_a_join_attempt() {
        let menu = network_menu(
            Some(&net_status(RadioState::On, None)),
            None,
            false,
            &[access_point("neighbour", Security::Wpa2Personal)],
            &[], // nothing saved
        );
        let row = menu.flatten().into_iter().find(|i| i.label == "neighbour").unwrap();
        assert_eq!(row.action.as_deref(), Some("wifi:settings"));
        assert_eq!(parse_menu_action(row.action.as_deref().unwrap()), Some(MenuAction::OpenSettings("network")));
    }

    /// The mirror image of the test above: a saved passphrase network is
    /// offered a real connect action rather than being bounced to
    /// Settings every time.
    #[test]
    fn a_passphrase_network_with_a_saved_connection_gets_a_connect_action() {
        let menu = network_menu(
            Some(&net_status(RadioState::On, None)),
            None,
            false,
            &[access_point("home", Security::Wpa2Personal)],
            &[saved("home")],
        );
        let row = menu.flatten().into_iter().find(|i| i.label == "home").unwrap();
        let action = row.action.as_deref().unwrap();
        assert!(action.starts_with("wifi:connect:"), "expected a connect action, got {action:?}");
    }

    // --- the action vocabulary --------------------------------------------

    /// A menu open is handled before actions are parsed. If it ever fell
    /// through to the parser it would be logged as an unknown action on
    /// every single open — noise that would bury the warnings that
    /// matter.
    /// The watcher only reads the file when this changes, so anything it
    /// cannot see is a change that never takes effect.
    ///
    /// Absence counts: deleting `tray.toml` is a reset to the defaults
    /// and has to act like any other edit.
    #[test]
    fn the_prefs_stamp_sees_a_file_appear_change_and_vanish() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tray.toml");

        assert_eq!(prefs_stamp(&path), None, "a file that is not there");

        std::fs::write(&path, "network = true\n").unwrap();
        let created = prefs_stamp(&path);
        assert!(created.is_some(), "a file that appeared");

        std::fs::write(&path, "network = false\nbluetooth = false\n").unwrap();
        assert_ne!(prefs_stamp(&path), created, "a file whose contents changed");

        std::fs::remove_file(&path).unwrap();
        assert_eq!(prefs_stamp(&path), None, "a file that was deleted");
    }

    #[test]
    fn a_menu_open_is_not_mistaken_for_a_click() {
        let opened = format!(
            "{}{}",
            hyprforge_tray::dbusmenu::OPENED_PREFIX,
            NETWORK_ITEM_ID
        );
        assert!(
            opened.strip_prefix(hyprforge_tray::dbusmenu::OPENED_PREFIX).is_some(),
            "the open event must be recognisable by its prefix"
        );
        assert!(
            parse_menu_action(&opened).is_none(),
            "an open is an event, not an action"
        );
    }

    /// The id in the open event has to be the id the item registers
    /// under, or the Wi-Fi menu opening scans nothing.
    #[test]
    fn the_network_items_id_is_the_one_the_open_event_carries() {
        let (item, _menu) = {
            let status = net_status(RadioState::On, None);
            (network_item(Some(&status), None, None, false), ())
        };
        assert_eq!(item.id, NETWORK_ITEM_ID);
    }

    #[test]
    fn an_unknown_action_string_is_ignored_rather_than_misrouted() {
        assert_eq!(parse_menu_action(""), None);
        assert_eq!(parse_menu_action("wifi:teleport"), None);
        assert_eq!(parse_menu_action("bt:levitate:AA:BB"), None);
        assert_eq!(parse_menu_action("settings"), None, "the vocabulary is domain-prefixed");
    }

    #[test]
    fn every_parseable_action_round_trips_to_a_distinct_operation() {
        assert_eq!(parse_menu_action("wifi:settings"), Some(MenuAction::OpenSettings("network")));
        assert_eq!(parse_menu_action("bt:settings"), Some(MenuAction::OpenSettings("bluetooth")));
        assert_eq!(parse_menu_action("wifi:radio:on"), Some(MenuAction::WifiRadio(true)));
        assert_eq!(parse_menu_action("wifi:radio:off"), Some(MenuAction::WifiRadio(false)));
        assert_eq!(parse_menu_action("bt:radio:on"), Some(MenuAction::BtRadio(true)));
        assert_eq!(parse_menu_action("bt:radio:off"), Some(MenuAction::BtRadio(false)));
        assert_eq!(
            parse_menu_action("wifi:connect:686f6d65"),
            Some(MenuAction::WifiConnect(b"home".to_vec()))
        );
        assert_eq!(
            parse_menu_action("bt:connect:AA:BB:CC:DD:EE:FF"),
            Some(MenuAction::BtConnect(Address::new("AA:BB:CC:DD:EE:FF")))
        );
        assert_eq!(
            parse_menu_action("bt:disconnect:AA:BB:CC:DD:EE:FF"),
            Some(MenuAction::BtDisconnect(Address::new("AA:BB:CC:DD:EE:FF")))
        );
    }

    #[test]
    fn a_malformed_hex_ssid_in_a_connect_action_is_ignored_rather_than_panicking() {
        assert_eq!(parse_menu_action("wifi:connect:zz"), None);
        assert_eq!(parse_menu_action("wifi:connect:abc"), None, "odd length is not valid hex");
    }

    /// The property the task calls out by name: a menu built from real
    /// (mocked) data must never contain an action string that fails to
    /// parse back — that is exactly the failure mode of a typo'd prefix,
    /// which otherwise does nothing when clicked and nothing tells you
    /// why.
    #[test]
    fn every_action_a_built_menu_can_contain_parses_back_to_the_operation_that_built_it() {
        let net_menus = [
            network_menu(None, None, true, &[], &[]),
            network_menu(Some(&net_status(RadioState::Off, None)), None, false, &[], &[]),
            network_menu(Some(&net_status(RadioState::HardwareOff, None)), None, false, &[], &[]),
            network_menu(
                Some(&net_status(RadioState::On, Some("home"))),
                Some(&Ssid::new("home")),
                false,
                &[
                    access_point("home", Security::Wpa2Personal),
                    access_point("open-cafe", Security::Open),
                    access_point("corp", Security::Enterprise),
                    access_point("neighbour", Security::Wpa3Personal),
                ],
                &[saved("home")],
            ),
        ];
        let bt_menus = [
            bluetooth_menu(None, true, &[]),
            bluetooth_menu(Some(&bt_status(AdapterState::Off)), false, &[]),
            bluetooth_menu(Some(&bt_status(AdapterState::HardwareBlocked)), false, &[]),
            bluetooth_menu(
                Some(&bt_status(AdapterState::On)),
                false,
                &[
                    bt_device("Connected Buds", true, true),
                    bt_device("Paired Keyboard", true, false),
                    bt_device("Stranger Phone", false, false),
                ],
            ),
        ];

        for menu in net_menus.iter().chain(bt_menus.iter()) {
            for item in menu.flatten() {
                let Some(action) = item.action.as_deref() else { continue };
                if action.is_empty() {
                    continue;
                }
                assert!(
                    parse_menu_action(action).is_some(),
                    "action {action:?} on row {:?} does not parse back to an operation",
                    item.label
                );
            }
        }
    }

    #[test]
    fn hex_round_trips_arbitrary_bytes_including_non_utf8_ones() {
        let bytes = vec![0u8, 1, 254, 255, b'a'];
        assert_eq!(hex_decode(&hex_encode(&bytes)), Some(bytes));
    }

    // --- keep awake ------------------------------------------------------

    fn inhibitor(who: &str, why: &str) -> InhibitorInfo {
        InhibitorInfo {
            what: "sleep:idle".to_string(),
            who: who.to_string(),
            why: why.to_string(),
            mode: "block".to_string(),
            uid: 1000,
            pid: 4242,
        }
    }

    #[test]
    fn every_keep_awake_icon_the_function_can_return_is_a_documented_freedesktop_name() {
        let other = [inhibitor("systemd-inhibit", "working")];
        let items = [
            keep_awake_item(None, &[], true),
            keep_awake_item(Some(false), &[], false),
            keep_awake_item(Some(true), &[], false),
            keep_awake_item(Some(true), &other, false),
            keep_awake_item(Some(false), &other, false),
        ];
        for item in items {
            assert!(
                KEEP_AWAKE_ICON_NAMES.contains(&item.icon_name.as_str()),
                "invented icon name: {}",
                item.icon_name
            );
        }
    }

    /// A switch has to be visible in both positions.
    ///
    /// This asserted `Passive` while off, on the reasoning that there is
    /// nothing to monitor when nothing is being held. `Passive` asks the
    /// host to *hide* the item and hosts obey, so switching the icon on
    /// in Settings made it appear and then vanish — "not currently
    /// keeping the machine awake" being the state it starts in. The
    /// toggle was unreachable from the bar it lived in.
    ///
    /// The radios are what `Passive` is genuinely for: they hide when the
    /// hardware is off, which is a state chosen elsewhere and not
    /// actionable from a tray icon anyway.
    #[test]
    fn keep_awake_stays_visible_whether_or_not_it_is_holding_the_machine_awake() {
        let off = keep_awake_item(Some(false), &[], false);
        assert_eq!(
            off.status,
            TrayStatus::Active,
            "a switch the user cannot see is a switch they cannot flip"
        );
        assert_eq!(on_icon(&off), "changes-allow-symbolic");

        let on = keep_awake_item(Some(true), &[], false);
        assert_eq!(on.status, TrayStatus::Active);
        assert_eq!(on_icon(&on), "changes-prevent-symbolic");
    }

    /// The two states still look different, which is the whole reason
    /// the icon is worth having when it is always visible.
    fn on_icon(item: &TrayItem) -> &str {
        &item.icon_name
    }

    #[test]
    fn an_unavailable_logind_produces_an_icon_and_tooltip_that_say_so() {
        let item = keep_awake_item(None, &[], true);
        assert_eq!(item.status, TrayStatus::Active, "must stay visible to be seen at all");
        assert!(item.tooltip_title.to_lowercase().contains("unavailable"));
    }

    /// The property this daemon's own filtering exists for: logind
    /// reports every inhibitor it holds, ours included, so `main`'s "who"
    /// string has to be filtered back out before it reaches a menu built
    /// to show *other* holders.
    #[test]
    fn our_own_inhibitor_is_excluded_from_the_list_of_other_holders() {
        let all = vec![inhibitor(KEEP_AWAKE_WHO, KEEP_AWAKE_WHY), inhibitor("mpv", "playing a video")];
        let others = other_inhibitors(all);
        assert_eq!(others.len(), 1);
        assert_eq!(others[0].who, "mpv");
    }

    /// Another process's inhibitor shows up in the menu as a disabled
    /// row, and ours never appears a second time next to the checkmark
    /// that already represents it.
    #[test]
    fn another_processs_inhibitor_appears_in_the_keep_awake_menu_and_ours_does_not_appear_twice() {
        let all = vec![inhibitor(KEEP_AWAKE_WHO, KEEP_AWAKE_WHY), inhibitor("mpv", "playing a video")];
        let others = other_inhibitors(all);
        let menu = keep_awake_menu(Some(true), &others, false);
        let rows = menu.flatten();

        let mpv_rows: Vec<_> = rows.iter().filter(|i| i.label.contains("mpv")).collect();
        assert_eq!(mpv_rows.len(), 1, "the other holder should appear exactly once");
        assert!(!mpv_rows[0].enabled, "another process's lock cannot be released from here");

        let own_rows: Vec<_> = rows.iter().filter(|i| i.label.contains(KEEP_AWAKE_WHO)).collect();
        assert!(own_rows.is_empty(), "our own inhibitor must not appear in the disabled list too");
    }

    #[test]
    fn the_keep_awake_menus_checkmark_reflects_whether_we_are_holding_it() {
        let held = keep_awake_menu(Some(true), &[], false);
        let row = held.flatten().into_iter().find(|i| i.kind == ItemKind::Checkmark).unwrap();
        assert_eq!(row.toggle, Some(true));
        assert_eq!(row.action.as_deref(), Some("keepawake:off"));

        let not_held = keep_awake_menu(Some(false), &[], false);
        let row = not_held.flatten().into_iter().find(|i| i.kind == ItemKind::Checkmark).unwrap();
        assert_eq!(row.toggle, Some(false));
        assert_eq!(row.action.as_deref(), Some("keepawake:on"));
    }

    // --- night light -------------------------------------------------------

    #[test]
    fn every_night_light_icon_the_function_can_return_is_a_documented_freedesktop_name() {
        let items = [
            night_light_item(&NightLightState::NotRunning),
            night_light_item(&NightLightState::CouldNotCheck),
            night_light_item(&NightLightState::Known { temperature: 4500, on: true }),
            night_light_item(&NightLightState::Known { temperature: 4500, on: false }),
        ];
        for item in items {
            assert!(
                NIGHT_LIGHT_ICON_NAMES.contains(&item.icon_name.as_str()),
                "invented icon name: {}",
                item.icon_name
            );
        }
    }

    /// The property the task calls out by name: a daemon that is not
    /// running must never be reported the same way as night light being
    /// deliberately switched off, in either the icon or the menu.
    #[test]
    fn hyprsunset_not_running_produces_a_menu_that_says_so_distinct_from_night_light_off() {
        let not_running = night_light_item(&NightLightState::NotRunning);
        let off = night_light_item(&NightLightState::Known { temperature: 4500, on: false });
        assert_ne!(not_running.icon_name, off.icon_name);
        assert_ne!(not_running.tooltip_title, off.tooltip_title);
        assert!(not_running.tooltip_title.to_lowercase().contains("unavailable"));
        assert!(off.tooltip_title.to_lowercase().contains("off"));

        let not_running_menu = night_light_menu(&NightLightState::NotRunning);
        let off_menu = night_light_menu(&NightLightState::Known { temperature: 4500, on: false });
        assert_ne!(not_running_menu, off_menu);
        assert!(not_running_menu
            .flatten()
            .iter()
            .any(|i| i.label.to_lowercase().contains("running")));
        // The "off" menu, unlike the "not running" one, offers a
        // checkmark to act on — there is something here to toggle.
        assert!(off_menu.flatten().iter().any(|i| i.kind == ItemKind::Checkmark));
        assert!(!not_running_menu.flatten().iter().any(|i| i.kind == ItemKind::Checkmark));
    }

    /// The third state: the check itself failing must read differently
    /// from both "not running" and "off", not collapse into either.
    #[test]
    fn a_failed_check_reads_differently_from_not_running_and_from_off() {
        let could_not_check = night_light_item(&NightLightState::CouldNotCheck);
        let not_running = night_light_item(&NightLightState::NotRunning);
        let off = night_light_item(&NightLightState::Known { temperature: 4500, on: false });
        assert_ne!(could_not_check.icon_name, not_running.icon_name);
        assert_ne!(could_not_check.icon_name, off.icon_name);
        assert_ne!(could_not_check.tooltip_title, not_running.tooltip_title);
        assert_ne!(could_not_check.tooltip_title, off.tooltip_title);
    }

    #[test]
    fn a_known_reading_shows_the_temperature_only_while_believed_on() {
        let on = night_light_item(&NightLightState::Known { temperature: 3500, on: true });
        assert!(on.tooltip_body.contains("3500"));
        assert_eq!(on.status, TrayStatus::Active);

        let off = night_light_item(&NightLightState::Known { temperature: 3500, on: false });
        assert_eq!(off.status, TrayStatus::Passive);
        assert!(!off.tooltip_body.contains("3500"), "off has nothing numeric to show");
    }

    #[test]
    fn the_current_preset_is_shown_checked_and_others_are_not() {
        let menu = night_light_menu(&NightLightState::Known { temperature: 3500, on: true });
        let current = menu.flatten().into_iter().find(|i| i.label == "3500K").unwrap();
        assert_eq!(current.toggle, Some(true));

        let other = menu.flatten().into_iter().find(|i| i.label == "4500K").unwrap();
        assert_ne!(other.toggle, Some(true));
    }

    // --- the action vocabulary, extended -----------------------------------

    /// Mirrors `every_action_a_built_menu_can_contain_parses_back_to_the_operation_that_built_it`
    /// for the two new menus, rather than folding them into that test —
    /// each set of menus is built from its own state enum, and keeping
    /// them apart makes it obvious which menu a failure came from.
    #[test]
    fn every_keep_awake_and_night_light_action_parses_back_to_an_operation() {
        let other = [inhibitor("mpv", "playing a video")];
        let keep_awake_menus = [
            keep_awake_menu(None, &[], true),
            keep_awake_menu(Some(false), &[], false),
            keep_awake_menu(Some(true), &other, false),
        ];
        let night_light_menus = [
            night_light_menu(&NightLightState::NotRunning),
            night_light_menu(&NightLightState::CouldNotCheck),
            night_light_menu(&NightLightState::Known { temperature: 4500, on: true }),
            night_light_menu(&NightLightState::Known { temperature: 2700, on: false }),
        ];

        for menu in keep_awake_menus.iter().chain(night_light_menus.iter()) {
            for item in menu.flatten() {
                let Some(action) = item.action.as_deref() else { continue };
                if action.is_empty() {
                    continue;
                }
                assert!(
                    parse_menu_action(action).is_some(),
                    "action {action:?} on row {:?} does not parse back to an operation",
                    item.label
                );
            }
        }
    }

    #[test]
    fn keep_awake_and_night_light_actions_round_trip_to_distinct_operations() {
        assert_eq!(parse_menu_action("keepawake:on"), Some(MenuAction::KeepAwake(true)));
        assert_eq!(parse_menu_action("keepawake:off"), Some(MenuAction::KeepAwake(false)));
        assert_eq!(parse_menu_action("nightlight:off"), Some(MenuAction::NightLightOff));
        assert_eq!(parse_menu_action("nightlight:set:2700"), Some(MenuAction::NightLightSet(2700)));
        assert_eq!(parse_menu_action("nightlight:set:not-a-number"), None);
    }

    // --- the one menu shape, applied to all four ---------------------------

    /// Every menu ends in a settings row, unavailable states included —
    /// the property this whole task was about. Before this, keep-awake
    /// and night-light's unavailable menus offered nothing at all: a
    /// dead end with no way out, the exact thing CLAUDE.md warns against.
    #[test]
    fn every_menus_unavailable_state_still_offers_its_settings_row() {
        let menus = [
            network_menu(None, None, true, &[], &[]),
            bluetooth_menu(None, true, &[]),
            keep_awake_menu(None, &[], true),
            night_light_menu(&NightLightState::NotRunning),
            night_light_menu(&NightLightState::CouldNotCheck),
        ];
        for menu in menus {
            let last = menu.items.last().expect("a menu with no rows at all");
            assert!(last.enabled, "the settings row must still be clickable: {}", last.label);
            assert!(
                last.label.to_lowercase().contains("settings"),
                "the last row of an unavailable menu must be its settings row, got {:?}",
                last.label
            );
        }
    }

    /// All four menus end with a settings row in their ordinary
    /// (available) state too, not only when something is wrong — that
    /// symmetry is the actual complaint this task set out to fix.
    #[test]
    fn every_menus_ordinary_state_ends_with_its_own_settings_row() {
        let net = network_menu(Some(&net_status(RadioState::On, None)), None, false, &[], &[]);
        assert_eq!(net.items.last().unwrap().label, "Network settings…");

        let bt = bluetooth_menu(Some(&bt_status(AdapterState::On)), false, &[]);
        assert_eq!(bt.items.last().unwrap().label, "Bluetooth settings…");

        let ka = keep_awake_menu(Some(false), &[], false);
        assert_eq!(ka.items.last().unwrap().label, "Keep awake settings…");

        let nl = night_light_menu(&NightLightState::Known { temperature: 4500, on: true });
        assert_eq!(nl.items.last().unwrap().label, "Night light settings…");
    }

    /// The settings rows for keep awake and night light are new: neither
    /// menu offered one before this task, because neither destination
    /// existed. Both do now (`--screen idle`, `--screen night-light`),
    /// which is also what a left click on either icon already opens —
    /// see `TrayItem::activate_screen`. This pins the two rows to those
    /// same names, so the menu and the icon can never disagree about
    /// where they send someone.
    #[test]
    fn the_new_settings_rows_open_the_same_screens_a_left_click_on_that_icon_does() {
        let ka = keep_awake_menu(Some(false), &[], false);
        let ka_row = ka.items.last().unwrap();
        assert_eq!(
            parse_menu_action(ka_row.action.as_deref().unwrap()),
            Some(MenuAction::OpenSettings("idle"))
        );

        let nl = night_light_menu(&NightLightState::Known { temperature: 4500, on: true });
        let nl_row = nl.items.last().unwrap();
        assert_eq!(
            parse_menu_action(nl_row.action.as_deref().unwrap()),
            Some(MenuAction::OpenSettings("night-light"))
        );
    }

    /// Toggle labels are all nouns now — "Keep awake" replaces "Keep this
    /// machine awake", to match "Wi-Fi", "Bluetooth" and "Night light".
    #[test]
    fn the_keep_awake_toggles_label_is_a_noun_like_the_other_three() {
        let menu = keep_awake_menu(Some(true), &[], false);
        let toggle = menu.flatten().into_iter().find(|i| i.kind == ItemKind::Checkmark).unwrap();
        assert_eq!(toggle.label, "Keep awake");
    }

    /// A row's id must not move depending on how much content comes
    /// between the toggle and the settings row — the whole reason
    /// `menu.rs` documents `id` as assigned from meaning, not position.
    /// Here: the keep-awake toggle is always row 1, whether or not any
    /// other process is holding an inhibitor.
    #[test]
    fn the_keep_awake_toggles_id_does_not_move_when_other_inhibitors_appear() {
        let alone = keep_awake_menu(Some(true), &[], false);
        let others = [inhibitor("mpv", "playing a video")];
        let with_others = keep_awake_menu(Some(true), &others, false);

        let alone_toggle = alone.flatten().into_iter().find(|i| i.kind == ItemKind::Checkmark).unwrap();
        let with_others_toggle =
            with_others.flatten().into_iter().find(|i| i.kind == ItemKind::Checkmark).unwrap();
        assert_eq!(alone_toggle.id, with_others_toggle.id);
    }
}
