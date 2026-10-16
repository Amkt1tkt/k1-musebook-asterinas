// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: four-bank PXA-style GPIO plus shared PLIC 58 demux.

//! SpacemiT K1 GPIO — read/write plus a shared-edge IRQ controller.
//!
//! Cross-checked against Linux `drivers/gpio/gpio-k1x.c` and
//! `k1-x.dtsi` `gpio@d4019000` (`spacemit,k1x-gpio`, PLIC 58).
//!
//! Banks 0–2 are interleaved (offsets 0/4/8); bank 3 sits at `+0x100`.
//! Edge IRQs use the PXA-style set/clear pair (GSRER/GCRER, GSFER/GCFER)
//! and GEDR (write-1-to-clear). GAPMASK must be all-ones or the AP never
//! sees a pin.

use core::sync::atomic::{AtomicU32, Ordering};

use ostd::{
    arch::{
        boot::DEVICE_TREE,
        irq::{IRQ_CHIP, InterruptSourceInFdt, MappedIrqLine},
    },
    io::IoMem,
    irq::IrqLine,
    mm::{HasSize, VmIoOnce, dma::dma_io_fence},
    sync::{LocalIrqDisabled, SpinLock},
};
use spin::Once;

use crate::{Edge, GpioIrqError};

const GPIO_BASE: usize = 0xd401_9000;
const GPIO_SIZE: usize = 0x800;
const GPIO_PLIC_IRQ: u32 = 58;
const NGPIO: u32 = 128;
const NBANK: usize = 4;

/// Relative offset of each bank's register block (U-Boot `offset[]`).
const BANK_OFF: [usize; 4] = [0x00, 0x04, 0x08, 0x100];

const REG_GPLR: usize = 0x00;
const REG_GPSR: usize = 0x18;
const REG_GPCR: usize = 0x24;
const REG_GEDR: usize = 0x48;
const REG_GSDR: usize = 0x54;
const REG_GCDR: usize = 0x60;
const REG_GSRER: usize = 0x6c;
const REG_GCRER: usize = 0x78;
const REG_GSFER: usize = 0x84;
const REG_GCFER: usize = 0x90;
const REG_GAPMASK: usize = 0x9c;

/// MUSE Book hall / lid switch (`lid-gpios = <&gpio 74 0>`).
const HALL_GPIO: u32 = 74;

type IrqHandler = fn(u32);

struct K1Gpio {
    io: IoMem,
}

static GPIO: Once<K1Gpio> = Once::new();
static IRQ_LINE: Once<MappedIrqLine> = Once::new();
static IRQ_MASK: [AtomicU32; NBANK] = [const { AtomicU32::new(0) }; NBANK];
// Registration can run with IRQs enabled; the demux shares this table.
static HANDLERS: SpinLock<[Option<IrqHandler>; NGPIO as usize], LocalIrqDisabled> =
    SpinLock::new([None; NGPIO as usize]);

/// Bank index and bit within that bank. `None` if `gpio` is out of range.
pub(crate) fn bank_and_bit(gpio: u32) -> Option<(usize, u32)> {
    if gpio >= NGPIO {
        return None;
    }
    Some(((gpio >> 5) as usize, gpio & 0x1f))
}

/// Reconstruct a GPIO number from a demuxed GEDR bit.
pub(crate) fn gpio_from_bank_bit(bank: usize, bit: u32) -> u32 {
    (bank as u32) * 32 + (bit & 0x1f)
}

/// `(rising, falling)` enable bits for `edge`.
pub(crate) fn edge_enables(edge: Edge) -> (bool, bool) {
    match edge {
        Edge::Rising => (true, false),
        Edge::Falling => (false, true),
        Edge::Both => (true, true),
    }
}

/// Bits of `gedr` that belong to this driver after applying the software mask.
///
/// Linux writes GEDR first (ack), then dispatches `gedr & irq_mask`.
pub(crate) fn pending_after_ack(gedr: u32, irq_mask: u32) -> u32 {
    gedr & irq_mask
}

