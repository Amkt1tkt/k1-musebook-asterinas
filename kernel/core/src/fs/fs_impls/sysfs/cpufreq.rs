// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: Linux-shaped `/sys/devices/system/cpu/cpufreq/policy0` on the device model.

//! `/sys/devices/system/cpu/cpufreq/policy0` plus the Linux per-CPU symlink
//! `/sys/devices/system/cpu/cpu0/cpufreq -> ../cpufreq/policy0`.
//!
//! Attribute names follow `Documentation/ABI/testing/sysfs-devices-system-cpu`.
//! Formatting lives in `k1_musebook_cpufreq::{attr_snap, nodes}`.
//!
//! A policy is not a virtual device: Linux parents it on `cpu_subsys`, so
//! the files live under [`system_devices_dir`] rather than
//! `/sys/devices/virtual/cpufreq`.

use alloc::format;
use core::sync::atomic::{AtomicBool, Ordering};

use aster_systree::{Error as SysTreeError, Result as SysTreeResult, SymlinkNode, SysStr};

use super::{
    LiveAttr, LiveRwAttr,
    device::{AttrDirInfo, class_index, register_attr_dir},
    system_devices_dir,
};
use crate::prelude::*;

const CLASS: &str = "cpufreq";
const NAME: &str = "policy0";

static PUBLISHED: AtomicBool = AtomicBool::new(false);

pub(crate) fn publish() -> bool {
    if PUBLISHED.swap(true, Ordering::AcqRel) {
        return true;
    }
    match register() {
        Ok(()) => {
            info!(
                "[k1-cpufreq] sysfs /sys/devices/system/cpu/cpu0/cpufreq and /sys/class/cpufreq/policy0"
            );
            true
        }
        Err(err) => {
            PUBLISHED.store(false, Ordering::Release);
            warn!("[k1-cpufreq] failed to publish sysfs: {:?}", err);
            false
        }
    }
}

fn register() -> Result<()> {
    let live_attrs: Vec<LiveAttr> = vec![
        (SysStr::from("scaling_cur_freq"), attr_cur),
        (
            SysStr::from("scaling_available_frequencies"),
            attr_available,
        ),
        (
            SysStr::from("scaling_available_governors"),
            attr_available_govs,
        ),
        (SysStr::from("cpuinfo_min_freq"), attr_cpuinfo_min),
        (SysStr::from("cpuinfo_max_freq"), attr_cpuinfo_max),
        (SysStr::from("cpuinfo_cur_freq"), attr_cpuinfo_cur),
        (SysStr::from("affected_cpus"), attr_affected_cpus),
        (SysStr::from("related_cpus"), attr_related_cpus),
        (SysStr::from("scaling_driver"), attr_driver),
        (SysStr::from("uevent"), attr_uevent),
    ];
    let live_rw_attrs: Vec<LiveRwAttr> = vec![
        (SysStr::from("scaling_min_freq"), attr_min, write_min),
        (SysStr::from("scaling_max_freq"), attr_max, write_max),
        (SysStr::from("scaling_governor"), attr_gov, write_gov),
        (
            SysStr::from("scaling_setspeed"),
            attr_setspeed,
            write_setspeed,
        ),
    ];

    let system = system_devices_dir()?;
    let cpu = system.get_or_create_dir(SysStr::from("cpu"))?;
    let cpufreq_dir = cpu.get_or_create_dir(SysStr::from("cpufreq"))?;

    register_attr_dir(
        &cpufreq_dir,
        AttrDirInfo {
            name: SysStr::from(NAME),
            uevent: String::new(),
            attrs: Vec::new(),
            live_attrs,
            live_rw_attrs,
        },
    )?;

    let class = class_index(SysStr::from(CLASS))?;
    let class_link = SymlinkNode::new(
        SysStr::from(NAME),
        String::from("../../devices/system/cpu/cpufreq/policy0"),
    );
    if let Err(err) = class.add_child(class_link) {
        let _ = cpufreq_dir.remove_child(NAME);
        return Err(err.into());
    }

    let cpu0 = cpu.get_or_create_dir(SysStr::from("cpu0"))?;
    let cpu_link = SymlinkNode::new(SysStr::from("cpufreq"), String::from("../cpufreq/policy0"));
    if let Err(err) = cpu0.add_child(cpu_link) {
        warn!("[k1-cpufreq] cpu0/cpufreq symlink: {:?}", err);
    }
    Ok(())
}

