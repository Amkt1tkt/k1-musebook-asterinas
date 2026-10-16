// SPDX-License-Identifier: MPL-2.0

//! Console output.

use core::fmt::{Arguments, Write};
// K1 MUSE Book --- (re-entrant UART owner token)
use core::sync::atomic::{AtomicU64, Ordering};

// K1 MUSE Book ---
// K1 MUSE Book --- (make early serial output tolerate nested diagnostics)
// use crate::arch::serial::SERIAL_PORT;
use crate::{
    arch::serial::SERIAL_PORT,
    sync::{LocalIrqDisabled, SpinLock},
};
// K1 MUSE Book ---

pub mod uart_ns16650a;

// K1 MUSE Book --- (boot splash mirrors UART bytes into a fixed ring; hook must not draw)
/// Optional sink for the bytes [`early_print`] just wrote to the UART.
///
/// Installed by the kernel boot splash so a log strip can mirror serial
/// output. The first registration wins. The hook must be cheap and must
/// not draw, allocate, or take a lock that a printer already holds: it
/// runs with IRQs off, on any CPU, including inside a panic.
static MIRROR: spin::Once<fn(&[u8])> = spin::Once::new();

/// Registers a function called with each [`early_print`] payload *after*
/// the UART write.
///
/// The hook receives the formatted bytes (possibly in several chunks,
/// one per `core::fmt` `write_str`). It must not call [`early_print`]
/// itself. A splash implementation should only push into a fixed ring
/// and let a dedicated thread paint.
pub fn set_early_print_mirror(f: fn(&[u8])) {
    MIRROR.call_once(|| f);
}

fn invoke_mirror(args: Arguments) {
    let Some(&mirror) = MIRROR.get() else {
        return;
    };
    struct Sink(fn(&[u8]));
    impl Write for Sink {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            (self.0)(s.as_bytes());
            Ok(())
        }
    }
    let _ = Sink(mirror).write_fmt(args);
}
// K1 MUSE Book ---

// K1 MUSE Book --- (serialize early_print and kernel println! on one UART lock)
/// No owner. Distinct from every [`owner_token`], including the `get_base()
/// == 0` fallback of `1`.
const NO_OWNER: u64 = 0;

/// Longest slice taken under one [`with_serial_output`] hold on the
/// user-space `SbiConsole::send` path.
///
/// `sbi_rt::console_write_byte` busy-waits the UART. At 115200 8N1 that
/// is 10 bits/byte → 86.8 µs/byte, so 256 bytes is **22 ms** of IRQ-off.
/// Tty `write(2)` hands this path up to 4096 bytes; holding across that
/// whole buffer would be 355 ms.
///
/// A newline ends the hold early. Longer lines may interleave at a chunk
/// boundary so bulk user output cannot monopolize interrupt-disabled time.
pub const SERIAL_OUTPUT_MAX_BYTES: usize = 256;

/// How many leading bytes of `buf` belong under one lock hold: up to
/// and including the next `\n`, but never more than `max`.
pub fn serial_output_span(buf: &[u8], max: usize) -> usize {
    if buf.is_empty() || max == 0 {
        return 0;
    }
    let span = buf.len().min(max);
    buf[..span]
        .iter()
        .position(|&b| b == b'\n')
        .map(|i| i + 1)
        .unwrap_or(span)
}

/// Serializes one complete write to the early UART.
///
/// [`early_print`] and the kernel `println!` path (via
/// [`with_serial_output`]) share this lock so concurrent transmit paths do
/// not interleave bytes within a bounded write.
///
/// This is a dedicated `SpinLock<(), LocalIrqDisabled>`, not `SERIAL_PORT`.
/// On x86/LoongArch `SERIAL_PORT` also covers receive; holding it across
/// the kernel `println!` path would deadlock when `Uart::send` takes the
/// same lock.
///
/// Nested calls on this CPU take the re-entrant path instead of spinning
/// (a panic during a print must not hang).
pub fn with_serial_output<R>(f: impl FnOnce() -> R) -> R {
    let me = owner_token();
    if PRINT_OWNER.load(Ordering::Relaxed) == me {
        return f();
    }

    let _guard = PRINT_LOCK.lock();
    PRINT_OWNER.store(owner_token(), Ordering::Relaxed);
    let _owner = PrintOwnerGuard;
    f()
}