pub(super) fn init() {
    let Some(dt) = DEVICE_TREE.get() else {
        return;
    };
    if dt
        .find_compatible(&["spacemit,k1-x", "spacemit,k1"])
        .is_none()
    {
        return;
    }

    k1_musebook_pinctrl::enable(k1_musebook_pinctrl::Periph::Gpio);

    let (base, size) = gpio_mmio_from_dt().unwrap_or((GPIO_BASE, GPIO_SIZE));
    let Some(end) = base.checked_add(size) else {
        return;
    };
    let Ok(io) = IoMem::acquire(base..end) else {
        log::warn!("[k1-gpio] failed to map GPIO MMIO at {:#x}", base);
        return;
    };
    if io.size() < BANK_OFF[NBANK - 1] + REG_GAPMASK + 4 {
        log::warn!("[k1-gpio] DT register window is too small");
        return;
    }
    log::info!("[k1-gpio] mapped at {:#x}", base);

    let gpio = K1Gpio { io };

    for bank in 0..NBANK {
        // Same initialization as Linux `k1x_gpio_probe`: drop every edge enable
        // and unmask edge detection toward the AP.
        gpio.write_bank(bank, REG_GCFER, 0xffff_ffff);
        gpio.write_bank(bank, REG_GCRER, 0xffff_ffff);
        gpio.write_bank(bank, REG_GAPMASK, 0xffff_ffff);
    }

    let level = gpio.get_value(HALL_GPIO);
    log::info!(
        "[k1-gpio] GPIO{} (hall/lid) = {} (1=open/high typical)",
        HALL_GPIO,
        level
    );

    GPIO.call_once(|| gpio);
    install_shared_irq();
    log::info!("[k1-gpio] probe OK");
}

fn gpio_mmio_from_dt() -> Option<(usize, usize)> {
    let dt = DEVICE_TREE.get()?;
    let node = dt.find_compatible(&["spacemit,k1x-gpio"])?;
    let region = node.reg().and_then(|mut regions| regions.next())?;
    Some((
        region.starting_address as usize,
        region.size.unwrap_or(GPIO_SIZE),
    ))
}

fn lookup_gpio_interrupt() -> Option<InterruptSourceInFdt> {
    let dt = DEVICE_TREE.get()?;
    if let Some(node) = dt.find_compatible(&["spacemit,k1x-gpio"])
        && let (Some(interrupt), Some(parent)) = (
            node.interrupts().and_then(|mut pins| pins.next()),
            node.property("interrupt-parent")
                .and_then(|parent| parent.as_usize()),
        )
    {
        return Some(InterruptSourceInFdt {
            arguments: [interrupt as u32],
            interrupt_parent: parent as u32,
        });
    }

    let plic = dt.find_compatible(&["sifive,plic-1.0.0", "riscv,plic0"])?;
    let parent = plic.property("phandle")?.as_usize()? as u32;
    Some(InterruptSourceInFdt {
        arguments: [GPIO_PLIC_IRQ],
        interrupt_parent: parent,
    })
}

fn install_shared_irq() {
    let Some(interrupt) = lookup_gpio_interrupt() else {
        log::warn!("[k1-gpio] no PLIC phandle; GPIO IRQs stay disabled");
        return;
    };
    let Ok(line) = IrqLine::alloc() else {
        log::warn!("[k1-gpio] could not allocate an IRQ line");
        return;
    };
    let Some(chip) = IRQ_CHIP.get() else {
        log::warn!("[k1-gpio] no interrupt controller");
        return;
    };
    let mut mapped = match chip.map_fdt_pin_to(interrupt, line) {
        Ok(mapped) => mapped,
        Err(err) => {
            log::warn!(
                "[k1-gpio] could not map PLIC {} on phandle {:#x}: {:?}",
                interrupt.arguments[0],
                interrupt.interrupt_parent,
                err
            );
            return;
        }
    };
    mapped.on_active(move |_trap| demux_gpio_irq());
    IRQ_LINE.call_once(|| mapped);
    log::info!(
        "[k1-gpio] shared IRQ PLIC {} installed",
        interrupt.arguments[0]
    );
}

fn demux_gpio_irq() {
    let Some(g) = GPIO.get() else {
        return;
    };
    for bank in 0..NBANK {
        let gedr = g.read_bank(bank, REG_GEDR);
        if gedr == 0 {
            continue;
        }
        g.write_bank(bank, REG_GEDR, gedr);
        let pending = pending_after_ack(gedr, IRQ_MASK[bank].load(Ordering::Relaxed));
        if pending == 0 {
            continue;
        }
        // Keep the IRQ-safe lock short and invoke callbacks outside it. Each
        // pointer is permanent after registration, so a bank snapshot suffices.
        let handlers: [Option<IrqHandler>; 32] = {
            let table = HANDLERS.lock();
            core::array::from_fn(|bit| table[bank * 32 + bit])
        };
        let mut bits = pending;
        while bits != 0 {
            let bit = bits.trailing_zeros();
            bits &= bits.wrapping_sub(1);
            let gpio = gpio_from_bank_bit(bank, bit);
            if let Some(handler) = handlers[bit as usize] {
                handler(gpio);
            }
        }
    }
}

/// Public read of a GPIO level after [`init`].
pub(super) fn get_value(gpio: u32) -> Option<u32> {
    bank_and_bit(gpio)?;
    Some(GPIO.get()?.get_value(gpio))
}

