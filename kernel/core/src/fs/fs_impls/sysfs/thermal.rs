// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: `/sys/class/thermal/thermal_zone0` on the device model.

//! `/sys/class/thermal/thermal_zone0`.
//!
//! Linux `thermal_sysfs.c` `temp` is millidegrees. The number comes from
//! `k1-musebook-thermal` (K1 TSEN bjt3 / cluster0). The zone is a parentless
//! class device, so it lands under `/sys/devices/virtual/thermal`.

use core::sync::atomic::{AtomicBool, Ordering};

use aster_systree::SysStr;

use super::{LiveAttr, VirtualDeviceInfo, register_virtual_device};
use crate::prelude::*;

const CLASS: &str = "thermal";
const NAME: &str = "thermal_zone0";

static PUBLISHED: AtomicBool = AtomicBool::new(false);

pub(crate) fn publish() -> bool {
    if PUBLISHED.swap(true, Ordering::AcqRel) {
        return true;
    }
    match register() {
        Ok(()) => {
            info!("[k1-therm] sysfs /sys/class/thermal/thermal_zone0");
            true
        }
        Err(err) => {
            PUBLISHED.store(false, Ordering::Release);
            warn!("failed to publish thermal sysfs: {:?}", err);
            false
        }
    }
}

fn register() -> Result<()> {
    let live_attrs: Vec<LiveAttr> = vec![
        (SysStr::from("type"), attr_type),
        (SysStr::from("temp"), attr_temp),
        (SysStr::from("uevent"), attr_uevent),
    ];
    register_virtual_device(VirtualDeviceInfo {
        class: SysStr::from(CLASS),
        name: SysStr::from(NAME),
        uevent: String::new(),
        attrs: Vec::new(),
        attr_groups: Vec::new(),
        live_attrs,
        live_rw_attrs: Vec::new(),
    })?;
    Ok(())
}

fn attr_type() -> Option<String> {
    Some(String::from("k1x-cluster0\n"))
}

fn attr_temp() -> Option<String> {
    let mc = k1_musebook_thermal::cluster0_temp_mc()
        .or_else(k1_musebook_thermal::cluster_hot_temp_mc)?;
    Some(alloc::format!("{mc}\n"))
}

fn attr_uevent() -> Option<String> {
    Some(String::from("DEVTYPE=thermal_zone\n"))
}
