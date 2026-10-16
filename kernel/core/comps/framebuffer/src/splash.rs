// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: kernel boot splash public API (no-op on non-riscv).

//! Kernel boot splash shown on the K1 eDP until the compositor takes over.
//!
//! On non-RISC-V targets every function is a no-op so `aster-core` can
//! call this API unconditionally.

use core::str::FromStr;

#[cfg(target_arch = "riscv64")]
use spin::Once;

/// How the splash finished.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EndReason {
    /// Userspace KMS programmed a new scanout address.
    Compositor,
    /// No compositor commit arrived within [`TIMEOUT_SECS`].
    Timeout,
}

/// Seconds to wait for a compositor page-flip before handing the FB back to VT.
pub const TIMEOUT_SECS: u64 = 120;

/// `k1.splash=` — what to draw after the panel comes up.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SplashMode {
    /// Draw nothing and hand the framebuffer directly to the VT.
    Off,
    /// Logo, status, progress bar; no kernel log strip.
    Quiet,
    /// Full splash including the recent-`early_println!` strip (default).
    Log,
}

impl FromStr for SplashMode {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "off" | "0" | "none" | "false" | "no" => Ok(Self::Off),
            "quiet" | "silent" => Ok(Self::Quiet),
            "log" | "on" | "1" | "true" | "yes" | "verbose" => Ok(Self::Log),
            _ => Err(()),
        }
    }
}

#[cfg(target_arch = "riscv64")]
static SPLASH_MODE: Once<SplashMode> = Once::new();
#[cfg(target_arch = "riscv64")]
aster_cmdline::define_kv_param!("k1.splash", SPLASH_MODE);

/// Parsed `k1.splash`, or [`SplashMode::Log`] when the knob is absent.
#[cfg_attr(not(target_arch = "riscv64"), expect(dead_code))]
pub(crate) fn mode() -> SplashMode {
    #[cfg(target_arch = "riscv64")]
    {
        *SPLASH_MODE.get().unwrap_or(&SplashMode::Log)
    }
    #[cfg(not(target_arch = "riscv64"))]
    SplashMode::Off
}

/// Whether the splash currently owns the scanout buffer.
///
/// Cheap (`AtomicBool`). Callers in hot paths (every `execve`, every VT
/// activate) should return immediately when this is `false`.
pub fn is_active() -> bool {
    #[cfg(target_arch = "riscv64")]
    {
        crate::arch::k1_splash::is_active()
    }
    #[cfg(not(target_arch = "riscv64"))]
    false
}

/// Record a successful `execve`. `basename` may be a full path; only the
/// last component is matched against the milestone table.
///
/// No-op when the splash is inactive.
pub fn on_exec(basename: &str) {
    #[cfg(target_arch = "riscv64")]
    crate::arch::k1_splash::on_exec(basename);
    #[cfg(not(target_arch = "riscv64"))]
    let _ = basename;
}

/// End the splash. Idempotent: the first call wins.
pub fn end(reason: EndReason) {
    #[cfg(target_arch = "riscv64")]
    crate::arch::k1_splash::end(reason);
    #[cfg(not(target_arch = "riscv64"))]
    let _ = reason;
}

/// Register the kernel-side finish hook.
///
/// # Layering
///
/// `aster-framebuffer` sits below `aster-core` and cannot call into the
/// VT manager. The splash ends from this crate (KMS first page-flip, or
/// the 120 s timeout in the splash thread), so the kernel VT init
/// registers a callback here. The first registration wins.
///
/// The callback should activate VT1 so its shadow buffer is flushed:
/// * [`EndReason::Timeout`] — the autologin shell becomes visible.
/// * [`EndReason::Compositor`] — the flush paints the *old* scanout,
///   which is no longer displayed, and is therefore harmless.
///
/// VT init also checks [`is_active`] after registering, so a splash that
/// already ended (or never started) still gets an activate and does not
/// depend on the callback firing.
pub fn set_on_end(f: fn(EndReason)) {
    #[cfg(target_arch = "riscv64")]
    crate::arch::k1_splash::set_on_end(f);
    #[cfg(not(target_arch = "riscv64"))]
    let _ = f;
}

/// Splash thread body. Spawn from `aster-core` via `ThreadOptions`.
///
/// A bare `ostd::task::TaskOptions::spawn` is not scheduled (`as_thread`
/// fails), so the kernel crate must create the thread. Returns immediately
/// when the splash is not active.
pub fn run_loop() {
    #[cfg(target_arch = "riscv64")]
    crate::arch::k1_splash::run_loop();
}
