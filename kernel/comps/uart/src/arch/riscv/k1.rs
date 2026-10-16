// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: SBI console with DBCN output and timer-polled legacy input.

//! RISC-V console via RustSBI: DBCN for output and legacy `console_getchar` for input.
//!
//! RustSBI owns the UART and its interrupt source. Output uses DBCN;
//! input uses the firmware's legacy getchar interface on the timer tick.

use alloc::{string::ToString, sync::Arc, vec::Vec};

use aster_console::{AnyConsoleDevice, ConsoleCallback};
use ostd::{
    mm::VmReader,
    sync::{LocalIrqDisabled, SpinLock},
};
use spin::Once;

/// SBI-backed console: DBCN output, polled legacy getchar input.
struct SbiConsole {
    callbacks: SpinLock<Vec<&'static ConsoleCallback>, LocalIrqDisabled>,
}

impl core::fmt::Debug for SbiConsole {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SbiConsole").finish_non_exhaustive()
    }
}

impl AnyConsoleDevice for SbiConsole {
    fn send(&self, buf: &[u8]) {
        // Bound each IRQ-disabled output section so serial writes do not
        // suppress input polling for an entire tty buffer.
        let mut rest = buf;
        while !rest.is_empty() {
            let n = ostd::console::serial_output_span(rest, ostd::console::SERIAL_OUTPUT_MAX_BYTES);
            let (chunk, next) = rest.split_at(n);
            ostd::console::with_serial_output(|| write_sbi_bytes(chunk));
            rest = next;
        }
    }

    fn register_callback(&self, callback: &'static ConsoleCallback) {
        self.callbacks.lock().push(callback);
    }
}

fn write_sbi_bytes(buf: &[u8]) {
    for &b in buf {
        if b == b'\n' {
            let _ = sbi_rt::console_write_byte(b'\r');
        }
        let _ = sbi_rt::console_write_byte(b);
    }
}

impl SbiConsole {
    /// Poll SBI for pending input bytes and feed registered tty callbacks.
    fn poll_input(&self) {
        loop {
            // Legacy getchar: no character is `usize::MAX` (-1); otherwise 0..=255.
            // K1 firmware supplies input through the legacy getchar extension.
            #[expect(deprecated, reason = "K1 input uses the legacy SBI getchar extension")]
            let c = sbi_rt::legacy::console_getchar();
            if c == usize::MAX || c > 0xff {
                break;
            }
            let byte = [c as u8];
            let callbacks = self.callbacks.lock();
            for callback in callbacks.iter() {
                (callback)(VmReader::from(&byte[..]));
            }
        }
    }
}

static SBI_CONSOLE: Once<Arc<SbiConsole>> = Once::new();

pub(super) fn init() {
    let console = Arc::new(SbiConsole {
        callbacks: SpinLock::new(Vec::new()),
    });
    aster_console::register_device(
        aster_console::UART_CONSOLE_NAME.to_string(),
        console.clone(),
    );
    SBI_CONSOLE.call_once(|| console);

    // Poll input on every timer tick (1000 Hz). Callbacks may run in IRQ context.
    ostd::timer::register_callback_on_cpu(|| {
        if let Some(console) = SBI_CONSOLE.get() {
            console.poll_input();
        }
    });
}
