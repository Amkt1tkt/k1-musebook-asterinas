// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: `/dev/rtc0` (and `/dev/rtc`) for hwclock RTC_RD_TIME / RTC_SET_TIME.

//! `/dev/rtc0` (and a `/dev/rtc` alias) — the Linux RTC character device.
//!
//! Hardware access is the platform RTC driver in `aster_time`. This file
//! is only the userspace node and the ioctl ABI `hwclock` needs.
//!
//! # RTC and system clock semantics
//!
//! `RTC_SET_TIME` writes the **hardware** RTC and does **not** move the
//! in-kernel wall clock. That is `rtc_dev_ioctl` → `rtc_set_time` in
//! `drivers/rtc/dev.c` / `class.c`. `hwclock --systohc` relies on it:
//! it reads `CLOCK_REALTIME` itself and then programs the chip. Moving
//! `WALL_BASE` here would make `--systohc` a disguised `date -s`.
//!
//! The opposite direction is `hwclock --hctosys`: `RTC_RD_TIME` then
//! userspace `clock_settime`, which **does** go through
//! [`crate::time::set_realtime`] (wall clock only).
//!
//! `RTC_SET_TIME` requires `CAP_SYS_TIME`, same as Linux
//! (`capable(CAP_SYS_TIME)` in `rtc_dev_ioctl`).
//!
//! `RTC_UIE_ON` / `RTC_UIE_OFF` return `ENOTTY`. util-linux `hwclock`
//! then falls back to polling `RTC_RD_TIME` (`hwclock-rtc.c`, the
//! `EINVAL || ENOTTY` branch). Returning success without a real update
//! IRQ would make `select()` on this fd hang.
//!
//! Device numbers: major 253, minor 0.

mod ioctl_defs;
mod rtc_time;

use aster_time::RtcWriteError;
use device_id::{DeviceId, MajorId, MinorId};
use spin::Once;

use self::rtc_time::{rtc_time_from_system_time, system_time_from_rtc_time};
use super::{
    Device, DeviceType,
    registry::char::{MajorIdOwner, acquire_major, register},
};
use crate::{
    dispatch_ioctl,
    events::IoEvents,
    fs::{
        devtmpfs::{DevtmpfsNode, DevtmpfsNodeMeta, create_node},
        file::{PerOpenFileOps, StatusFlags},
        vfs::{inode::FileOps, path::Path},
    },
    prelude::*,
    process::signal::{PollHandle, Pollable},
    util::ioctl::RawIoctl,
};

/// Historical RTC-class major (`Documentation/admin-guide/devices.txt`).
const RTC_MAJOR_ID: u16 = 253;

static RTC_MAJOR: Once<MajorIdOwner> = Once::new();
static RTC_ID: Once<DeviceId> = Once::new();

#[derive(Debug)]
struct RtcDevice {
    id: DeviceId,
}

impl RtcDevice {
    fn new() -> Arc<Self> {
        let major = RTC_MAJOR.get().unwrap().get();
        Arc::new(Self {
            id: DeviceId::new(major, MinorId::new(0)),
        })
    }
}

impl Device for RtcDevice {
    fn type_(&self) -> DeviceType {
        DeviceType::Char
    }

    fn id(&self) -> DeviceId {
        self.id
    }

    fn devtmpfs_meta(&self) -> Option<DevtmpfsNodeMeta> {
        Some(DevtmpfsNodeMeta::new("rtc0").unwrap())
    }

    fn open(&self) -> Result<Box<dyn PerOpenFileOps>> {
        Ok(Box::new(RtcFile))
    }
}

struct RtcFile;

impl Pollable for RtcFile {
    fn poll(&self, mask: IoEvents, _poller: Option<&mut PollHandle>) -> IoEvents {
        // No update IRQ, so there is never a pending interrupt to wait
        // for. Advertise the VFS default (always ready) the way
        // `/dev/fb0` does; `hwclock` does not poll this fd after we
        // refuse `RTC_UIE_ON`.
        mask & (IoEvents::IN | IoEvents::OUT)
    }
}

