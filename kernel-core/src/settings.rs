//! The machine's settings (ADR-219): what the operator chose, kept in the namespace and put back
//! when the console starts, instead of constants compiled into every image.
//!
//! The object `settings` holds one `key=value` per line. Unknown keys and malformed lines are
//! skipped by name, never fatal: a settings file written by a newer machine, or edited by hand,
//! must not stop the console from starting. Today it carries:
//!
//! * `resolution=WxH` - the desktop's mode (ADR-196).
//! * `refresh=HZ` - how often the desktop redraws and polls its devices, [`MIN_HZ`]..=[`MAX_HZ`].
//! * `autostart=NAME` - a program the console starts in the background before its first prompt
//!   (ADR-221), up to [`crate::jobs::MAX_JOBS`] of them, in order: the operator's own additions
//!   to the machine.
//! * `persona=NAME` - the desktop's look (ADR-220): Aletheia's own, or a Windows-, macOS- or
//!   GNOME-like layout.

use core::sync::atomic::{AtomicU32, Ordering};

/// The namespace object the settings live in.
pub const OBJECT: &str = "settings";
/// The slowest and fastest desktop refresh the console accepts.
pub const MIN_HZ: u32 = 30;
pub const MAX_HZ: u32 = 1000;
/// The refresh every target booted with before this was a setting.
pub const DEFAULT_HZ: u32 = 1000;

static PUMP_HZ: AtomicU32 = AtomicU32::new(DEFAULT_HZ);

/// How often the desktop is pumped now.
pub fn pump_hz() -> u32 {
    PUMP_HZ.load(Ordering::Relaxed)
}

/// Record the pump rate a target has switched to. Clamped to the accepted range.
pub fn set_pump_hz(hz: u32) -> u32 {
    let hz = hz.clamp(MIN_HZ, MAX_HZ);
    PUMP_HZ.store(hz, Ordering::Relaxed);
    hz
}

/// What the `settings` object says.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Settings {
    pub resolution: Option<(u32, u32)>,
    pub refresh: Option<u32>,
    pub persona: Option<crate::persona::ShellPersona>,
    /// Programs to start at boot, in order (ADR-221).
    pub autostart: [Option<crate::jobs::JobName>; crate::jobs::MAX_JOBS],
}

impl Settings {
    /// Add `name` to the programs started at boot; `false` when it is there already or the list
    /// is full.
    pub fn add_autostart(&mut self, name: &str) -> bool {
        if self.autostarts().any(|n| n.as_str() == name) {
            return false;
        }
        match self.autostart.iter_mut().find(|s| s.is_none()) {
            Some(slot) => {
                *slot = Some(crate::jobs::JobName::new(name));
                true
            }
            None => false,
        }
    }

    /// Take `name` off the programs started at boot; `false` when it was not there. The rest keep
    /// their order.
    pub fn remove_autostart(&mut self, name: &str) -> bool {
        let Some(i) = self
            .autostart
            .iter()
            .position(|s| s.is_some_and(|n| n.as_str() == name))
        else {
            return false;
        };
        self.autostart.copy_within(i + 1.., i);
        self.autostart[crate::jobs::MAX_JOBS - 1] = None;
        true
    }

    /// The programs started at boot, in order.
    pub fn autostarts(&self) -> impl Iterator<Item = &crate::jobs::JobName> {
        self.autostart.iter().flatten()
    }
}

