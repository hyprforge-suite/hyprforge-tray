//! Does a **real tray host** accept what this crate registers?
//!
//! The unit tests check this crate against its own idea of the protocol.
//! Nothing in them can tell whether a bar will actually show the icon —
//! a wrong property type, a missing one, or a bus name in the wrong shape
//! all produce an item that registers happily and never appears.
//!
//! These tests register a real item with whatever `StatusNotifierWatcher`
//! is running and assert the watcher lists it. **The visible side effect
//! is an icon appearing in the user's bar for a fraction of a second**,
//! which is the smallest observable form of "it worked". Nothing else is
//! touched: no configuration is written, nothing is clicked, and the item
//! is dropped at the end of each test.
//!
//! Run with `cargo test -p hyprforge-tray -- --ignored`.

use hyprforge_tray::item::{Category, Status};
use hyprforge_tray::sni::TrayError;
use hyprforge_tray::{TrayIcon, TrayItem};

/// libtest has no skipped state, so a check that could not run says so
/// rather than returning early and printing `ok` like one that passed.
const SKIP_MARKER: &str = "HYPRFORGE-SKIP:";

fn item(id: &str) -> TrayItem {
    TrayItem {
        id: id.to_string(),
        category: Category::Hardware,
        status: Status::Active,
        title: "Hyprforge test item".to_string(),
        icon_name: "network-wireless-signal-good".to_string(),
        tooltip_title: "Hyprforge".to_string(),
        tooltip_body: "live test, disappears immediately".to_string(),
    }
}

/// The claim everything else rests on: a bar accepts this registration.
#[tokio::test]
#[ignore]
async fn a_running_tray_host_accepts_the_item_this_crate_registers() {
    let (clicks, _rx) = tokio::sync::mpsc::unbounded_channel();
    match TrayIcon::register(item("hyprforge-network"), 90, clicks).await {
        Ok(icon) => {
            println!("registered as {}", icon.bus_name());
            assert!(icon.bus_name().starts_with("org.kde.StatusNotifierItem-"));
        }
        Err(TrayError::NoWatcher) => {
            eprintln!("{SKIP_MARKER} no StatusNotifierWatcher is running (no bar with a tray)");
        }
        Err(e) => panic!("a tray host was present but refused the item: {e}"),
    }
}

/// Registering is not the same as being listed. A watcher can accept the
/// call and drop the item — for a malformed bus name, say — so this asks
/// the watcher what it actually holds.
#[tokio::test]
#[ignore]
async fn the_watcher_lists_the_item_after_it_registers() {
    let (clicks, _rx) = tokio::sync::mpsc::unbounded_channel();
    let icon = match TrayIcon::register(item("hyprforge-bluetooth"), 91, clicks).await {
        Ok(icon) => icon,
        Err(TrayError::NoWatcher) => {
            eprintln!("{SKIP_MARKER} no StatusNotifierWatcher is running (no bar with a tray)");
            return;
        }
        Err(e) => panic!("registration failed: {e}"),
    };

    // The watcher records the registration asynchronously; give it a beat
    // rather than racing it.
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;

    let connection = zbus::Connection::session().await.expect("a session bus");
    let listed: Vec<String> = connection
        .call_method(
            Some("org.kde.StatusNotifierWatcher"),
            "/StatusNotifierWatcher",
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &("org.kde.StatusNotifierWatcher", "RegisteredStatusNotifierItems"),
        )
        .await
        .and_then(|m| m.body().deserialize::<zbus::zvariant::Value>().map(|v| {
            Vec::<String>::try_from(v).unwrap_or_default()
        }))
        .unwrap_or_default();

    assert!(
        listed.iter().any(|n| n.contains(icon.bus_name())),
        "the watcher accepted the registration but does not list {}; it holds {listed:?}",
        icon.bus_name()
    );
}