impl FileOps for RtcFile {
    fn read_at(
        &self,
        _offset: usize,
        _writer: &mut VmWriter,
        _status_flags: StatusFlags,
    ) -> Result<usize> {
        // Linux `rtc_dev_read` waits for an IRQ and returns `unsigned long`.
        // We have no IRQ; do not pretend to.
        return_errno_with_message!(Errno::EINVAL, "rtc read needs an update IRQ");
    }

    fn write_at(
        &self,
        _offset: usize,
        _reader: &mut VmReader,
        _status_flags: StatusFlags,
    ) -> Result<usize> {
        return_errno_with_message!(Errno::EINVAL, "rtc is ioctl-only");
    }
}

impl PerOpenFileOps for RtcFile {
    fn check_seekable(&self) -> Result<()> {
        return_errno_with_message!(Errno::ESPIPE, "rtc is not seekable");
    }

    fn is_offset_aware(&self) -> bool {
        false
    }

    fn ioctl(&self, _path: &Path, raw_ioctl: RawIoctl) -> Result<i32> {
        use ioctl_defs::*;

        dispatch_ioctl!(match raw_ioctl {
            cmd @ RdTime => {
                let Some(now) = aster_time::read_rtc() else {
                    return_errno_with_message!(Errno::ENODEV, "no RTC driver");
                };
                let tm = rtc_time_from_system_time(now)?;
                cmd.write(&tm)?;
                Ok(0)
            }
            cmd @ SetTime => {
                // Linux `rtc_dev_ioctl` `RTC_SET_TIME`: capability first,
                // then copy, then `rtc_set_time`. Hardware only.
                crate::time::require_sys_time()?;
                let tm = cmd.read()?;
                let calendar = system_time_from_rtc_time(tm)?;
                match aster_time::write_rtc(calendar) {
                    Ok(()) => Ok(0),
                    Err(RtcWriteError::Unsupported | RtcWriteError::InvalidTime) => {
                        return_errno_with_message!(
                            Errno::EINVAL,
                            "RTC_SET_TIME rejected by the RTC driver"
                        )
                    }
                    Err(RtcWriteError::Hardware) => {
                        return_errno_with_message!(Errno::EIO, "RTC_SET_TIME did not read back")
                    }
                }
            }
            UieOn | UieOff => {
                // util-linux `hwclock` treats ENOTTY as “poll RTC_RD_TIME”.
                return_errno_with_message!(Errno::ENOTTY, "RTC update IRQs are not implemented")
            }
            _ => {
                return_errno_with_message!(Errno::ENOTTY, "the rtc ioctl command is unknown")
            }
        })
    }
}

pub(super) fn init_in_first_kthread() {
    RTC_MAJOR.call_once(|| acquire_major(MajorId::new(RTC_MAJOR_ID)).unwrap());
    let device = RtcDevice::new();
    RTC_ID.call_once(|| device.id());
    register(device).expect("failed to register /dev/rtc0");
    // `/dev/rtc` alias (same major:minor). Linux udev creates that
    // symlink; we add a second node with the same device ID.
    if let Some(id) = RTC_ID.get().copied()
        && let Err(err) = create_node(DevtmpfsNode::new(
            DeviceType::Char,
            id,
            DevtmpfsNodeMeta::new("rtc").unwrap(),
        ))
    {
        warn!("[k1-rtc] /dev/rtc alias: {:?}", err);
    }
    info!("[k1-rtc] /dev/rtc0");
}

#[cfg(ktest)]
mod ioctl_number_tests {
    use ostd::prelude::*;

    use super::ioctl_defs::*;
    use crate::util::ioctl::RawIoctl;

    #[ktest]
    fn linux_rtc_ioctl_numbers() {
        assert!(RdTime::try_from_raw(RawIoctl::new(0x8024_7009, 0)).is_some());
        assert!(SetTime::try_from_raw(RawIoctl::new(0x4024_700a, 0)).is_some());
        assert!(UieOn::try_from_raw(RawIoctl::new(0x7003, 0)).is_some());
        assert!(UieOff::try_from_raw(RawIoctl::new(0x7004, 0)).is_some());

        // Wrong size bit: `_IOR('p', 0x09, …)` with size 0 must not match.
        assert!(RdTime::try_from_raw(RawIoctl::new(0x8000_7009, 0)).is_none());
        // Wrong nr.
        assert!(RdTime::try_from_raw(RawIoctl::new(0x8024_7008, 0)).is_none());
    }
}
