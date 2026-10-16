// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: minimal org.freedesktop.UPower for cosmic-applet-battery.

//! Minimal `org.freedesktop.UPower` for cosmic-applet-battery alpha.7.
//!
//! Battery data and the bus may become available after the desktop starts.
//! Retry initialization without publishing an absent battery. Connect to the
//! system bus or the launcher's explicit `DBUS_SYSTEM_BUS_ADDRESS` fallback.

mod dbus;

use std::{
    env,
    path::PathBuf,
    process,
    sync::{Arc, Mutex},
    time::Duration,
};

use upower_shim::{read_battery, Snapshot};

use crate::dbus::{
    emit_applet_properties, BatteryBat0, DisplayDevice, UPower, BAT0_PATH, DISPLAY_PATH, SERVICE,
    UP_PATH,
};

struct Args {
    sysfs_root: PathBuf,
    interval: Duration,
}

fn usage() -> &'static str {
    "upower-shim [--sysfs-root PATH] [--interval-secs 1..5]\n\
     Reads PATH (default /sys/class/power_supply/BAT0). Claims\n\
     org.freedesktop.UPower only after a successful read. Interval default 2."
}

fn parse_args(argv: &[String]) -> Result<Args, String> {
    let mut sysfs_root = PathBuf::from("/sys/class/power_supply/BAT0");
    let mut interval = Duration::from_secs(2);
    let mut i = 1;
    while i < argv.len() {
        match argv[i].as_str() {
            "-h" | "--help" => return Err(usage().into()),
            "--sysfs-root" => {
                i += 1;
                let p = argv.get(i).ok_or("--sysfs-root needs a path")?;
                sysfs_root = PathBuf::from(p);
            }
            "--interval-secs" => {
                i += 1;
                let raw = argv.get(i).ok_or("--interval-secs needs 1..5")?;
                let n: u64 = raw
                    .parse()
                    .map_err(|_| format!("--interval-secs: not an integer: {raw}"))?;
                if !(1..=5).contains(&n) {
                    return Err(format!("--interval-secs must be 1..5, got {n}"));
                }
                interval = Duration::from_secs(n);
            }
            other => return Err(format!("unknown argument: {other}\n{}", usage())),
        }
        i += 1;
    }
    Ok(Args {
        sysfs_root,
        interval,
    })
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let argv: Vec<String> = env::args().collect();
    if argv.iter().any(|a| a == "-h" || a == "--help") {
        eprintln!("{}", usage());
        process::exit(0);
    }
    let args = match parse_args(&argv) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("upower-shim: {e}");
            process::exit(2);
        }
    };

    let mut battery_unavailable = false;
    let mut bus_unavailable = false;
    let (conn, state) = loop {
        // Re-read before every connection attempt so a late bus gets current data.
        let snapshot = match read_battery(&args.sysfs_root) {
            Ok(snapshot) => snapshot,
            Err(e) => {
                if !battery_unavailable {
                    eprintln!("upower-shim: waiting for battery data: {e}");
                    battery_unavailable = true;
                }
                tokio::time::sleep(args.interval).await;
                continue;
            }
        };
        let state = Arc::new(Mutex::new(snapshot));
        match connect(state.clone()).await {
            Ok(conn) => break (conn, state),
            Err(e) => {
                if !bus_unavailable {
                    eprintln!("upower-shim: waiting for UPower bus connection: {e}");
                    bus_unavailable = true;
                }
            }
        }
        tokio::time::sleep(args.interval).await;
    };
    {
        let first = state.lock().expect("snapshot mutex");
        eprintln!(
            "upower-shim: {} ready at {} percentage={} on_battery={} time_to_empty={}s",
            SERVICE,
            args.sysfs_root.display(),
            first.percentage,
            first.on_battery,
            first.time_to_empty,
        );
    }

    if let Err(e) = emit_applet_properties(&conn).await {
        eprintln!("upower-shim: initial PropertiesChanged failed: {e}");
    }

    let mut ticker = tokio::time::interval(args.interval);
    ticker.tick().await; // the first tick is immediate; we already emitted
    let mut read_failed = false;
    loop {
        ticker.tick().await;
        match read_battery(&args.sysfs_root) {
            Ok(next) => {
                if read_failed {
                    eprintln!("upower-shim: battery data recovered");
                    read_failed = false;
                }
                let changed = {
                    let mut guard = state.lock().expect("snapshot mutex");
                    if *guard == next {
                        false
                    } else {
                        *guard = next.clone();
                        true
                    }
                };
                if changed {
                    if let Err(e) = emit_applet_properties(&conn).await {
                        eprintln!("upower-shim: PropertiesChanged failed: {e}");
                    }
                }
            }
            Err(e) => {
                // A temporary read failure is not evidence that the battery is gone.
                if !read_failed {
                    eprintln!("upower-shim: battery read failed ({e}); retaining last state");
                    read_failed = true;
                }
            }
        }
    }
}

async fn connect(state: Arc<Mutex<Snapshot>>) -> zbus::Result<zbus::Connection> {
    // zbus registers these objects before claiming the well-known name.
    zbus::connection::Builder::system()?
        .name(SERVICE)?
        .serve_at(
            UP_PATH,
            UPower {
                state: state.clone(),
            },
        )?
        .serve_at(DISPLAY_PATH, DisplayDevice { state })?
        .serve_at(BAT0_PATH, BatteryBat0)?
        .build()
        .await
}
