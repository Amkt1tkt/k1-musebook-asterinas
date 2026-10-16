// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: write /sys/class backlight brightness the way systemd-logind does.

//! Write `/sys/class/<subsystem>/<name>/brightness` the way systemd-logind
//! `Session.SetBrightness` does, with the arguments treated as untrusted.
//!
//! COSMIC calls `SetBrightness(subsystem, name, value)` on
//! `/org/freedesktop/login1/session/auto`. The binary requires a successful
//! backlight probe before publishing its interfaces. Keep the probe independent
//! of D-Bus so failures are testable.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

/// Subsystems systemd-logind accepts for `SetBrightness`.
///
/// Fact: systemd `src/login/logind-session-dbus.c` `method_set_brightness`
/// (`STR_IN_SET(subsystem, "backlight", "leds")`). COSMIC's display path
/// only ever sends `"backlight"` (`choose_best_backlight` →
/// `BrightnessDevice::new("backlight", …)`). `leds` is here so a busctl
/// caller matching the real interface is not rejected; it is not a
/// COSMIC requirement.
pub const ALLOWED_SUBSYSTEMS: &[&str] = &["backlight", "leds"];

/// Why `subsystem` / `name` from D-Bus will not be turned into a path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reject {
    SubsystemNotAllowed { subsystem: String },
    EmptyName,
    NameHasSlash { name: String },
    NameHasDotDot { name: String },
    NameIsDot,
    NameHasNul { name: String },
}

impl std::fmt::Display for Reject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SubsystemNotAllowed { subsystem } => {
                write!(f, "subsystem {subsystem:?} is not backlight or leds")
            }
            Self::EmptyName => write!(f, "device name is empty"),
            Self::NameHasSlash { name } => {
                write!(f, "device name {name:?} contains '/'")
            }
            Self::NameHasDotDot { name } => {
                write!(f, "device name {name:?} contains '..'")
            }
            Self::NameIsDot => write!(f, "device name is '.'"),
            Self::NameHasNul { name } => {
                write!(f, "device name {name:?} contains NUL")
            }
        }
    }
}

/// Why a sysfs integer attribute could not be parsed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseError {
    Empty,
    NotInteger { raw: String },
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "sysfs text is empty after trim"),
            Self::NotInteger { raw } => {
                write!(f, "sysfs text is not a u32 (got {raw:?})")
            }
        }
    }
}

/// Why `SetBrightness` will not write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SetError {
    Reject(Reject),
    DeviceMissing { path: PathBuf },
    MaxMissing { path: PathBuf },
    MaxUnreadable { path: PathBuf, reason: String },
    MaxUnparseable { path: PathBuf, raw: String },
    WriteFailed { path: PathBuf, reason: String },
}

impl std::fmt::Display for SetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Reject(r) => write!(f, "{r}"),
            Self::DeviceMissing { path } => {
                write!(f, "no device directory at {}", path.display())
            }
            Self::MaxMissing { path } => {
                write!(f, "{} has no max_brightness", path.display())
            }
            Self::MaxUnreadable { path, reason } => {
                write!(f, "cannot read {}/max_brightness: {reason}", path.display())
            }
            Self::MaxUnparseable { path, raw } => {
                write!(
                    f,
                    "{}/max_brightness is not a u32 (got {raw:?})",
                    path.display()
                )
            }
            Self::WriteFailed { path, reason } => {
                write!(f, "cannot write {}: {reason}", path.display())
            }
        }
    }
}

impl From<Reject> for SetError {
    fn from(r: Reject) -> Self {
        Self::Reject(r)
    }
}

/// Why we will not claim `org.freedesktop.login1`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Absent {
    NoClass { path: PathBuf },
    NotADirectory { path: PathBuf },
    Unreadable { path: PathBuf, reason: String },
    NoUsableDevice { path: PathBuf },
}

impl std::fmt::Display for Absent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoClass { path } => {
                write!(f, "no backlight class at {}", path.display())
            }
            Self::NotADirectory { path } => {
                write!(f, "{} exists but is not a directory", path.display())
            }
            Self::Unreadable { path, reason } => {
                write!(f, "cannot read {}: {reason}", path.display())
            }
            Self::NoUsableDevice { path } => {
                write!(
                    f,
                    "{} has no device with readable max_brightness and a brightness file",
                    path.display()
                )
            }
        }
    }
}

