// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: read /sys/class/power_supply/BAT0 (or a test double).

//! Read `/sys/class/power_supply/BAT0` (or a test double of that tree).
//!
//! Validates the battery before the service publishes it and derives the
//! properties needed by the COSMIC battery applet.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

/// Published D-Bus fields only. Polling compares this with `PartialEq` and
/// emits `PropertiesChanged` iff it differs — voltage-only jitter must not
/// wake the applet.
#[derive(Clone, Debug, PartialEq)]
pub struct Snapshot {
    /// UPower `Percentage` (`d`). Integer `capacity` as `f64`, clamped 0–100.
    pub percentage: f64,
    /// UPower `OnBattery` (`b`). Drives the charging- / not-charging icon.
    pub on_battery: bool,
    /// UPower `TimeToEmpty` (`x`), seconds. 0 if unknown. Fact: UPower
    /// Device docs, "Number of seconds until the power source is considered
    /// empty. Is set to 0 if unknown."
    pub time_to_empty: i64,
}

/// Why we will not claim `org.freedesktop.UPower`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Absent {
    NoDirectory {
        path: PathBuf,
    },
    NotADirectory {
        path: PathBuf,
    },
    /// `present` exists and is `0`. Missing `present` is *not* this — Linux
    /// batteries often omit the file.
    NotPresent {
        path: PathBuf,
    },
    CapacityMissing {
        path: PathBuf,
    },
    CapacityUnreadable {
        path: PathBuf,
        reason: String,
    },
    CapacityUnparseable {
        path: PathBuf,
        raw: String,
    },
}

impl std::fmt::Display for Absent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoDirectory { path } => {
                write!(f, "no power_supply directory at {}", path.display())
            }
            Self::NotADirectory { path } => {
                write!(f, "{} exists but is not a directory", path.display())
            }
            Self::NotPresent { path } => {
                write!(f, "{} reports present=0", path.display())
            }
            Self::CapacityMissing { path } => {
                write!(
                    f,
                    "{} has no capacity file (cannot publish Percentage)",
                    path.display()
                )
            }
            Self::CapacityUnreadable { path, reason } => {
                write!(f, "cannot read {}/capacity: {reason}", path.display())
            }
            Self::CapacityUnparseable { path, raw } => {
                write!(
                    f,
                    "{}/capacity is not a 0–100 integer (got {raw:?})",
                    path.display()
                )
            }
        }
    }
}

/// Kernel `POWER_SUPPLY_STATUS_*` text. Match is case-sensitive: that is
/// what `drivers/power/supply/power_supply_sysfs.c` writes. A lowercase
/// "discharging" is treated as [`Status::Unknown`], not silently accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Charging,
    Discharging,
    /// Kernel string is `"Not charging"` — space in the middle.
    NotCharging,
    Full,
    Unknown,
}

impl Status {
    pub fn parse(raw: &str) -> Self {
        match raw.trim() {
            "Charging" => Self::Charging,
            "Discharging" => Self::Discharging,
            "Not charging" => Self::NotCharging,
            "Full" => Self::Full,
            _ => Self::Unknown,
        }
    }

    /// Maps kernel status onto UPower `OnBattery`.
    ///
    /// Interpret battery status without a separate AC-presence signal.
    ///
    /// This port derives status from SBS current and does not expose AC presence.
    /// `Not charging` therefore means zero sampled current, which can also occur
    /// while unplugged. Treat it as on-battery to avoid a false charging icon.
    /// Revisit this mapping if a charger GPIO or PMIC AC-presence signal is exposed.
    /// `Unknown` uses the current sign, falling back to on-battery when unavailable.
    pub fn on_battery(self, current_now_ua: Option<i64>) -> bool {
        match self {
            Self::Charging | Self::Full => false,
            Self::Discharging | Self::NotCharging => true,
            Self::Unknown => match current_now_ua {
                Some(c) if c < 0 => true,
                Some(c) if c > 0 => false,
                _ => true,
            },
        }
    }
}