/// Updating must not require re-registering, and must not error when
/// nothing changed — the daemon calls this on every poll tick.
#[tokio::test]
#[ignore]
async fn an_update_reaches_the_host_and_an_unchanged_one_is_cheap() {
    let (clicks, _rx) = tokio::sync::mpsc::unbounded_channel();
    let icon = match TrayIcon::register(item("hyprforge-network"), 92, clicks).await {
        Ok(icon) => icon,
        Err(TrayError::NoWatcher) => {
            eprintln!("{SKIP_MARKER} no StatusNotifierWatcher is running (no bar with a tray)");
            return;
        }
        Err(e) => panic!("registration failed: {e}"),
    };

    icon.update(item("hyprforge-network"))
        .await
        .expect("an identical update is a no-op, not an error");

    let mut changed = item("hyprforge-network");
    changed.icon_name = "network-wireless-signal-weak".to_string();
    changed.status = Status::NeedsAttention;
    icon.update(changed).await.expect("a changed update emits and succeeds");
}

/// The menu, over the wire — or rather, the deliberate absence of one.
///
/// `hyprforge-tray` used to serve `com.canonical.dbusmenu` at
/// `/StatusNotifierItem/Menu` for any item registered with a menu, and a
/// wrong `GetLayout` signature there was invisible: the host would read a
/// menu with no rows and show an empty popup, with nothing logged at
/// either end — which is why this test used to put one on a real bus and
/// read it back. It no longer does either of those things: this crate
/// stopped serving that protocol so that only `hyprforge-traymenu` draws
/// the menu (see `src/lib.rs`'s own module doc for the cost). What this
/// test can still uniquely prove, against a real host rather than this
/// crate's own idea of the protocol, is the property that change depends
/// on: an item registered *with* a menu advertises exactly the same "no
/// menu" that one registered with none does — no `Menu` property at all,
/// not `/` (see the comment in the body for why those differ). If that
/// property ever regresses — a `Menu` property appears again — a
/// spec-compliant bar goes straight back to drawing its own menu
/// alongside `hyprforge-traymenu`'s, which is the exact bug this whole
/// architecture exists to avoid.
#[tokio::test]
#[ignore]
async fn an_item_registered_with_a_menu_still_advertises_no_menu_over_d_bus() {
    use hyprforge_tray::menu::{Menu, MenuItem};

    let (clicks, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (menu_clicks, _mrx) = tokio::sync::mpsc::unbounded_channel();
    let menu = Menu::new(vec![
        MenuItem::checkmark("Wi-Fi", true, "radio:toggle"),
        MenuItem::separator(),
        MenuItem::standard("home", "connect:home"),
        MenuItem::standard("Network settings…", "settings"),
    ]);

    let icon = match TrayIcon::register_with_menu(
        item("hyprforge-network"),
        menu,
        93,
        clicks,
        menu_clicks,
    )
    .await
    {
        Ok(icon) => icon,
        Err(TrayError::NoWatcher) => {
            eprintln!("{SKIP_MARKER} no StatusNotifierWatcher is running (no bar with a tray)");
            return;
        }
        Err(e) => panic!("registration failed: {e}"),
    };

    let connection = zbus::Connection::session().await.expect("a session bus");

    // Not "Menu answers `/`" — **there must be no `Menu` property at
    // all**, and the difference is the whole bug this test was rewritten
    // for. Answering `/` looked like the spec's way of saying "no menu";
    // waybar instead saw a property, built a dbusmenu client against the
    // path it named, got nothing, drew an empty four-pixel GTK menu at
    // the pointer, and — believing it had served the click — never
    // called `ContextMenu`, so `hyprforge-traymenu` never opened.
    //
    // Reading a property that is not declared is an error, and that
    // error is the assertion.
    let reply = connection
        .call_method(
            Some(icon.bus_name()),
            "/StatusNotifierItem",
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &("org.kde.StatusNotifierItem", "Menu"),
        )
        .await;
    assert!(
        reply.is_err(),
        "the item must declare no Menu property at all — a host that sees one builds \
         its own menu from it and never calls ContextMenu, which is where \
         hyprforge-traymenu is launched"
    );

    // And nothing answers `com.canonical.dbusmenu` at the old path either
    // — a host that ignored `Menu` and asked anyway (unlikely, but the
    // whole point of a live test is not assuming) must find nothing
    // there, not a stale object this crate forgot to stop serving.
    let reply = connection
        .call_method(
            Some(icon.bus_name()),
            "/StatusNotifierItem/Menu",
            Some("com.canonical.dbusmenu"),
            "GetLayout",
            &(0i32, -1i32, Vec::<String>::new()),
        )
        .await;
    assert!(
        reply.is_err(),
        "com.canonical.dbusmenu must not be served at all any more; got {reply:?}"
    );
}
