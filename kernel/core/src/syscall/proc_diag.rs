// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: `k1.diag=proc` / `k1.diag=stderr` process and stderr console tee.
// Prints with `ostd::early_println!` (a console tee). `log::` would recurse or
// reorder relative to the syscall that is being observed.

//! Opt-in process lifecycle and stderr console diagnostics.
//!
//! A service's stderr may be routed to journald rather than the serial console.
//! These tags expose exec/exit context and stderr without changing the
//! always-on `terminating on SIG*` line:
//!
//! - `proc`: one line per resolved `execve` attempt (pid, ppid, path), one
//!   per failed `execve` lookup (path and errno), one per `exit_group` /
//!   thread `exit` with a non-zero code, and every unimplemented
//!   system call with the caller named. Upstream always `warn!`s ENOSYS;
//!   this additional `[enosys]` line has no print budget when `proc` is enabled.
//! - `stderr`: every `write` / `writev` / `pwritev` to fd 2 is echoed to the
//!   console (at most [`STDERR_TEE_MAX`] bytes per call), prefixed with the
//!   writer. Unterminated fragments are joined within a bounded per-process
//!   buffer and flushed on exit.
//!
//! Both are read once and cached; `diag_enabled` is cheap but the write
//! path is hot.

use core::sync::atomic::{AtomicU8, Ordering};

use crate::prelude::*;

/// Longest slice of one stderr write echoed to the console.
pub(super) const STDERR_TEE_MAX: usize = 768;

/// Total bytes of stderr ever echoed. The console is a 115200-baud UART
/// written synchronously; a chatty `RUST_LOG` would otherwise turn this
/// diagnostic into a system-wide slowdown. When the budget is gone one
/// notice says so and the tee goes quiet.
const STDERR_TEE_BUDGET: usize = 512 * 1024;
static STDERR_TEE_SPENT: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

const UNKNOWN: u8 = 0;
const OFF: u8 = 1;
const ON: u8 = 2;

static PROC: AtomicU8 = AtomicU8::new(UNKNOWN);
static STDERR: AtomicU8 = AtomicU8::new(UNKNOWN);

fn cached(cell: &AtomicU8, tag: &str) -> bool {
    match cell.load(Ordering::Relaxed) {
        ON => true,
        OFF => false,
        _ => {
            let on = aster_cmdline::diag_enabled(tag);
            cell.store(if on { ON } else { OFF }, Ordering::Relaxed);
            on
        }
    }
}

/// Whether `k1.diag=proc` is on.
pub(super) fn proc_on() -> bool {
    cached(&PROC, "proc")
}

/// Whether `k1.diag=stderr` is on.
pub(super) fn stderr_on() -> bool {
    cached(&STDERR, "stderr")
}

/// `pid=N ppid=M name=X` for the calling thread's process.
pub(super) fn who(ctx: &Context) -> String {
    alloc::format!(
        "pid={} ppid={} name={}",
        ctx.process.pid(),
        ctx.process.parent().pid(),
        ctx.posix_thread
            .thread_name()
            .lock()
            .as_cstr()
            .to_string_lossy()
    )
}

/// `k1.diag=proc`: name the caller of an unimplemented syscall. No line
/// budget -- systemd spends any small cap during boot (`open_tree` /
/// `fsopen` / `bpf`) before a desktop app can trip over a missing call.
pub(super) fn note_enosys(syscall_number: u64, ctx: &Context) {
    if proc_on() {
        ostd::early_println!("[enosys] syscall={} {}", syscall_number, who(ctx));
    }
}

