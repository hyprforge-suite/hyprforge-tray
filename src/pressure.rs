//! Whether the machine is waiting on something: the kernel's pressure
//! stall information (PSI), for the tray's pressure icon.
//!
//! `/proc/pressure/{cpu,memory,io}` each say what share of the last ten
//! seconds some task spent waiting for that resource (`some avg10`). It
//! is the honest answer to "why does this feel slow": not how busy
//! something is, but whether anything had to wait for it.
//!
//! No polling while the machine is calm. A file opened for writing takes
//! a trigger — "wake me when tasks stall this long in this window" — and
//! `poll` on it sleeps until that happens, so a calm machine costs this
//! icon nothing at all. Measured on this machine (2026-10-18, kernel
//! 7.2): an unprivileged user may set one on each of the three, with a
//! two-second window. Once one fires the icon is read every few seconds
//! until it is calm again, and then it goes back to sleeping.

use std::path::Path;

/// Waiting this share of the time or more is worth a look.
pub const STRAINED: f32 = 10.0;
/// And this much is why things are slow — the mark procman's Recorded
/// tab shades a stretch at.
pub const HEAVY: f32 = 40.0;

/// The trigger each file is given: 200 ms of stall in a 2 s window,
/// the [`STRAINED`] share. Two seconds is the shortest window an
/// unprivileged user may ask for.
pub const TRIGGER: &[u8] = b"some 200000 2000000\0";

/// One reading: the `some avg10` of each resource, in percent.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Pressure {
    pub cpu: f32,
    pub memory: f32,
    pub io: f32,
}

/// How the icon should look for a reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Calm,
    Strained,
    Heavy,
}

/// What the machine is waiting on most, for the tooltip: its name and
/// share.
pub fn worst(p: &Pressure) -> (&'static str, f32) {
    [("memory", p.memory), ("disk", p.io), ("the processor", p.cpu)].into_iter().fold(("the processor", p.cpu), |best, next| if next.1 > best.1 { next } else { best })
}

pub fn level(p: &Pressure) -> Level {
    let (_, share) = worst(p);
    if share >= HEAVY {
        Level::Heavy
    } else if share >= STRAINED {
        Level::Strained
    } else {
        Level::Calm
    }
}

/// `some avg10` from one pressure file's text, or `None` if it has no
/// such line — a kernel built without PSI has no file at all.
pub fn parse(text: &str) -> Option<f32> {
    let line = text.lines().find(|l| l.starts_with("some "))?;
    line.split_whitespace().find_map(|f| f.strip_prefix("avg10="))?.parse().ok()
}

/// The three files under `root` (`/proc/pressure`), or why they could
/// not be read. A kernel without PSI is the usual reason, and is said as
/// such rather than shown as a calm machine.
pub fn read(root: &Path) -> Result<Pressure, String> {
    let one = |name: &str| -> Result<f32, String> {
        let text = std::fs::read_to_string(root.join(name)).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => "this kernel doesn't report pressure".to_string(),
            _ => format!("/proc/pressure/{name} couldn't be read: {e}"),
        })?;
        parse(&text).ok_or_else(|| format!("/proc/pressure/{name} has no \"some\" line"))
    };
    Ok(Pressure { cpu: one("cpu")?, memory: one("memory")?, io: one("io")? })
}

/// Sleeps until pressure crosses [`STRAINED`], then calls `tick` every
/// `every` until a reading is calm again — once more after that, so the
/// icon shows the calm — and goes back to sleeping. Returns when `tick`
/// says to stop, or with why it cannot watch at all: no PSI, or triggers
/// refused, in which case the tray's own minute-long fallback is all the
/// icon gets.
pub fn watch(root: &Path, every: std::time::Duration, mut tick: impl FnMut() -> bool) -> Result<(), String> {
    use std::io::Write;
    use std::os::fd::AsRawFd;
    let mut files = Vec::new();
    for name in ["cpu", "memory", "io"] {
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(root.join(name))
            .map_err(|e| format!("/proc/pressure/{name} couldn't be opened: {e}"))?;
        f.write_all(TRIGGER).map_err(|e| format!("a pressure trigger on {name} was refused: {e}"))?;
        files.push(f);
    }
    loop {
        let mut fds: Vec<libc::pollfd> = files.iter().map(|f| libc::pollfd { fd: f.as_raw_fd(), events: libc::POLLPRI, revents: 0 }).collect();
        // SAFETY: a buffer of valid pollfds this owns.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
        if n < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(format!("waiting on pressure failed: {}", std::io::Error::last_os_error()));
        }
        if fds.iter().any(|p| p.revents & libc::POLLERR != 0) {
            return Err("the pressure files went away".to_string());
        }
        // Strained: follow it until it is calm, then sleep again.
        loop {
            if !tick() {
                return Ok(());
            }
            if read(root).map(|p| level(&p) == Level::Calm).unwrap_or(true) {
                break;
            }
            std::thread::sleep(every);
        }
    }
}

use std::os::unix::fs::OpenOptionsExt;

#[cfg(test)]
mod tests {
    use super::*;

    /// This machine's `/proc/pressure/memory`, 2026-10-18.
    const MEMORY: &str = "some avg10=0.00 avg60=0.00 avg300=0.00 total=3410448\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=3323718\n";

    #[test]
    fn the_some_line_is_the_one_read() {
        assert_eq!(parse(MEMORY), Some(0.0));
        assert_eq!(parse("some avg10=34.20 avg60=12.00 avg300=4.00 total=9\nfull avg10=30.00 avg60=1 avg300=1 total=1"), Some(34.2));
        assert_eq!(parse("full avg10=1.00 avg60=0 avg300=0 total=0"), None, "cpu once had only some; a file with none is not calm");
    }

    #[test]
    fn the_worst_resource_sets_the_level() {
        assert_eq!(level(&Pressure::default()), Level::Calm);
        assert_eq!(level(&Pressure { io: 12.0, ..Pressure::default() }), Level::Strained);
        assert_eq!(level(&Pressure { memory: 45.0, io: 12.0, ..Pressure::default() }), Level::Heavy);
        assert_eq!(worst(&Pressure { cpu: 3.0, memory: 1.0, io: 12.0 }), ("disk", 12.0));
    }

    #[test]
    fn a_kernel_without_pressure_says_so() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read(dir.path()), Err("this kernel doesn't report pressure".to_string()));
    }

    /// This machine's own files, read-only.
    #[test]
    fn this_machines_pressure_reads() {
        match read(Path::new("/proc/pressure")) {
            Ok(p) => assert!(p.cpu >= 0.0 && p.memory >= 0.0 && p.io >= 0.0),
            Err(why) => eprintln!("HYPRFORGE-SKIP: {why}"),
        }
    }
}
