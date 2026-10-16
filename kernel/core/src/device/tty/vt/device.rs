// SPDX-License-Identifier: MPL-2.0

use alloc::sync::Arc;

use aster_console::{AnyConsoleDevice, ConsoleCallback};

use crate::device::tty::vt::manager::{VtConsole, default_vt};

const DEFAULT_VT_CONSOLE_NAME: &str = "Default-VT-Console";

/// A console device that forwards output to the default VT console.
#[derive(Debug)]
struct DefaultVtConsoleDevice;

impl DefaultVtConsoleDevice {
    fn vt_console(&self) -> &VtConsole {
        // Keep kernel logs on the default VT console so log visibility is
        // predictable for debugging.
        default_vt().driver().vt_console()
    }
}

impl AnyConsoleDevice for DefaultVtConsoleDevice {
    fn send(&self, buf: &[u8]) {
        self.vt_console().send(buf);
    }

    fn register_callback(&self, _callback: &'static ConsoleCallback) {}
}

pub(super) fn init_in_first_process() {
    // K1 MUSE Book --- (the K1 panel keeps kernel messages on the serial console)
    // aster_console::register_device(
    //     DEFAULT_VT_CONSOLE_NAME.into(),
    //     Arc::new(DefaultVtConsoleDevice),
    // );
    let board_panel = {
        #[cfg(target_arch = "riscv64")]
        {
            aster_framebuffer::FRAMEBUFFER.get().is_some()
                && ostd::arch::boot::DEVICE_TREE.get().is_some_and(|tree| {
                    tree.find_compatible(&["spacemit,k1-x", "spacemit,k1"])
                        .is_some()
                })
        }
        #[cfg(not(target_arch = "riscv64"))]
        {
            false
        }
    };
    if !board_panel {
        aster_console::register_device(
            DEFAULT_VT_CONSOLE_NAME.into(),
            Arc::new(DefaultVtConsoleDevice),
        );
    }
    // K1 MUSE Book ---
}
