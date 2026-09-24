# hyprforge-tray

A tray icon library over `org.kde.StatusNotifierItem`, plus
`hyprforge-trayd`, the daemon that puts Wi-Fi, Bluetooth, keep-awake,
night-light and battery/power-profile icons in whatever bar is running,
and `hyprforge-traymenu`, which draws their right-click menus.

Part of [Hyprforge](https://github.com/adamrpostjr/hyprforge), a suite of
native Hyprland desktop apps — but it runs alone. Installing this gets
you a tray daemon and nothing else.

## Why this needs no GTK or Qt

A tray icon is a D-Bus object, not a widget drawn by a toolkit. This
crate registers an `org.kde.StatusNotifierItem` with whichever
`StatusNotifierWatcher` a bar runs — the icon a user sees is the bar's
own rendering of a name and a handful of properties read off the bus.
The right-click menu is not left to the bar: `hyprforge-traymenu` draws
it, themed like the rest of the suite and anchored below the bar. The
cost is that a bar with no Hyprforge installed shows the icon and no
menu, where a `com.canonical.dbusmenu` menu would have drawn anywhere;
`src/lib.rs` has the reasoning. That
is what makes it possible inside a suite that forbids GTK and Qt
everywhere else: every other way to put an icon in a tray drags one of
them in.

## What is in here

- **The library** (`src/item.rs`, `src/menu.rs`, `src/sni.rs`,
  `src/launch.rs`, `src/prefs.rs`) — the StatusNotifierItem protocol,
  the menu model, and launching the popup that draws it, and nothing
  else. It knows nothing about Wi-Fi or
  Bluetooth, the same way `hyprforge-ui` knows nothing about Hyprland.
- **`hyprforge-trayd`** (`src/bin/trayd.rs`) — the daemon that joins the
  library to `hyprforge-network`, `hyprforge-bluetooth`,
  `hyprforge-power` (keep awake, over `systemd-logind`; battery and
  power profile, over UPower and `power-profiles-daemon`) and
  `hyprforge-ecosystem::sunset_control` (night light, over
  `hyprctl hyprsunset`), and forwards a click to
  `hyprforge-settings --screen <name>`.
- **`hyprforge-traymenu`** (`src/bin/traymenu/`) — the popup that draws
  an item's right-click menu.

## The shape: backend trait, then the client

`NetworkBackend`, `BluetoothBackend`, `InhibitBackend`,
`BatteryBackend`, `PowerProfilesBackend` and `SunsetBackend` are traits this daemon consumes, each with a mock
behind a `mock` feature — none of that lives in this crate, it is what
`hyprforge-network`, `hyprforge-bluetooth`, `hyprforge-power` and
`hyprforge-ecosystem` each expose. What *does* live here, and is
deliberately structured the same way: which icon a given state
deserves, and which rows a menu needs, are plain functions over plain
data (`network_item`, `bluetooth_item`, `keep_awake_item`,
`night_light_item`, `power_item` and their `*_menu` counterparts in `trayd.rs`, and
the pure model in `src/item.rs` / `src/menu.rs`) — no D-Bus, no bar, no
radio, so the interesting question is testable without any of the
three. Everything else in `trayd.rs` is plumbing: polling the
backends, keeping one `TrayIcon` per item in sync, and re-announcing
when a bar restarts.

## Two failure modes this always names

**A service that is not running is its own state, never an empty
list.** `hyprforge-tray::sni::TrayError::NoWatcher` is what a
`StatusNotifierWatcher` that has never shown up looks like — the daemon
waits and retries rather than exiting — and `trayd.rs`'s
`NightLightState::NotRunning` is kept distinct from "night light is
off": hyprsunset confirmed absent is a different fact from a feature the
user switched off, and collapsing the two would mean the wrong action
(start hyprsunset vs. flip a setting) shows up in the menu.

**A failed connection is never cached.** `trayd.rs`'s `Reconnecting`
holds a backend connection once one succeeds, and only `Ok` is ever
stored — a failed `connect()` leaves nothing behind, so the next poll
tick tries again rather than repeating a stale failure forever. The test
`a_failed_connect_is_retried_on_the_next_tick_rather_than_cached` pins
exactly this.

## Building

```
cargo build --release
```

It depends on five other Hyprforge crates — `hyprforge-paths`,
`hyprforge-network`, `hyprforge-bluetooth`, `hyprforge-power` and
`hyprforge-ecosystem` — taken as git dependencies on the main repository
rather than from crates.io, which is where they will move once they are
published. Nothing else here is Hyprforge-specific.

## Running

`packaging/hyprforge-trayd.service` is a user unit:

```
systemctl --user enable --now hyprforge-trayd
```

`Restart=always` rather than `on-failure`: the daemon waits for a
`StatusNotifierWatcher` rather than exiting when there isn't one yet, so
starting before the bar is not a race it can lose, and a clean exit that
leaves the tray empty is exactly as bad as a crash — it should not read
as a healthy, stopped unit.

## What CI checks, and what it can't

`.github/workflows/ci.yml` builds the crate, runs clippy with warnings
denied, and runs `cargo test`. That is tier 1 only:

- `tests/live_tray.rs` registers a real item against whatever
  `StatusNotifierWatcher` is running and asserts the watcher lists it —
  the visible side effect is an icon appearing in a real bar for a
  fraction of a second. That is the one deliberate exception to this
  suite's "the live tier is read-only" rule: nothing else here writes
  configuration, clicks anything, or starts a discovery session, but
  registering an item *is* putting something in the user's bar, however
  briefly, and there is no way to ask "does a real host accept this"
  without doing exactly that. These tests are `#[ignore]`d and need
  `cargo test -p hyprforge-tray -- --ignored` on a machine with a bar
  running.
- `tests/live_icons.rs` resolves every icon name this daemon can put on
  the bus against the icon themes actually installed — it needs no bus
  or bar, only files on disk, but it does need a real icon theme to be
  present to mean anything, and marks itself skipped
  (`HYPRFORGE-SKIP:`) rather than passing silently when one isn't.
- Nothing here exercises `hyprforge-network` against a real
  NetworkManager, `hyprforge-bluetooth` against real BlueZ, or
  `hyprforge-power` against real logind — those crates' own live tests
  cover that, gated on the service each one actually asks.

A green run means the code is internally consistent, not that a real
bar shows the icon or that this daemon has ever touched a real radio.

## Licence

MIT. See `LICENSE`.
