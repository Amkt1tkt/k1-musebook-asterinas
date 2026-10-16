// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: the three UPower objects alpha.7's battery applet talks to.

//! The three objects alpha.7's battery applet actually talks to.
//!
//! Extra properties support GetAll without implementing the complete UPower
//! service. PowerDaemon, PowerProfiles and KbdBacklight are absent; the battery
//! icon works independently of those performance and keyboard-light controls.

use std::sync::{Arc, Mutex};

use upower_shim::Snapshot;
use zbus::{interface, object_server::InterfaceRef, zvariant::OwnedObjectPath, Connection};

pub const SERVICE: &str = "org.freedesktop.UPower";
pub const UP_PATH: &str = "/org/freedesktop/UPower";
pub const DISPLAY_PATH: &str = "/org/freedesktop/UPower/devices/DisplayDevice";
pub const BAT0_PATH: &str = "/org/freedesktop/UPower/devices/battery_BAT0";

fn obj(path: &str) -> OwnedObjectPath {
    OwnedObjectPath::try_from(path).expect("constant is a valid object path")
}

pub struct UPower {
    pub state: Arc<Mutex<Snapshot>>,
}

#[interface(name = "org.freedesktop.UPower")]
impl UPower {
    /// Returns the composite display device.
    fn get_display_device(&self) -> OwnedObjectPath {
        obj(DISPLAY_PATH)
    }

    /// Enumerates physical batteries; the composite display device is excluded.
    fn enumerate_devices(&self) -> Vec<OwnedObjectPath> {
        vec![obj(BAT0_PATH)]
    }

    #[zbus(property)]
    fn on_battery(&self) -> bool {
        self.state.lock().expect("snapshot mutex").on_battery
    }

    // Supply common properties for GetAll clients; the applet does not read them.
    #[zbus(property)]
    fn daemon_version(&self) -> String {
        "0.1.0-k1-shim".into()
    }

    #[zbus(property)]
    fn lid_is_closed(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn lid_is_present(&self) -> bool {
        false
    }
}

pub struct DisplayDevice {
    pub state: Arc<Mutex<Snapshot>>,
}

#[interface(name = "org.freedesktop.UPower.Device")]
impl DisplayDevice {
    #[zbus(property)]
    fn percentage(&self) -> f64 {
        self.state.lock().expect("snapshot mutex").percentage
    }

    #[zbus(property)]
    fn time_to_empty(&self) -> i64 {
        self.state.lock().expect("snapshot mutex").time_to_empty
    }

    #[zbus(property, name = "Type")]
    fn device_type(&self) -> u32 {
        2
    }

    #[zbus(property)]
    fn power_supply(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn is_present(&self) -> bool {
        true
    }
}

/// Physical battery identity used to distinguish batteries from other supplies.
/// The current state is published on the composite display device.
pub struct BatteryBat0;

#[interface(name = "org.freedesktop.UPower.Device")]
impl BatteryBat0 {
    #[zbus(property, name = "Type")]
    fn device_type(&self) -> u32 {
        2
    }

    #[zbus(property)]
    fn power_supply(&self) -> bool {
        true
    }
}

/// Notifies subscribers that the published battery state changed.
pub async fn emit_applet_properties(conn: &Connection) -> zbus::Result<()> {
    let up: InterfaceRef<UPower> = conn.object_server().interface(UP_PATH).await?;
    {
        let iface = up.get().await;
        iface.on_battery_changed(up.signal_context()).await?;
    }

    let disp: InterfaceRef<DisplayDevice> = conn.object_server().interface(DISPLAY_PATH).await?;
    {
        let iface = disp.get().await;
        iface.percentage_changed(disp.signal_context()).await?;
        iface.time_to_empty_changed(disp.signal_context()).await?;
    }
    Ok(())
}