/// First usable `/sys/class/backlight/<name>` found at claim time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Probe {
    pub name: String,
    pub max_brightness: u32,
}

/// Exact match against [`ALLOWED_SUBSYSTEMS`]. Case-sensitive: sysfs
/// class names are lowercase, and COSMIC sends `"backlight"` literally.
pub fn validate_subsystem(subsystem: &str) -> Result<(), Reject> {
    if ALLOWED_SUBSYSTEMS.contains(&subsystem) {
        Ok(())
    } else {
        Err(Reject::SubsystemNotAllowed {
            subsystem: subsystem.to_owned(),
        })
    }
}

/// Reject anything that is not a single sysfs device name.
///
/// `subsystem` / `name` arrive from D-Bus. We never pass them through
/// `Path::join` until this returns `Ok`, so `../` and `/etc/passwd`
/// cannot walk out of `sysfs_root`.
///
/// Reject empty names, separators, parent-directory patterns, `.` and NUL.
pub fn validate_name(name: &str) -> Result<(), Reject> {
    if name.is_empty() {
        return Err(Reject::EmptyName);
    }
    if name.contains('\0') {
        return Err(Reject::NameHasNul {
            name: name.to_owned(),
        });
    }
    if name.contains('/') {
        return Err(Reject::NameHasSlash {
            name: name.to_owned(),
        });
    }
    if name.contains("..") {
        return Err(Reject::NameHasDotDot {
            name: name.to_owned(),
        });
    }
    if name == "." {
        return Err(Reject::NameIsDot);
    }
    Ok(())
}

/// `{sysfs_root}/{subsystem}/{name}` after the arguments have been
/// checked. `sysfs_root` is `/sys/class` in production.
pub fn device_dir(sysfs_root: &Path, subsystem: &str, name: &str) -> Result<PathBuf, Reject> {
    validate_subsystem(subsystem)?;
    validate_name(name)?;
    let mut p = sysfs_root.to_path_buf();
    p.push(subsystem);
    p.push(name);
    Ok(p)
}

/// Linux sysfs integer: trailing newline / spaces, no other junk.
///
/// COSMIC's own reader is `u32::from_str(value.trim())`
/// (`brightness_device.rs` `BrightnessDevice::new`). Match that, so a
/// file the daemon already accepted is a file we accept.
pub fn parse_sysfs_u32(raw: &str) -> Result<u32, ParseError> {
    let t = raw.trim();
    if t.is_empty() {
        return Err(ParseError::Empty);
    }
    t.parse()
        .map_err(|_| ParseError::NotInteger { raw: t.to_owned() })
}

/// logind does not clamp (it writes and lets the kernel return EINVAL
/// above max). This shim clamps: COSMIC swallows the D-Bus error
/// (`_ = brightness_device.set_brightness(...)`), so a reject looks
/// like a dead slider. Clamping makes a 256-on-a-255-device succeed.
pub fn clamp_brightness(value: u32, max: u32) -> u32 {
    value.min(max)
}

fn read_max_brightness(dir: &Path) -> Result<u32, SetError> {
    let path = dir.join("max_brightness");
    match fs::read_to_string(&path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Err(SetError::MaxMissing {
            path: dir.to_path_buf(),
        }),
        Err(e) => Err(SetError::MaxUnreadable {
            path: dir.to_path_buf(),
            reason: e.to_string(),
        }),
        Ok(raw) => parse_sysfs_u32(&raw).map_err(|e| match e {
            ParseError::Empty => SetError::MaxUnparseable {
                path: dir.to_path_buf(),
                raw: String::new(),
            },
            ParseError::NotInteger { raw } => SetError::MaxUnparseable {
                path: dir.to_path_buf(),
                raw,
            },
        }),
    }
}

/// Validate, clamp to `max_brightness`, write `brightness`.
///
/// Returns the value actually written. The write is `{n}\n`, which is
/// what `echo n > brightness` produces and what the kernel's
/// `kstrtouint` accepts.
pub fn set_brightness(
    sysfs_root: &Path,
    subsystem: &str,
    name: &str,
    value: u32,
) -> Result<u32, SetError> {
    let dir = device_dir(sysfs_root, subsystem, name)?;
    match fs::metadata(&dir) {
        Err(_) => {
            return Err(SetError::DeviceMissing { path: dir });
        }
        Ok(m) if !m.is_dir() => {
            return Err(SetError::DeviceMissing { path: dir });
        }
        Ok(_) => {}
    }
    let max = read_max_brightness(&dir)?;
    let clamped = clamp_brightness(value, max);
    let brightness = dir.join("brightness");
    fs::write(&brightness, format!("{clamped}\n")).map_err(|e| SetError::WriteFailed {
        path: brightness,
        reason: e.to_string(),
    })?;
    Ok(clamped)
}