fn attr_cur() -> Option<String> {
    k1_musebook_cpufreq::attr_snap().attr("scaling_cur_freq")
}

fn attr_available() -> Option<String> {
    k1_musebook_cpufreq::attr_snap().attr("scaling_available_frequencies")
}

fn attr_available_govs() -> Option<String> {
    k1_musebook_cpufreq::attr_snap().attr("scaling_available_governors")
}

fn attr_min() -> Option<String> {
    k1_musebook_cpufreq::attr_snap().attr("scaling_min_freq")
}

fn attr_max() -> Option<String> {
    k1_musebook_cpufreq::attr_snap().attr("scaling_max_freq")
}

fn attr_gov() -> Option<String> {
    k1_musebook_cpufreq::attr_snap().attr("scaling_governor")
}

fn attr_setspeed() -> Option<String> {
    k1_musebook_cpufreq::attr_snap().attr("scaling_setspeed")
}

fn attr_cpuinfo_min() -> Option<String> {
    Some(k1_musebook_cpufreq::nodes::format_khz(
        k1_musebook_cpufreq::allowed_min_khz(),
    ))
}

fn attr_cpuinfo_max() -> Option<String> {
    Some(k1_musebook_cpufreq::nodes::format_khz(
        k1_musebook_cpufreq::allowed_max_khz(),
    ))
}

fn attr_cpuinfo_cur() -> Option<String> {
    Some(k1_musebook_cpufreq::nodes::format_khz(
        k1_musebook_cpufreq::attr_snap().policy.cur_khz,
    ))
}

fn attr_affected_cpus() -> Option<String> {
    Some(String::from("0\n"))
}

fn attr_related_cpus() -> Option<String> {
    Some(String::from("0\n"))
}

fn attr_driver() -> Option<String> {
    Some(String::from("k1-cpufreq\n"))
}

fn attr_uevent() -> Option<String> {
    let p = k1_musebook_cpufreq::attr_snap().policy;
    Some(format!(
        "CPUFREQ_GOVERNOR={}\nCPUFREQ_CUR_FREQ={}\n",
        p.gov.as_str(),
        p.cur_khz
    ))
}

fn write_min(reader: &mut VmReader) -> SysTreeResult<usize> {
    let (khz, n) = parse_u32(reader)?;
    k1_musebook_cpufreq::set_min_khz(khz).map_err(|_| SysTreeError::InvalidOperation)?;
    Ok(n)
}

fn write_max(reader: &mut VmReader) -> SysTreeResult<usize> {
    let (khz, n) = parse_u32(reader)?;
    k1_musebook_cpufreq::set_max_khz(khz).map_err(|_| SysTreeError::InvalidOperation)?;
    Ok(n)
}

fn write_gov(reader: &mut VmReader) -> SysTreeResult<usize> {
    let (s, n) = parse_word(reader)?;
    k1_musebook_cpufreq::set_governor(&s).map_err(|_| SysTreeError::InvalidOperation)?;
    Ok(n)
}

fn write_setspeed(reader: &mut VmReader) -> SysTreeResult<usize> {
    let (khz, n) = parse_u32(reader)?;
    k1_musebook_cpufreq::set_setspeed(khz).map_err(|_| SysTreeError::InvalidOperation)?;
    Ok(n)
}

fn parse_u32(reader: &mut VmReader) -> SysTreeResult<(u32, usize)> {
    let (s, n) = parse_word(reader)?;
    let value = s
        .parse::<u32>()
        .map_err(|_| SysTreeError::InvalidOperation)?;
    Ok((value, n))
}

fn parse_word(reader: &mut VmReader) -> SysTreeResult<(String, usize)> {
    const MAX: usize = 32;
    let (cstr, n) = reader
        .read_cstring_until_end(MAX)
        .map_err(|_| SysTreeError::PageFault)?;
    let s = cstr
        .to_str()
        .ok()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or(SysTreeError::InvalidOperation)?
        .to_string();
    Ok((s, n))
}
