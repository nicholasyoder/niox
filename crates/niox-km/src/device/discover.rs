//! Finding the local keyboards and mice.

use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

use evdev::raw_stream::RawDevice;
use evdev::{EventType, KeyCode};
use tracing::{debug, warn};

use super::{DeviceError, VIRTUAL_DEVICE_NAME};
use crate::caps::{AbsInfo, Capabilities};
use crate::event::{EV_KEY, EV_MSC, EV_REL, EV_SW};

/// An opened (not yet grabbed) keyboard or mouse.
#[derive(Debug)]
pub struct InputDevice {
    pub(super) path: PathBuf,
    pub(super) name: String,
    pub(super) raw: RawDevice,
}

impl InputDevice {
    /// Opens one device node, whatever it is.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let raw = RawDevice::open(&path)?;
        let name = raw.name().unwrap_or_default().to_owned();
        Ok(Self { path, name, raw })
    }

    /// The device node, e.g. `/dev/input/event3`.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The name the device reports.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether this looks like a keyboard or mouse, by NKMS's heuristic:
    /// it can press the left mouse button, or both `A` and `Z`. niox's own
    /// virtual devices never count.
    pub fn is_keyboard_or_mouse(&self) -> bool {
        if self.name == VIRTUAL_DEVICE_NAME {
            return false;
        }
        self.raw.supported_keys().is_some_and(|keys| {
            keys.contains(KeyCode::BTN_LEFT)
                || (keys.contains(KeyCode::KEY_A) && keys.contains(KeyCode::KEY_Z))
        })
    }

    /// What this device can produce, limited to what a uinput device can
    /// replay: keys, relative and absolute axes, `EV_MSC`, switches, and
    /// input properties. LEDs, sounds, force feedback, and `EV_REP` are
    /// left out: they flow toward a device, not from it, and a virtual
    /// device with `EV_REP` would autorepeat on top of the repeats it is
    /// sent.
    pub fn capabilities(&self) -> Capabilities {
        let mut caps = Capabilities::new();
        let raw = &self.raw;
        // Codes come from kernel bitmaps, so they're always in range; an
        // out-of-range one would just be skipped.
        let mut add = |kind: u16, codes: &mut dyn Iterator<Item = u16>| {
            for code in codes {
                let _ = caps.insert(kind, code);
            }
        };
        if let Some(keys) = raw.supported_keys() {
            add(EV_KEY, &mut keys.iter().map(|k| k.0));
        }
        if let Some(rel) = raw.supported_relative_axes() {
            add(EV_REL, &mut rel.iter().map(|r| r.0));
        }
        if let Some(msc) = raw.misc_properties() {
            add(EV_MSC, &mut msc.iter().map(|m| m.0));
        }
        if let Some(sw) = raw.supported_switches() {
            add(EV_SW, &mut sw.iter().map(|s| s.0));
        }
        if raw.supported_events().contains(EventType::ABSOLUTE) {
            match raw.get_absinfo() {
                Ok(axes) => {
                    for (axis, info) in axes {
                        let _ = caps.insert_abs(
                            axis.0,
                            AbsInfo {
                                minimum: info.minimum(),
                                maximum: info.maximum(),
                                fuzz: info.fuzz(),
                                flat: info.flat(),
                                resolution: info.resolution(),
                            },
                        );
                    }
                }
                Err(e) => warn!(path = %self.path.display(), "reading axis ranges: {e}"),
            }
        }
        for prop in raw.properties().iter() {
            let _ = caps.insert_property(prop.0);
        }
        caps
    }
}

/// Opens every keyboard and mouse under `/dev/input`, ordered by path.
///
/// Devices that can't be opened are skipped with a warning (it is normally
/// a permissions problem, and the log says so); only failing to list the
/// directory is an error.
pub fn discover() -> Result<Vec<InputDevice>, DeviceError> {
    discover_in(Path::new("/dev/input"))
}

fn discover_in(dir: &Path) -> Result<Vec<InputDevice>, DeviceError> {
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)
        .map_err(DeviceError::List)?
        .filter_map(Result::ok)
        .filter(|e| e.file_name().as_bytes().starts_with(b"event"))
        .map(|e| e.path())
        .collect();
    paths.sort_by_key(|p| event_number(p));

    let mut devices = Vec::new();
    for path in paths {
        match InputDevice::open(&path) {
            Ok(dev) if dev.is_keyboard_or_mouse() => devices.push(dev),
            Ok(dev) => debug!(path = %path.display(), name = dev.name, "not a keyboard or mouse"),
            Err(e) => warn!(path = %path.display(), "can't open input device: {e}"),
        }
    }
    Ok(devices)
}

/// `event12` sorts after `event2`.
fn event_number(path: &Path) -> (u64, PathBuf) {
    let n = path
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_prefix("event"))
        .and_then(|n| n.parse().ok())
        .unwrap_or(u64::MAX);
    (n, path.to_path_buf())
}

/// Everything any of `devices` can produce.
pub fn union_capabilities<'a>(devices: impl IntoIterator<Item = &'a InputDevice>) -> Capabilities {
    let mut caps = Capabilities::new();
    for dev in devices {
        caps.union_with(&dev.capabilities());
    }
    caps
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_nodes_sort_numerically() {
        let mut paths: Vec<PathBuf> = ["event10", "event2", "event1", "eventx"]
            .iter()
            .map(|n| Path::new("/dev/input").join(n))
            .collect();
        paths.sort_by_key(|p| event_number(p));
        let names: Vec<_> = paths
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap())
            .collect();
        assert_eq!(names, ["event1", "event2", "event10", "eventx"]);
    }

    #[test]
    fn missing_directory_is_an_error() {
        assert!(matches!(
            discover_in(Path::new("/nonexistent/niox")),
            Err(DeviceError::List(_))
        ));
    }
}
