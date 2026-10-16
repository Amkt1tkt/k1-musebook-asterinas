// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: login1 brightness and root-only power interfaces.

//! Brightness plus the small login1 Manager power-control surface.
//!
//! Power operations require uid 0 verified through the bus daemon and ask
//! PID 1 for an orderly shutdown. This remains a single-root-user board shim,
//! not a seat manager: GetSession/TakeDevice are deliberately not implemented.
//! The compositor must keep LIBSEAT_BACKEND=builtin.

use std::path::PathBuf;

use logind_shim::set_brightness;
use zbus::{fdo, interface};

pub const SERVICE: &str = "org.freedesktop.login1";
pub const SESSION_AUTO: &str = "/org/freedesktop/login1/session/auto";
pub const MANAGER: &str = "/org/freedesktop/login1";

pub struct Manager;

#[interface(name = "org.freedesktop.login1.Manager")]
impl Manager {
    async fn power_off(
        &self,
        _interactive: bool,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> fdo::Result<()> {
        require_root(conn, &header).await?;
        eprintln!("logind-shim: root requested orderly poweroff.target");
        crate::power::queue(crate::power::Action::PowerOff)
            .await
            .map_err(fdo::Error::Failed)
    }

    async fn reboot(
        &self,
        _interactive: bool,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> fdo::Result<()> {
        require_root(conn, &header).await?;
        eprintln!("logind-shim: root requested orderly reboot.target");
        crate::power::queue(crate::power::Action::Reboot)
            .await
            .map_err(fdo::Error::Failed)
    }

    async fn can_power_off(
        &self,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> &'static str {
        if require_root(conn, &header).await.is_ok() && crate::power::available() {
            "yes"
        } else {
            "no"
        }
    }

    async fn can_reboot(
        &self,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> &'static str {
        self.can_power_off(conn, header).await
    }

    fn can_suspend(&self) -> &'static str {
        "no"
    }

    fn can_hibernate(&self) -> &'static str {
        "no"
    }
}

async fn require_root(
    conn: &zbus::Connection,
    header: &zbus::message::Header<'_>,
) -> fdo::Result<()> {
    let sender = header
        .sender()
        .ok_or_else(|| fdo::Error::AccessDenied("missing bus sender".into()))?;
    let proxy = fdo::DBusProxy::new(conn).await?;
    let uid = proxy
        .get_connection_unix_user(sender.clone().into())
        .await?;
    crate::power::authorize_uid(uid).map_err(fdo::Error::AccessDenied)
}

pub struct Session {
    pub sysfs_root: PathBuf,
}

#[interface(name = "org.freedesktop.login1.Session")]
impl Session {
    /// Sets brightness through the sysfs write path.
    ///
    /// COSMIC calls this method on the fixed `session/auto` endpoint.
    fn set_brightness(&self, subsystem: String, name: String, brightness: u32) -> fdo::Result<()> {
        match set_brightness(&self.sysfs_root, &subsystem, &name, brightness) {
            Ok(_) => Ok(()),
            Err(e) => {
                eprintln!(
                    "logind-shim: SetBrightness({subsystem}, {name}, {brightness}) failed: {e}"
                );
                Err(to_fdo(e))
            }
        }
    }
}

fn to_fdo(e: logind_shim::SetError) -> fdo::Error {
    match e {
        logind_shim::SetError::Reject(r) => fdo::Error::InvalidArgs(r.to_string()),
        logind_shim::SetError::DeviceMissing { path } => {
            fdo::Error::FileNotFound(format!("no device at {}", path.display()))
        }
        other => fdo::Error::Failed(other.to_string()),
    }
}