/// Successful read of a usable battery. `Err` means the binary must not
/// request `org.freedesktop.UPower`.
pub fn read_battery(dir: &Path) -> Result<Snapshot, Absent> {
    match fs::metadata(dir) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(Absent::NoDirectory {
                path: dir.to_path_buf(),
            });
        }
        Err(e) => {
            // An inaccessible directory cannot provide a valid battery snapshot.
            return Err(Absent::CapacityUnreadable {
                path: dir.to_path_buf(),
                reason: e.to_string(),
            });
        }
        Ok(meta) if !meta.is_dir() => {
            return Err(Absent::NotADirectory {
                path: dir.to_path_buf(),
            });
        }
        Ok(_) => {}
    }

    if !is_present(dir)? {
        return Err(Absent::NotPresent {
            path: dir.to_path_buf(),
        });
    }

    let capacity = read_capacity(dir)?;
    let status = read_optional_text(dir, "status").map_or(Status::Unknown, |s| Status::parse(&s));
    let current_now_ua = read_optional_i64(dir, "current_now");
    let charge_now_uah = read_optional_i64(dir, "charge_now");
    let energy_now_uwh = read_optional_i64(dir, "energy_now");
    let voltage_now_uv = read_optional_i64(dir, "voltage_now");

    Ok(Snapshot {
        percentage: f64::from(capacity),
        on_battery: status.on_battery(current_now_ua),
        time_to_empty: time_to_empty_secs(
            status,
            charge_now_uah,
            energy_now_uwh,
            voltage_now_uv,
            current_now_ua,
        ),
    })
}

fn is_present(dir: &Path) -> Result<bool, Absent> {
    match fs::read_to_string(dir.join("present")) {
        Ok(s) => Ok(s.trim() != "0"),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(e) => Err(Absent::CapacityUnreadable {
            path: dir.to_path_buf(),
            reason: format!("present: {e}"),
        }),
    }
}

fn read_capacity(dir: &Path) -> Result<u8, Absent> {
    let path = dir.join("capacity");
    let raw = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(Absent::CapacityMissing {
                path: dir.to_path_buf(),
            });
        }
        Err(e) => {
            return Err(Absent::CapacityUnreadable {
                path: dir.to_path_buf(),
                reason: e.to_string(),
            });
        }
    };
    parse_capacity(&raw).ok_or_else(|| Absent::CapacityUnparseable {
        path: dir.to_path_buf(),
        raw: raw.trim().to_string(),
    })
}

/// Integer 0–100 after trim. Values above 100 clamp — a lying gauge is
/// still a battery. Negative / non-integer is not.
fn parse_capacity(raw: &str) -> Option<u8> {
    let n: i32 = raw.trim().parse().ok()?;
    if n < 0 {
        return None;
    }
    Some(n.min(100) as u8)
}

fn read_optional_text(dir: &Path, name: &str) -> Option<String> {
    fs::read_to_string(dir.join(name)).ok()
}

fn read_optional_i64(dir: &Path, name: &str) -> Option<i64> {
    read_optional_text(dir, name)?.trim().parse().ok()
}

/// Seconds until empty. 0 when we cannot know.
///
/// Charge path: `charge_now` (µAh) / |`current_now`| (µA) × 3600.
/// Energy path (only if charge is absent): `energy_now` (µWh) / power(µW) × 3600
/// with power = `voltage_now`(µV) × |current|(µA) / 1_000_000.
///
/// This kernel reports signed current; other power-supply drivers use an
/// unsigned magnitude. Use the absolute value when status is `Discharging`.
pub fn time_to_empty_secs(
    status: Status,
    charge_now_uah: Option<i64>,
    energy_now_uwh: Option<i64>,
    voltage_now_uv: Option<i64>,
    current_now_ua: Option<i64>,
) -> i64 {
    if status != Status::Discharging {
        return 0;
    }
    let current = match current_now_ua {
        Some(c) if c != 0 => c.unsigned_abs() as i128,
        _ => return 0,
    };

    if let Some(charge) = charge_now_uah {
        if charge <= 0 {
            return 0;
        }
        return clamp_secs(charge as i128 * 3600 / current);
    }

    if let (Some(energy), Some(voltage)) = (energy_now_uwh, voltage_now_uv) {
        if energy <= 0 || voltage <= 0 {
            return 0;
        }
        let power_uw = voltage as i128 * current / 1_000_000;
        if power_uw == 0 {
            return 0;
        }
        return clamp_secs(energy as i128 * 3600 / power_uw);
    }

    0
}