impl Settings {
    /// Read the object's bytes. Lines that are not `key=value` with a known key and a valid value
    /// are skipped; the count of skipped lines is returned beside the result.
    pub fn parse(bytes: &[u8]) -> (Settings, usize) {
        let mut s = Settings::default();
        let mut skipped = 0;
        for raw in bytes.split(|&b| b == b'\n') {
            let Ok(line) = core::str::from_utf8(raw).map(str::trim) else {
                skipped += 1;
                continue;
            };
            if line.is_empty() {
                continue;
            }
            let parsed = line.split_once('=').and_then(|(k, v)| match k.trim() {
                "resolution" => v
                    .trim()
                    .split_once('x')
                    .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
                    .filter(|&(w, h): &(u32, u32)| w > 0 && h > 0)
                    .map(|r| s.resolution = Some(r)),
                "refresh" => v
                    .trim()
                    .parse::<u32>()
                    .ok()
                    .filter(|hz| (MIN_HZ..=MAX_HZ).contains(hz))
                    .map(|hz| s.refresh = Some(hz)),
                "autostart" => {
                    let name = v.trim();
                    (!name.is_empty() && name.len() <= crate::fs::MAX_NAME)
                        .then(|| s.add_autostart(name))
                        .filter(|&added| added)
                        .map(|_| ())
                }
                "persona" => {
                    crate::persona::ShellPersona::from_label(v.trim()).map(|p| s.persona = Some(p))
                }
                _ => None,
            });
            if parsed.is_none() {
                skipped += 1;
            }
        }
        (s, skipped)
    }

    /// The object's bytes, one line per setting that is set, in a fixed order.
    pub fn render(&self, out: &mut alloc::string::String) {
        use core::fmt::Write;
        out.clear();
        if let Some((w, h)) = self.resolution {
            let _ = writeln!(out, "resolution={}x{}", w, h);
        }
        if let Some(hz) = self.refresh {
            let _ = writeln!(out, "refresh={}", hz);
        }
        if let Some(p) = self.persona {
            let _ = writeln!(out, "persona={}", p.label());
        }
        for name in self.autostarts() {
            let _ = writeln!(out, "autostart={}", name.as_str());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::String;
    use alloc::vec::Vec;

    #[test]
    fn settings_round_trip_and_bad_lines_are_skipped_not_fatal() {
        let s = Settings {
            resolution: Some((1024, 768)),
            refresh: Some(120),
            persona: Some(crate::persona::ShellPersona::Macos),
            ..Default::default()
        };
        let mut text = String::new();
        s.render(&mut text);
        assert_eq!(text, "resolution=1024x768\nrefresh=120\npersona=macos\n");
        assert_eq!(Settings::parse(b"persona=amiga\n").1, 1);
        assert_eq!(Settings::parse(text.as_bytes()), (s, 0));
        let (t, skipped) = Settings::parse(
            b"refresh=5\nresolution=0x10\ncolour=blue\nnot a line\n\n refresh = 60 \n\xff",
        );
        assert_eq!(t.refresh, Some(60));
        assert_eq!(t.resolution, None);
        assert_eq!(skipped, 5);
        assert_eq!(Settings::parse(b""), (Settings::default(), 0));
    }

    #[test]
    fn autostart_programs_keep_their_order_refuse_duplicates_and_cap_at_the_job_table() {
        let mut s = Settings::default();
        assert!(s.add_autostart("clock") && s.add_autostart("draw"));
        assert!(!s.add_autostart("clock"));
        assert!(s.add_autostart("a") && s.add_autostart("b"));
        assert!(!s.add_autostart("c"), "four is the job table's size");
        assert!(s.remove_autostart("clock") && !s.remove_autostart("clock"));
        let names: Vec<&str> = s.autostarts().map(|n| n.as_str()).collect();
        assert_eq!(names, ["draw", "a", "b"]);
        let mut text = String::new();
        s.render(&mut text);
        assert_eq!(text, "autostart=draw\nautostart=a\nautostart=b\n");
        let (back, skipped) = Settings::parse(text.as_bytes());
        assert_eq!((back, skipped), (s, 0));
        // A duplicate or an empty name in the file is a skipped line.
        assert_eq!(
            Settings::parse(b"autostart=x\nautostart=x\nautostart=\n").1,
            2
        );
    }

    #[test]
    fn the_pump_rate_is_clamped_to_what_the_console_accepts() {
        assert_eq!(set_pump_hz(5), MIN_HZ);
        assert_eq!(set_pump_hz(100_000), MAX_HZ);
        assert_eq!(set_pump_hz(144), 144);
        assert_eq!(pump_hz(), 144);
        set_pump_hz(DEFAULT_HZ);
    }
}
