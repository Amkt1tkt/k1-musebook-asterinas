// SPDX-License-Identifier: MIT OR GPL-2.0-only
// K1 MUSE Book: documented GPU clock and autosuspend policies.

//! Optional GPU performance knobs, parsed from the kernel command line.
//!
//! Unset options preserve the default clock selection and 50 ms autosuspend delay.
//!
//! | Parameter | Default | Alternative |
//! | --- | --- | --- |
//! | `k1.gpuclk` | reset mux/divider (307.2 MHz on K1) | `614` selects 614.4 MHz |
//! | `k1.gpuidle` | 50 ms autosuspend | positive milliseconds or `off` |
//!

use core::str::FromStr;

use spin::Once;

/// Default autosuspend delay in milliseconds.
pub(crate) const DEFAULT_IDLE_MS: u64 = crate::power::AUTOSUSPEND_DELAY_MS;

/// `APMU_GPU_CLK_RES_CTRL` offset in the APMU window.
///
/// BSP `drivers/clk/spacemit/ccu-spacemit-k1x.c`:
/// `#define APMU_GPU_CLK_RES_CTRL 0xcc`.
#[cfg(any(target_arch = "riscv64", test, ktest))]
pub(crate) const APMU_GPU_CLK_RES_CTRL: usize = 0xcc;

/// Divider field in `APMU_GPU_CLK_RES_CTRL`: bits `[14:12]`.
///
/// BSP `SPACEMIT_CCU_DIV_FC_MUX_GATE(gpu_clk, …, 12, 3, BIT(15), 18, 3, …)`.
/// `CCU_DIV_INIT(12, 3, NULL, 0)` ⇒ Linux `divide = field + 1`.
#[cfg(any(target_arch = "riscv64", test, ktest))]
pub(crate) const GPU_CLK_DIV_SHIFT: u32 = 12;

/// Width-3 mask for [`GPU_CLK_DIV_SHIFT`].
#[cfg(any(target_arch = "riscv64", test, ktest))]
pub(crate) const GPU_CLK_DIV_MASK: u32 = 0x7 << GPU_CLK_DIV_SHIFT;

/// Mux field in `APMU_GPU_CLK_RES_CTRL`: bits `[20:18]`.
#[cfg(any(target_arch = "riscv64", test, ktest))]
pub(crate) const GPU_CLK_MUX_SHIFT: u32 = 18;

/// Width-3 mask for [`GPU_CLK_MUX_SHIFT`].
#[cfg(any(target_arch = "riscv64", test, ktest))]
pub(crate) const GPU_CLK_MUX_MASK: u32 = 0x7 << GPU_CLK_MUX_SHIFT;

/// `gpu_parent_names[0]` = `pll1_d4_614p4` = 614.4 MHz.
#[cfg(any(target_arch = "riscv64", test, ktest))]
pub(crate) const GPU_CLK_614_MUX: u32 = 0;

/// Divider field 0 ⇒ ÷1 ⇒ 614.4 MHz on parent 0.
#[cfg(any(target_arch = "riscv64", test, ktest))]
pub(crate) const GPU_CLK_614_DIV_FIELD: u32 = 0;

/// Rate the firmware is told after a successful `k1.gpuclk=614`.
#[cfg(any(target_arch = "riscv64", test, ktest))]
pub(crate) const GPU_CLK_614_HZ: u32 = 614_400_000;

/// Bits written into mux+div for 614.4 MHz (both fields zero).
#[cfg(any(target_arch = "riscv64", test, ktest))]
pub(crate) const fn gpu_clk_614_word() -> u32 {
    (GPU_CLK_614_MUX << GPU_CLK_MUX_SHIFT) | (GPU_CLK_614_DIV_FIELD << GPU_CLK_DIV_SHIFT)
}

/// What `k1.gpuclk=` asked for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GpuClk {
    /// Leave the mux and divider as reset configured them.
    ///
    /// On K1, mux 0 selects `pll1_d4_614p4`; divider field 1 means divide by two,
    /// giving 307.2 MHz. Only the clock-change strobe is committed.
    KeepReset,
    /// `clk_set_rate(gpu_clk, 614400000)` equivalent: same parent, ÷1.
    Mhz614,
}

impl FromStr for GpuClk {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "307" | "307.2" => Ok(Self::KeepReset),
            "614" | "614.4" => Ok(Self::Mhz614),
            _ => Err(()),
        }
    }
}

impl GpuClk {
    #[cfg(any(test, ktest))]
    fn is_default(self) -> bool {
        matches!(self, Self::KeepReset)
    }
}

/// What `k1.gpuidle=` asked for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GpuIdle {
    /// Autosuspend after this many milliseconds.
    DelayMs(u64),
    /// Never autosuspend. The GPU stays powered after the first client.
    Off,
}

impl FromStr for GpuIdle {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "off" | "never" | "none" | "disable" | "-1" => Ok(Self::Off),
            other => {
                let ms: u64 = other.parse().map_err(|_| ())?;
                // Reject zero: disabling the autosuspend delay would power down between
                // adjacent uses and repeatedly require firmware boot.
                if ms == 0 {
                    return Err(());
                }
                Ok(Self::DelayMs(ms))
            }
        }
    }
}