fn clamp_secs(secs: i128) -> i64 {
    secs.clamp(0, i64::MAX as i128) as i64
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

    static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

    struct TempBat {
        dir: PathBuf,
    }

    impl TempBat {
        fn new(files: &[(&str, &str)]) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("upower-shim-{}-{seq}-{nanos}", std::process::id()));
            fs::create_dir_all(&dir).unwrap();
            for (name, body) in files {
                fs::write(dir.join(name), body).unwrap();
            }
            Self { dir }
        }
    }

    impl Drop for TempBat {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn capacity_trims_newline_and_spaces() {
        let bat = TempBat::new(&[("capacity", "  88\n")]);
        let snap = read_battery(&bat.dir).unwrap();
        assert_eq!(snap.percentage, 88.0);
    }

    #[test]
    fn capacity_crlf() {
        let bat = TempBat::new(&[("capacity", "7\r\n")]);
        assert_eq!(read_battery(&bat.dir).unwrap().percentage, 7.0);
    }

    #[test]
    fn capacity_above_100_still_a_battery_but_clamped() {
        let bat = TempBat::new(&[("capacity", "150\n")]);
        let snap = read_battery(&bat.dir).unwrap();
        assert_eq!(snap.percentage, 100.0);
    }

    #[test]
    fn not_charging_keeps_the_space_and_counts_as_on_battery() {
        // The space is the whole reason the parse half of this exists.
        assert_eq!(Status::parse("Not charging\n"), Status::NotCharging);
        assert_eq!(Status::parse("Notcharging"), Status::Unknown);
        // The board reaches this state from a zero-current sample while
        // unplugged, not from AC presence -- see `Status::on_battery`.
        assert!(Status::NotCharging.on_battery(None));
        assert!(Status::NotCharging.on_battery(Some(0)));
        assert!(Status::NotCharging.on_battery(Some(-1000)));
    }

    #[test]
    fn status_is_case_sensitive() {
        assert_eq!(Status::parse("discharging"), Status::Unknown);
        assert_eq!(Status::parse("Discharging"), Status::Discharging);
        assert_eq!(Status::parse("Charging"), Status::Charging);
        assert_eq!(Status::parse("Full"), Status::Full);
        assert_eq!(Status::parse("Unknown"), Status::Unknown);
    }

    #[test]
    fn on_battery_from_status() {
        assert!(Status::Discharging.on_battery(None));
        assert!(Status::NotCharging.on_battery(None));
        assert!(!Status::Charging.on_battery(None));
        assert!(!Status::Full.on_battery(None));
        assert!(Status::Unknown.on_battery(None));
        assert!(Status::Unknown.on_battery(Some(-130_000)));
        assert!(!Status::Unknown.on_battery(Some(130_000)));
        assert!(Status::Unknown.on_battery(Some(0)));
    }

    #[test]
    fn discharging_with_negative_current_sets_on_battery() {
        let bat = TempBat::new(&[
            ("capacity", "88\n"),
            ("status", "Discharging\n"),
            ("current_now", "-130000\n"),
            ("charge_now", "4309000\n"),
        ]);
        let snap = read_battery(&bat.dir).unwrap();
        assert!(snap.on_battery);
        // 4309000 µAh / 130000 µA * 3600 s = 119326 s
        assert_eq!(snap.time_to_empty, 4_309_000 * 3600 / 130_000);
    }

    #[test]
    fn discharging_with_unsigned_current_still_computes_tte() {
        // Linux often reports current_now as a magnitude.
        let secs = time_to_empty_secs(
            Status::Discharging,
            Some(4_309_000),
            None,
            None,
            Some(130_000),
        );
        assert_eq!(secs, 4_309_000 * 3600 / 130_000);
    }

    #[test]
    fn time_to_empty_zero_when_not_discharging() {
        let secs = time_to_empty_secs(
            Status::NotCharging,
            Some(4_309_000),
            None,
            None,
            Some(-130_000),
        );
        assert_eq!(secs, 0);
        assert_eq!(
            time_to_empty_secs(Status::Charging, Some(1_000), None, None, Some(-1_000)),
            0
        );
        assert_eq!(
            time_to_empty_secs(Status::Full, Some(1_000), None, None, Some(-1_000)),
            0
        );
    }

    #[test]
    fn time_to_empty_zero_without_current_or_charge() {
        assert_eq!(
            time_to_empty_secs(Status::Discharging, Some(1000), None, None, None),
            0
        );
        assert_eq!(
            time_to_empty_secs(Status::Discharging, Some(1000), None, None, Some(0)),
            0
        );
        assert_eq!(
            time_to_empty_secs(Status::Discharging, None, None, None, Some(-1000)),
            0
        );
    }

    #[test]
    fn time_to_empty_prefers_charge_over_energy() {
        // Energy numbers deliberately disagree with the charge path so a
        // "whichever we happen to read" bug would fail this.
        let from_charge = time_to_empty_secs(
            Status::Discharging,
            Some(2_000_000),
            Some(99_000_000),
            Some(8_000_000),
            Some(-100_000),
        );
        assert_eq!(from_charge, 2_000_000 * 3600 / 100_000);
        let from_energy = time_to_empty_secs(
            Status::Discharging,
            None,
            Some(35_720_000),
            Some(8_290_000),
            Some(-130_000),
        );
        // power_µW = 8_290_000 * 130_000 / 1_000_000 = 1_077_700
        assert_eq!(from_energy, 35_720_000 * 3600 / 1_077_700);
        assert_ne!(from_charge, from_energy);
    }

    #[test]
    fn charging_snapshot_is_not_on_battery_and_tte_zero() {
        let bat = TempBat::new(&[
            ("capacity", "40\n"),
            ("status", "Charging\n"),
            ("current_now", "200000\n"),
            ("charge_now", "2000000\n"),
        ]);
        let snap = read_battery(&bat.dir).unwrap();
        assert!(!snap.on_battery);
        assert_eq!(snap.time_to_empty, 0);
        assert_eq!(snap.percentage, 40.0);
    }

    #[test]
    fn absent_when_directory_missing() {
        let path = PathBuf::from("/no/such/upower-shim-battery-dir");
        match read_battery(&path) {
            Err(Absent::NoDirectory { path: p }) => assert_eq!(p, path),
            other => panic!("expected NoDirectory, got {other:?}"),
        }
    }

    #[test]
    fn absent_when_path_is_a_file() {
        let bat = TempBat::new(&[]);
        let file = bat.dir.join("not-a-dir");
        fs::write(&file, "x").unwrap();
        match read_battery(&file) {
            Err(Absent::NotADirectory { path }) => assert_eq!(path, file),
            other => panic!("expected NotADirectory, got {other:?}"),
        }
    }

    #[test]
    fn absent_when_capacity_missing() {
        let bat = TempBat::new(&[("status", "Discharging\n")]);
        match read_battery(&bat.dir) {
            Err(Absent::CapacityMissing { .. }) => {}
            other => panic!("expected CapacityMissing, got {other:?}"),
        }
    }

    #[test]
    fn absent_when_capacity_empty_or_garbage() {
        let empty = TempBat::new(&[("capacity", "\n")]);
        match read_battery(&empty.dir) {
            Err(Absent::CapacityUnparseable { raw, .. }) => assert_eq!(raw, ""),
            other => panic!("expected CapacityUnparseable, got {other:?}"),
        }
        let garbage = TempBat::new(&[("capacity", "88%\n")]);
        match read_battery(&garbage.dir) {
            Err(Absent::CapacityUnparseable { raw, .. }) => assert_eq!(raw, "88%"),
            other => panic!("expected CapacityUnparseable, got {other:?}"),
        }
        let neg = TempBat::new(&[("capacity", "-1\n")]);
        match read_battery(&neg.dir) {
            Err(Absent::CapacityUnparseable { raw, .. }) => assert_eq!(raw, "-1"),
            other => panic!("expected CapacityUnparseable, got {other:?}"),
        }
    }

    #[test]
    fn absent_when_present_is_zero() {
        let bat = TempBat::new(&[("capacity", "88\n"), ("present", "0\n")]);
        match read_battery(&bat.dir) {
            Err(Absent::NotPresent { .. }) => {}
            other => panic!("expected NotPresent, got {other:?}"),
        }
    }

    #[test]
    fn present_missing_is_still_a_battery() {
        let bat = TempBat::new(&[("capacity", "1\n")]);
        assert_eq!(read_battery(&bat.dir).unwrap().percentage, 1.0);
    }

    #[test]
    fn present_one_is_a_battery() {
        let bat = TempBat::new(&[("capacity", "12\n"), ("present", "1\n")]);
        assert_eq!(read_battery(&bat.dir).unwrap().percentage, 12.0);
    }

    #[test]
    fn snapshot_equality_ignores_nothing_we_do_not_publish() {
        // Same published triple → no PropertiesChanged. This is the
        // comparison the poll loop uses; if someone later adds voltage
        // to Snapshot, this test should be rewritten, not left green.
        let a = Snapshot {
            percentage: 88.0,
            on_battery: true,
            time_to_empty: 100,
        };
        let b = Snapshot {
            percentage: 88.0,
            on_battery: true,
            time_to_empty: 100,
        };
        let c = Snapshot {
            percentage: 87.0,
            on_battery: true,
            time_to_empty: 100,
        };
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_ne!(
            a,
            Snapshot {
                time_to_empty: 99,
                ..a.clone()
            }
        );
        assert_ne!(
            a,
            Snapshot {
                on_battery: false,
                ..a
            }
        );
    }

    #[test]
    fn err_means_do_not_claim_the_name() {
        // The binary's claim gate is `read_battery(...).is_ok()`. This is
        // the mapping the task asked to test; it is not `assert!(true)`.
        let missing = PathBuf::from("/no/such/upower-shim-battery-dir");
        assert!(
            read_battery(&missing).is_err(),
            "missing dir must refuse the name"
        );
        let ok = TempBat::new(&[("capacity", "0\n")]);
        let snap = read_battery(&ok.dir).expect("0% is a real empty pack, not Absent");
        assert_eq!(snap.percentage, 0.0);
    }
}
