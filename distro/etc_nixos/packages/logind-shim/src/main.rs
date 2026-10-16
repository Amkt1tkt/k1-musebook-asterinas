// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: minimal login1 brightness and orderly power-control methods.

//! Minimal `org.freedesktop.login1` for brightness and root-only power control.
//!
//! Startup requires a backlight with a brightness file and readable
//! `max_brightness`. A failed probe leaves both brightness and power unavailable.
//! This shim does not implement session or device management; the compositor
//! uses `LIBSEAT_BACKEND=builtin`.
//!
//! `Builder::system()` honours `DBUS_SYSTEM_BUS_ADDRESS`. With no system bus,
//! `k1-cosmic-session` aliases it to the session socket.

mod dbus;
mod power;

use std::{env, path::PathBuf, process};

use logind_shim::probe_backlight;

use crate::dbus::{Manager, Session, MANAGER, SERVICE, SESSION_AUTO};

struct Args {
    sysfs_root: PathBuf,
}

fn usage() -> &'static str {
    "logind-shim [--sysfs-root PATH]\n\
     PATH is the sysfs *class* directory (default /sys/class).\n\
     Claims org.freedesktop.login1 only after the backlight probe succeeds.\n\
     Serves Session.SetBrightness on /org/freedesktop/login1/session/auto.\n\
     Serves root-only Manager.PowerOff/Reboot on /org/freedesktop/login1."
}

fn parse_args(argv: &[String]) -> Result<Args, String> {
    let mut sysfs_root = PathBuf::from("/sys/class");
    let mut i = 1;
    while i < argv.len() {
        match argv[i].as_str() {
            "-h" | "--help" => return Err(usage().into()),
            "--sysfs-root" => {
                i += 1;
                let p = argv.get(i).ok_or("--sysfs-root needs a path")?;
                sysfs_root = PathBuf::from(p);
            }
            other => return Err(format!("unknown argument: {other}\n{}", usage())),
        }
        i += 1;
    }
    Ok(Args { sysfs_root })
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
            eprintln!("logind-shim: {e}");
            process::exit(2);
        }
    };

    // A successful backlight probe is required before requesting the bus name.
    let first = match probe_backlight(&args.sysfs_root) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("logind-shim: backlight probe failed: {e}; not claiming {SERVICE}");
            process::exit(1);
        }
    };
    eprintln!(
        "logind-shim: backlight {}/backlight/{} max_brightness={} — requesting {}",
        args.sysfs_root.display(),
        first.name,
        first.max_brightness,
        SERVICE
    );

    // Register both interfaces before requesting the service name.
    let _conn = match zbus::connection::Builder::system()
        .and_then(|b| b.name(SERVICE))
        .and_then(|b| b.serve_at(MANAGER, Manager))
        .and_then(|b| {
            b.serve_at(
                SESSION_AUTO,
                Session {
                    sysfs_root: args.sysfs_root.clone(),
                },
            )
        }) {
        Ok(builder) => match builder.build().await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("logind-shim: system-bus connection or name request failed: {e}");
                process::exit(1);
            }
        },
        Err(e) => {
            eprintln!("logind-shim: cannot build system-bus connection: {e}");
            process::exit(1);
        }
    };

    eprintln!(
        "logind-shim: owning {SERVICE} at {SESSION_AUTO}; serving SetBrightness and root-only PowerOff/Reboot"
    );
    std::future::pending::<()>().await;
}