impl GpuIdle {
    #[cfg(any(test, ktest))]
    fn is_default(self) -> bool {
        matches!(self, Self::DelayMs(DEFAULT_IDLE_MS))
    }

    /// Whether autosuspend is disabled (stay powered).
    pub(crate) fn is_off(self) -> bool {
        matches!(self, Self::Off)
    }
}

static GPU_CLK: Once<GpuClk> = Once::new();
static GPU_IDLE: Once<GpuIdle> = Once::new();
aster_cmdline::define_kv_param!("k1.gpuclk", GPU_CLK);
aster_cmdline::define_kv_param!("k1.gpuidle", GPU_IDLE);

/// The requested core-clock policy.
#[cfg(any(target_arch = "riscv64", test, ktest))]
pub(crate) fn gpu_clk() -> GpuClk {
    GPU_CLK.get().copied().unwrap_or(GpuClk::KeepReset)
}

/// Whether the 614.4 MHz path should write mux/div.
#[cfg(any(target_arch = "riscv64", test, ktest))]
pub(crate) fn wants_core_614() -> bool {
    matches!(gpu_clk(), GpuClk::Mhz614)
}

/// The requested autosuspend policy.
pub(crate) fn gpu_idle() -> GpuIdle {
    GPU_IDLE
        .get()
        .copied()
        .unwrap_or(GpuIdle::DelayMs(DEFAULT_IDLE_MS))
}

/// Nanoseconds stored in [`crate::power::Pm`] at construction.
///
/// The default is `AUTOSUSPEND_DELAY_MS * 1_000_000`.
pub(crate) fn autosuspend_delay_ns() -> u64 {
    match gpu_idle() {
        GpuIdle::DelayMs(ms) => ms.saturating_mul(1_000_000),
        GpuIdle::Off => u64::MAX,
    }
}

#[cfg(any(test, ktest))]
mod tests {
    #[cfg(ktest)]
    use ostd::prelude::*;

    use super::*;

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn gpuclk_parses_the_documented_tokens() {
        assert_eq!("307".parse::<GpuClk>(), Ok(GpuClk::KeepReset));
        assert_eq!("307.2".parse::<GpuClk>(), Ok(GpuClk::KeepReset));
        assert_eq!("614".parse::<GpuClk>(), Ok(GpuClk::Mhz614));
        assert_eq!("614.4".parse::<GpuClk>(), Ok(GpuClk::Mhz614));
        assert!("819".parse::<GpuClk>().is_err());
        assert!("409".parse::<GpuClk>().is_err());
        assert!("".parse::<GpuClk>().is_err());
        assert!(GpuClk::KeepReset.is_default());
        assert!(!GpuClk::Mhz614.is_default());
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn gpuidle_parses_delay_and_off() {
        assert_eq!("50".parse::<GpuIdle>(), Ok(GpuIdle::DelayMs(50)));
        assert_eq!("2000".parse::<GpuIdle>(), Ok(GpuIdle::DelayMs(2000)));
        assert_eq!("1000".parse::<GpuIdle>(), Ok(GpuIdle::DelayMs(1000)));
        assert_eq!("off".parse::<GpuIdle>(), Ok(GpuIdle::Off));
        assert_eq!("-1".parse::<GpuIdle>(), Ok(GpuIdle::Off));
        assert!("0".parse::<GpuIdle>().is_err());
        assert!("bogus".parse::<GpuIdle>().is_err());
        assert!(GpuIdle::DelayMs(DEFAULT_IDLE_MS).is_default());
        assert!(!GpuIdle::DelayMs(2000).is_default());
        assert!(!GpuIdle::Off.is_default());
        assert_eq!(DEFAULT_IDLE_MS, 50);
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn unset_cmdline_preserves_defaults() {
        assert_eq!(gpu_clk(), GpuClk::KeepReset);
        assert!(!wants_core_614());
        assert_eq!(gpu_idle(), GpuIdle::DelayMs(DEFAULT_IDLE_MS));
        assert!(!gpu_idle().is_off());
        assert_eq!(autosuspend_delay_ns(), DEFAULT_IDLE_MS * 1_000_000);
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn gpuclk_614_word_is_mux0_div0() {
        // Parent 0, divider field 0. Clearing those fields is the whole write;
        // gate bit4 / reset bit1 / fc bit15 are left alone.
        assert_eq!(gpu_clk_614_word(), 0);
        assert_eq!(GPU_CLK_DIV_MASK | GPU_CLK_MUX_MASK, 0x001c_7000);
        assert_eq!(APMU_GPU_CLK_RES_CTRL, 0xcc);
        assert_eq!(GPU_CLK_614_HZ, 614_400_000);
        // 614.4 / 2 = 307.2: the reset divider field of 1 is the current board.
        assert_eq!(GPU_CLK_614_HZ / 2, 307_200_000);
    }
}