/// Echoes up to [`STDERR_TEE_MAX`] bytes from the user buffers behind a
/// `write(2, ..)` / `writev(2, ..)` / `pwritev(2, ..)`. Reads a *copy* of
/// the user memory, so the syscall's own reader is untouched; a fault
/// while copying just truncates the echo.
pub(super) fn tee_stderr(ctx: &Context, readers: &mut [VmReader<'_>]) {
    let mut buf = alloc::vec![0u8; STDERR_TEE_MAX];
    let mut filled = 0usize;
    for reader in readers.iter_mut() {
        if filled >= buf.len() {
            break;
        }
        let mut writer = VmWriter::from(&mut buf[filled..]);
        match reader.read_fallible(&mut writer) {
            Ok(n) | Err((_, n)) => filled += n,
        }
    }
    if filled == 0 {
        return;
    }
    let spent = STDERR_TEE_SPENT.fetch_add(filled, Ordering::Relaxed);
    if spent >= STDERR_TEE_BUDGET {
        return;
    }
    if spent + filled >= STDERR_TEE_BUDGET {
        ostd::early_println!(
            "[stderr] {} KiB echoed; budget spent, tee off after this line",
            STDERR_TEE_BUDGET / 1024
        );
    }
    emit_lines(ctx, &buf[..filled]);
}

/// Per-process carry-over of an unterminated last line. Rust's backtrace
/// printer and `write!`-driven loggers can split a line across many writes.
/// Join those fragments until a newline or [`STDERR_TEE_MAX`] is reached.
/// Storage is bounded by [`CARRY_SLOTS`] and the per-line limit.
const CARRY_SLOTS: usize = 8;
static CARRY: SpinLock<[(u32, Vec<u8>); CARRY_SLOTS]> =
    SpinLock::new([const { (0, Vec::new()) }; CARRY_SLOTS]);

fn emit_lines(ctx: &Context, bytes: &[u8]) {
    let pid = ctx.process.pid();
    let who = who(ctx);
    // Only collect owned byte strings under CARRY. Formatting, UART output and
    // thread-name lookup must not run while holding this spin lock.
    let (evicted, lines, tail) = {
        let mut carry = CARRY.lock();
        let mut evicted = None;
        let slot = match carry.iter().position(|(p, _)| *p == pid) {
            Some(i) => i,
            None => {
                let i = carry
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, (_, v))| v.len())
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                if !carry[i].1.is_empty() {
                    evicted = Some((carry[i].0, core::mem::take(&mut carry[i].1)));
                }
                carry[i].0 = pid;
                i
            }
        };
        let pending = &mut carry[slot].1;
        pending.extend_from_slice(bytes);
        let mut lines = Vec::new();
        while let Some(nl) = pending.iter().position(|&b| b == b'\n') {
            lines.push(pending.drain(..=nl).collect::<Vec<u8>>());
        }
        let tail = if pending.len() >= STDERR_TEE_MAX {
            Some(core::mem::take(pending))
        } else {
            None
        };
        (evicted, lines, tail)
    };
    if let Some((old_pid, bytes)) = evicted {
        ostd::early_println!(
            "[stderr pid={} ...] {}",
            old_pid,
            String::from_utf8_lossy(&bytes).trim_end()
        );
    }
    for line in lines {
        let text = String::from_utf8_lossy(&line);
        let text = text.trim_end_matches(['\n', '\r']);
        if !text.is_empty() {
            ostd::early_println!("[stderr {}] {}", who, text);
        }
    }
    if let Some(bytes) = tail {
        ostd::early_println!(
            "[stderr {}] {}",
            who,
            String::from_utf8_lossy(&bytes).trim_end()
        );
    }
}

/// Flushes a pending fragment before its process exit line. The fragment is
/// removed under CARRY; obtaining the thread name and printing happen afterward.
fn flush_carry(ctx: &Context) {
    let pid = ctx.process.pid();
    let bytes = {
        let mut carry = CARRY.lock();
        carry
            .iter_mut()
            .find(|(p, _)| *p == pid)
            .and_then(|(p, pending)| {
                if pending.is_empty() {
                    return None;
                }
                *p = 0;
                Some(core::mem::take(pending))
            })
    };
    if let Some(bytes) = bytes {
        ostd::early_println!(
            "[stderr {}] {}",
            who(ctx),
            String::from_utf8_lossy(&bytes).trim_end()
        );
    }
}

/// Reports a non-zero exit under `k1.diag=proc`; flushes the process's
/// pending stderr fragment first under `k1.diag=stderr`.
pub(super) fn note_exit(ctx: &Context, how: &str, code: i32) {
    if stderr_on() {
        flush_carry(ctx);
    }
    if code != 0 && proc_on() {
        ostd::early_println!("[proc] {} {} code={}", who(ctx), how, code);
    }
}

/// `execve` lookup failures: one line per process for `ENOENT`, every line
/// for anything else. glibc's `execvp` walks `PATH` with one `execve` per
/// directory. Report the first miss to identify the requested program;
/// a later successful exec reports its resolved path.
pub(super) fn note_exec_failed(ctx: &Context, filename: &str, errno: Errno) {
    if !proc_on() {
        return;
    }
    static LAST_ENOENT_PID: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
    let pid = ctx.process.pid();
    if errno == Errno::ENOENT {
        if LAST_ENOENT_PID.swap(pid, Ordering::Relaxed) == pid {
            return;
        }
        ostd::early_println!(
            "[proc] exec {} failed: {} -> ENOENT (further ENOENT for this pid = PATH walk, not shown)",
            who(ctx),
            filename
        );
        return;
    }
    ostd::early_println!(
        "[proc] exec {} failed: {} -> {:?}",
        who(ctx),
        filename,
        errno
    );
}