/// Drive `gpio` as output to `value` (0 or 1). Returns `false` if not ready.
pub(super) fn set_value(gpio: u32, value: u32) -> bool {
    let Some(g) = GPIO.get() else {
        return false;
    };
    if bank_and_bit(gpio).is_none() {
        return false;
    }
    g.set_value(gpio, value);
    true
}

pub(super) fn register_irq(gpio: u32, edge: Edge, handler: fn(u32)) -> Result<(), GpioIrqError> {
    let Some(g) = GPIO.get() else {
        return Err(GpioIrqError::Unavailable);
    };
    let Some((bank, bit)) = bank_and_bit(gpio) else {
        return Err(GpioIrqError::InvalidGpio);
    };
    if IRQ_LINE.get().is_none() {
        return Err(GpioIrqError::IrqMapFailed);
    }

    let mask = 1u32 << bit;
    {
        let mut handlers = HANDLERS.lock();
        if handlers[gpio as usize].is_some() {
            return Err(GpioIrqError::Busy);
        }
        handlers[gpio as usize] = Some(handler);
    }

    // Input: clear GPDR bit via GCDR, then program edges and unmask.
    g.write_bank(bank, REG_GCDR, mask);
    let (rise, fall) = edge_enables(edge);
    if rise {
        g.write_bank(bank, REG_GSRER, mask);
    } else {
        g.write_bank(bank, REG_GCRER, mask);
    }
    if fall {
        g.write_bank(bank, REG_GSFER, mask);
    } else {
        g.write_bank(bank, REG_GCFER, mask);
    }
    g.write_bank(bank, REG_GEDR, mask);
    dma_io_fence();
    IRQ_MASK[bank].fetch_or(mask, Ordering::Release);

    Ok(())
}

impl K1Gpio {
    fn read_bank(&self, bank: usize, reg: usize) -> u32 {
        let off = BANK_OFF[bank] + reg;
        self.io.read_once(off).unwrap()
    }

    fn write_bank(&self, bank: usize, reg: usize, val: u32) {
        let off = BANK_OFF[bank] + reg;
        self.io.write_once(off, &val).unwrap();
        dma_io_fence();
    }

    fn get_value(&self, gpio: u32) -> u32 {
        let Some((bank, bit)) = bank_and_bit(gpio) else {
            return 0;
        };
        let gplr = self.read_bank(bank, REG_GPLR);
        (gplr >> bit) & 1
    }

    fn set_value(&self, gpio: u32, value: u32) {
        let Some((bank, bit)) = bank_and_bit(gpio) else {
            return;
        };
        let mask = 1u32 << bit;
        self.write_bank(bank, REG_GSDR, mask);
        if value != 0 {
            self.write_bank(bank, REG_GPSR, mask);
        } else {
            self.write_bank(bank, REG_GPCR, mask);
        }
    }
}

#[cfg(ktest)]
mod test {
    use ostd::prelude::ktest;

    use super::{bank_and_bit, edge_enables, gpio_from_bank_bit, pending_after_ack};
    use crate::Edge;

    #[ktest]
    fn gpio50_is_bank1_bit18() {
        assert_eq!(bank_and_bit(50), Some((1, 18)));
        assert_eq!(gpio_from_bank_bit(1, 18), 50);
    }

    #[ktest]
    fn gpio74_is_bank2_bit10() {
        assert_eq!(bank_and_bit(74), Some((2, 10)));
        assert_eq!(gpio_from_bank_bit(2, 10), 74);
    }

    #[ktest]
    fn gpio0_and_gpio127_are_the_ends_of_the_four_banks() {
        assert_eq!(bank_and_bit(0), Some((0, 0)));
        assert_eq!(bank_and_bit(31), Some((0, 31)));
        assert_eq!(bank_and_bit(32), Some((1, 0)));
        assert_eq!(bank_and_bit(127), Some((3, 31)));
        assert_eq!(bank_and_bit(128), None);
    }

    #[ktest]
    fn falling_edge_enables_only_gfer() {
        assert_eq!(edge_enables(Edge::Falling), (false, true));
        assert_eq!(edge_enables(Edge::Rising), (true, false));
        assert_eq!(edge_enables(Edge::Both), (true, true));
    }

    #[ktest]
    fn gedr_bits_outside_the_software_mask_are_acked_but_not_dispatched() {
        // Bit 18 (GPIO50) latched, but only bit 10 is registered.
        let gedr = 1u32 << 18 | 1u32 << 10;
        let mask = 1u32 << 10;
        assert_eq!(pending_after_ack(gedr, mask), 1u32 << 10);
        assert_eq!(pending_after_ack(gedr, 0), 0);
    }
}