/// Look under `{sysfs_root}/backlight/` for one device the daemon could
/// pick. Claim gate: `Err` means the binary must not request the name.
///
/// Only `backlight` is probed. COSMIC's slider is the display path;
/// `leds` is accepted later by `set_brightness` if someone calls it.
pub fn probe_backlight(sysfs_root: &Path) -> Result<Probe, Absent> {
    let class = sysfs_root.join("backlight");
    match fs::metadata(&class) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(Absent::NoClass { path: class });
        }
        Err(e) => {
            return Err(Absent::Unreadable {
                path: class,
                reason: e.to_string(),
            });
        }
        Ok(m) if !m.is_dir() => {
            return Err(Absent::NotADirectory { path: class });
        }
        Ok(_) => {}
    }
    let entries = match fs::read_dir(&class) {
        Ok(e) => e,
        Err(e) => {
            return Err(Absent::Unreadable {
                path: class,
                reason: e.to_string(),
            });
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let name = match entry.file_name().into_string() {
            Ok(n) => n,
            Err(_) => continue,
        };
        if validate_name(&name).is_err() {
            continue;
        }
        let dir = class.join(&name);
        if !dir.is_dir() {
            continue;
        }
        if !dir.join("brightness").is_file() {
            continue;
        }
        if let Ok(max_brightness) = read_max_brightness(&dir) {
            return Ok(Probe {
                name,
                max_brightness,
            });
        }
    }
    Err(Absent::NoUsableDevice { path: class })
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::fs::symlink,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

    static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

    struct TempClass {
        root: PathBuf,
    }

    impl TempClass {
        fn new() -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir()
                .join(format!("logind-shim-{}-{seq}-{nanos}", std::process::id()));
            fs::create_dir_all(&root).unwrap();
            Self { root }
        }

        fn add_device(&self, subsystem: &str, name: &str, files: &[(&str, &str)]) -> PathBuf {
            let dir = self.root.join(subsystem).join(name);
            fs::create_dir_all(&dir).unwrap();
            for (fname, body) in files {
                fs::write(dir.join(fname), body).unwrap();
            }
            dir
        }
    }

    impl Drop for TempClass {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn subsystem_whitelist() {
        assert!(validate_subsystem("backlight").is_ok());
        assert!(validate_subsystem("leds").is_ok());
        assert_eq!(
            validate_subsystem("Backlight"),
            Err(Reject::SubsystemNotAllowed {
                subsystem: "Backlight".into()
            })
        );
        assert_eq!(
            validate_subsystem("power_supply"),
            Err(Reject::SubsystemNotAllowed {
                subsystem: "power_supply".into()
            })
        );
        assert_eq!(
            validate_subsystem("backlight/../etc"),
            Err(Reject::SubsystemNotAllowed {
                subsystem: "backlight/../etc".into()
            })
        );
        assert_eq!(
            validate_subsystem(""),
            Err(Reject::SubsystemNotAllowed {
                subsystem: "".into()
            })
        );
    }

    #[test]
    fn name_rejects_slash_and_dotdot() {
        assert!(validate_name("lcd_backlight").is_ok());
        assert!(validate_name("intel_backlight").is_ok());
        assert_eq!(validate_name(""), Err(Reject::EmptyName));
        assert_eq!(
            validate_name("foo/bar"),
            Err(Reject::NameHasSlash {
                name: "foo/bar".into()
            })
        );
        assert_eq!(
            validate_name("/etc"),
            Err(Reject::NameHasSlash {
                name: "/etc".into()
            })
        );
        assert_eq!(
            validate_name(".."),
            Err(Reject::NameHasDotDot { name: "..".into() })
        );
        assert_eq!(
            validate_name("../etc"),
            Err(Reject::NameHasSlash {
                name: "../etc".into()
            })
        );
        assert_eq!(
            validate_name("foo/../bar"),
            Err(Reject::NameHasSlash {
                name: "foo/../bar".into()
            })
        );
        assert_eq!(
            validate_name("foo..bar"),
            Err(Reject::NameHasDotDot {
                name: "foo..bar".into()
            })
        );
        assert_eq!(validate_name("."), Err(Reject::NameIsDot));
        assert_eq!(
            validate_name("a\0b"),
            Err(Reject::NameHasNul {
                name: "a\0b".into()
            })
        );
    }

    #[test]
    fn device_dir_does_not_escape_sysfs_root() {
        let root = Path::new("/sys/class");
        let ok = device_dir(root, "backlight", "lcd_backlight").unwrap();
        assert_eq!(ok, PathBuf::from("/sys/class/backlight/lcd_backlight"));

        assert!(device_dir(root, "backlight", "..").is_err());
        assert!(device_dir(root, "backlight", "../shadow").is_err());
        assert!(device_dir(root, "backlight", "a/b").is_err());
        assert!(device_dir(root, "firmware", "lcd_backlight").is_err());

        // After a rejected name we never produce a PathBuf the caller
        // could still join. The `Err` is the whole API.
        match device_dir(root, "backlight", "../../etc") {
            Err(Reject::NameHasSlash { .. }) | Err(Reject::NameHasDotDot { .. }) => {}
            other => panic!("expected slash or dotdot, got {other:?}"),
        }
    }

    #[test]
    fn parse_max_brightness_trims_and_rejects_junk() {
        assert_eq!(parse_sysfs_u32("255\n"), Ok(255));
        assert_eq!(parse_sysfs_u32("  100  "), Ok(100));
        assert_eq!(parse_sysfs_u32("7\r\n"), Ok(7));
        assert_eq!(parse_sysfs_u32("0"), Ok(0));
        assert_eq!(parse_sysfs_u32(""), Err(ParseError::Empty));
        assert_eq!(parse_sysfs_u32("   \n"), Err(ParseError::Empty));
        assert_eq!(
            parse_sysfs_u32("abc"),
            Err(ParseError::NotInteger { raw: "abc".into() })
        );
        assert_eq!(
            parse_sysfs_u32("-1"),
            Err(ParseError::NotInteger { raw: "-1".into() })
        );
        assert_eq!(
            parse_sysfs_u32("255 extra"),
            Err(ParseError::NotInteger {
                raw: "255 extra".into()
            })
        );
        assert!(parse_sysfs_u32("4294967296").is_err());
    }

    #[test]
    fn clamp_to_max_inclusive_zero() {
        assert_eq!(clamp_brightness(40, 255), 40);
        assert_eq!(clamp_brightness(255, 255), 255);
        assert_eq!(clamp_brightness(256, 255), 255);
        assert_eq!(clamp_brightness(u32::MAX, 255), 255);
        assert_eq!(clamp_brightness(0, 255), 0);
        assert_eq!(clamp_brightness(7, 0), 0);
    }

    #[test]
    fn set_brightness_writes_clamped_value() {
        let tmp = TempClass::new();
        tmp.add_device(
            "backlight",
            "lcd_backlight",
            &[("max_brightness", "255\n"), ("brightness", "255\n")],
        );
        let written = set_brightness(&tmp.root, "backlight", "lcd_backlight", 40).unwrap();
        assert_eq!(written, 40);
        let raw = fs::read_to_string(tmp.root.join("backlight/lcd_backlight/brightness")).unwrap();
        assert_eq!(raw, "40\n");

        let written = set_brightness(&tmp.root, "backlight", "lcd_backlight", 400).unwrap();
        assert_eq!(written, 255);
        let raw = fs::read_to_string(tmp.root.join("backlight/lcd_backlight/brightness")).unwrap();
        assert_eq!(raw, "255\n");
    }

    #[test]
    fn set_brightness_accepts_leds() {
        let tmp = TempClass::new();
        tmp.add_device(
            "leds",
            "kbd::white",
            &[("max_brightness", "3\n"), ("brightness", "0\n")],
        );
        let written = set_brightness(&tmp.root, "leds", "kbd::white", 2).unwrap();
        assert_eq!(written, 2);
    }

    #[test]
    fn set_brightness_rejects_unknown_device() {
        let tmp = TempClass::new();
        tmp.add_device(
            "backlight",
            "lcd_backlight",
            &[("max_brightness", "255\n"), ("brightness", "255\n")],
        );
        match set_brightness(&tmp.root, "backlight", "no_such", 10) {
            Err(SetError::DeviceMissing { .. }) => {}
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn set_brightness_rejects_traversal_before_any_io() {
        let tmp = TempClass::new();
        // A sibling the join would reach if `..` were accepted.
        fs::create_dir_all(tmp.root.join("secret")).unwrap();
        fs::write(tmp.root.join("secret/brightness"), "nope\n").unwrap();
        match set_brightness(&tmp.root, "backlight", "..", 1) {
            Err(SetError::Reject(Reject::NameHasDotDot { .. })) => {}
            other => panic!("{other:?}"),
        }
        let secret = fs::read_to_string(tmp.root.join("secret/brightness")).unwrap();
        assert_eq!(secret, "nope\n");
    }

    #[test]
    fn probe_finds_lcd_backlight() {
        let tmp = TempClass::new();
        tmp.add_device(
            "backlight",
            "lcd_backlight",
            &[("max_brightness", "255\n"), ("brightness", "200\n")],
        );
        let p = probe_backlight(&tmp.root).unwrap();
        assert_eq!(p.name, "lcd_backlight");
        assert_eq!(p.max_brightness, 255);
    }

    #[test]
    fn probe_skips_dotdot_and_files() {
        let tmp = TempClass::new();
        fs::create_dir_all(tmp.root.join("backlight")).unwrap();
        fs::write(tmp.root.join("backlight/not-a-dir"), "x").unwrap();
        // read_dir does not yield `..`, but a malicious name with `..`
        // in it must still be skipped if it ever appeared.
        tmp.add_device(
            "backlight",
            "foo..bar",
            &[("max_brightness", "10\n"), ("brightness", "1\n")],
        );
        tmp.add_device(
            "backlight",
            "lcd_backlight",
            &[("max_brightness", "255\n"), ("brightness", "1\n")],
        );
        let p = probe_backlight(&tmp.root).unwrap();
        assert_eq!(p.name, "lcd_backlight");
    }

    #[test]
    fn probe_requires_both_attributes() {
        let tmp = TempClass::new();
        tmp.add_device("backlight", "half", &[("max_brightness", "255\n")]);
        match probe_backlight(&tmp.root) {
            Err(Absent::NoUsableDevice { .. }) => {}
            other => panic!("{other:?}"),
        }
        tmp.add_device(
            "backlight",
            "lcd_backlight",
            &[("max_brightness", "255\n"), ("brightness", "1\n")],
        );
        assert!(probe_backlight(&tmp.root).is_ok());
    }

    #[test]
    fn probe_missing_class_is_absent() {
        let tmp = TempClass::new();
        match probe_backlight(&tmp.root) {
            Err(Absent::NoClass { .. }) => {}
            other => panic!("{other:?}"),
        }
        let missing = PathBuf::from("/no/such/logind-shim-backlight-dir");
        match probe_backlight(&missing) {
            Err(Absent::NoClass { path }) => assert_eq!(path, missing.join("backlight")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn probe_unparseable_max_is_not_usable() {
        let tmp = TempClass::new();
        tmp.add_device(
            "backlight",
            "broken",
            &[("max_brightness", "nope\n"), ("brightness", "1\n")],
        );
        match probe_backlight(&tmp.root) {
            Err(Absent::NoUsableDevice { .. }) => {}
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn set_brightness_max_unparseable() {
        let tmp = TempClass::new();
        tmp.add_device(
            "backlight",
            "lcd_backlight",
            &[("max_brightness", "xx\n"), ("brightness", "1\n")],
        );
        match set_brightness(&tmp.root, "backlight", "lcd_backlight", 10) {
            Err(SetError::MaxUnparseable { raw, .. }) => assert_eq!(raw, "xx"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn symlink_device_name_is_still_a_single_component() {
        // A class entry that is a symlink is a single name; we do not
        // try to detect it. The name itself has no `/` or `..`.
        let tmp = TempClass::new();
        let real = tmp.add_device(
            "devices",
            "real_bl",
            &[("max_brightness", "10\n"), ("brightness", "10\n")],
        );
        fs::create_dir_all(tmp.root.join("backlight")).unwrap();
        symlink(&real, tmp.root.join("backlight/lcd_backlight")).unwrap();
        let written = set_brightness(&tmp.root, "backlight", "lcd_backlight", 3).unwrap();
        assert_eq!(written, 3);
        assert_eq!(fs::read_to_string(real.join("brightness")).unwrap(), "3\n");
    }
}
