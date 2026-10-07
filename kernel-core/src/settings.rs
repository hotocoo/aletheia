//! The machine's settings (ADR-219): what the operator chose, kept in the namespace and put back
//! when the console starts, instead of constants compiled into every image.
//!
//! The object `settings` holds one `key=value` per line. Unknown keys and malformed lines are
//! skipped by name, never fatal: a settings file written by a newer machine, or edited by hand,
//! must not stop the console from starting. Today it carries:
//!
//! * `resolution=WxH` - the desktop's mode (ADR-196).
//! * `refresh=HZ` - how often the desktop redraws and polls its devices, [`MIN_HZ`]..=[`MAX_HZ`].

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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::String;

    #[test]
    fn settings_round_trip_and_bad_lines_are_skipped_not_fatal() {
        let s = Settings {
            resolution: Some((1024, 768)),
            refresh: Some(120),
        };
        let mut text = String::new();
        s.render(&mut text);
        assert_eq!(text, "resolution=1024x768\nrefresh=120\n");
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
    fn the_pump_rate_is_clamped_to_what_the_console_accepts() {
        assert_eq!(set_pump_hz(5), MIN_HZ);
        assert_eq!(set_pump_hz(100_000), MAX_HZ);
        assert_eq!(set_pump_hz(144), 144);
        assert_eq!(pump_hz(), 144);
        set_pump_hz(DEFAULT_HZ);
    }
}