struct PrintOwnerGuard;

impl Drop for PrintOwnerGuard {
    fn drop(&mut self) {
        PRINT_OWNER.store(NO_OWNER, Ordering::Release);
    }
}

/// Prints formatted arguments to the console.
pub fn early_print(args: Arguments) {
    // let Some(serial) = SERIAL_PORT.get() else {
    //     return;
    // };
    //
    // #[cfg(target_arch = "x86_64")]
    // crate::arch::if_tdx_enabled!({
    //     let _guard = serial.lock();
    //     tdx_guest::print(args);
    // } else {
    //     serial.lock().write_fmt(args).unwrap();
    // });
    // #[cfg(not(target_arch = "x86_64"))]
    // serial.lock().write_fmt(args).unwrap();
    with_serial_output(|| early_print_under_lock(args));
}

fn early_print_under_lock(args: Arguments) {
    let Some(serial) = SERIAL_PORT.get() else {
        // K1 MUSE Book --- (boot splash mirrors early_print; hook must not draw)
        invoke_mirror(args);
        // K1 MUSE Book ---
        return;
    };

    #[cfg(target_arch = "x86_64")]
    crate::arch::if_tdx_enabled!({
        let _guard = serial.lock();
        tdx_guest::print(args);
    } else {
        write_to_serial(serial, args);
    });
    #[cfg(not(target_arch = "x86_64"))]
    write_to_serial(serial, args);
    // K1 MUSE Book --- (boot splash mirrors early_print; hook must not draw)
    invoke_mirror(args);
    // K1 MUSE Book ---
}

fn write_to_serial<T: Write>(serial: &SpinLock<T, LocalIrqDisabled>, args: Arguments) {
    if let Some(mut guard) = serial.try_lock() {
        guard.write_fmt(args).unwrap();
        return;
    }

    crate::arch::serial::write_fmt_raw(args);
}

fn owner_token() -> u64 {
    let base = crate::arch::cpu::local::get_base();
    if base == 0 { 1 } else { base }
}

static PRINT_LOCK: SpinLock<(), LocalIrqDisabled> = SpinLock::new(());
static PRINT_OWNER: AtomicU64 = AtomicU64::new(NO_OWNER);
// K1 MUSE Book ---

/// Prints to the console.
#[macro_export]
macro_rules! early_print {
    ($fmt: literal $(, $($arg: tt)+)?) => {
        $crate::console::early_print(format_args!($fmt $(, $($arg)+)?))
    }
}

/// Prints to the console with a newline.
#[macro_export]
macro_rules! early_println {
    () => { $crate::early_print!("\n") };
    ($fmt: literal $(, $($arg: tt)+)?) => {
        $crate::console::early_print(format_args!(concat!($fmt, "\n") $(, $($arg)+)?))
    }
}

// K1 MUSE Book --- (ktests for with_serial_output / serial_output_span)
#[cfg(ktest)]
mod test {
    use super::*;
    use crate::prelude::ktest;

    #[ktest]
    fn nested_serial_output_does_not_deadlock() {
        let mut n = 0;
        with_serial_output(|| {
            n += 1;
            with_serial_output(|| {
                n += 1;
            });
        });
        assert_eq!(n, 2);
    }

    #[ktest]
    fn early_print_is_reentrant_under_serial_output() {
        with_serial_output(|| {
            early_print(format_args!(""));
        });
    }

    #[ktest]
    fn serial_output_disables_local_irqs() {
        with_serial_output(|| {
            assert!(!crate::arch::irq::is_local_enabled());
        });
    }

    #[ktest]
    fn serial_output_span_stops_after_newline() {
        assert_eq!(serial_output_span(b"hello\nworld", 256), 6);
        assert_eq!(serial_output_span(b"hello", 256), 5);
        assert_eq!(serial_output_span(b"", 256), 0);
    }

    #[ktest]
    fn serial_output_span_caps_a_line_with_no_newline() {
        assert_eq!(serial_output_span(&[b'x'; 400], 256), 256);
        assert_eq!(serial_output_span(b"abcdef\n", 3), 3);
    }
}
// K1 MUSE Book ---
