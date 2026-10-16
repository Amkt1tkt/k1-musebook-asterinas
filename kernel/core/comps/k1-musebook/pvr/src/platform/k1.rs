// SPDX-License-Identifier: MIT OR GPL-2.0-only
// K1 MUSE Book: K1 GPU power, clock, reset, and deferred IRQ integration.

//! SpacemiT K1 GPU platform integration.
//!
//! Platform hooks follow the BSP power-domain, clock-enable, reset-release sequence
//! and reverse it for shutdown. [`crate::power`] owns the ordering. A BVNC read
//! checks register access and GPU identity; it does not prove firmware execution.
//!
//! [`probe`] runs before filesystem-backed firmware loading. It maps registers,
//! checks identity, and retains the register window. [`attach`] consumes the window
//! and firmware image, validates their compatibility, and registers the DRM device.
//! Runtime power management boots the firmware when the device is used.

use alloc::{sync::Arc, vec::Vec};

use aster_softirq::Taskless;
use component::ComponentInitError;
use ostd::{
    arch::{
        boot::DEVICE_TREE,
        ccu,
        irq::{IRQ_CHIP, InterruptSourceInFdt, MappedIrqLine},
    },
    io::IoMem,
    irq::IrqLine,
    sync::SpinLock,
};
use spin::Once;

use crate::{
    device::{GpuId, PvrDevice, RegisterWindow},
    error::{PvrError, PvrResult},
    fw,
    power::{self, PlatformOps, Pm},
    rogue::cr,
};

/// The `imggpu` register window base and size.
///
/// The DTS node also carries the interrupt and clock references; prefer reading
/// them from the device tree over these constants, and treat the constants as a
/// cross-check that the node was found correctly.
///
/// Transcribed from `imggpu@cac00000` in the BSP's
/// `arch/riscv/boot/dts/spacemit/k1-x.dtsi` (line 2544):
/// `reg = <0x0 0xcac00000 0x0 0x80000>`. A wrong base is the one mistake this
/// driver cannot diagnose — every register reads back as all-zeroes or
/// all-ones with no error anywhere — so it is worth checking against the DTB
/// rather than trusting this constant.
pub const GPU_REG_BASE: usize = 0xcac0_0000;

/// The size of the register window, from the same `reg` property.
pub const GPU_REG_SIZE: usize = 0x8_0000;

/// The device tree `compatible` string of the GPU node.
pub const GPU_COMPATIBLE: &str = "img,rgx";

/// The GPU node's full device tree path, used as `OF_FULLNAME` in sysfs.
///
/// Must differ from the DPU's, or `drmFoldDuplicatedDevices()` collapses the
/// two into one `drmDevice`. See `ABI.md` device identity contract.
pub const GPU_OF_FULLNAME: &str = "/soc/imggpu@cac00000";

/// The GPU's interrupt number, from `interrupts = <75>` on the same node.
pub const GPU_IRQ: u32 = 75;

/// The directory name under `/sys/devices/platform`, as Linux names a platform
/// device created from a device tree node: the node name with its unit address.
///
/// The same shape as the DPU's `dpu@c0340000`; see
/// `ABI.md` device identity contract.
pub const GPU_DEVICE_NAME: &str = "imggpu@cac00000";

/// The firmware this GPU needs.
///
/// Binary firmware from `linux-firmware`, redistributable under
/// `LICENSE.powervr`, carrying BVNC 36.29.52.182 with
/// `PVR_FW_FLAGS_OPEN_SOURCE` set. Note this is *not* the DDK's
/// `rgx.fw.36.29.52.182`: same hardware, different and incompatible firmware
/// protocol.
pub const FIRMWARE_NAME: &str = "powervr/rogue_36.29.52.182_v1.fw";

/// The `compatible` strings this driver claims, for sysfs.
///
/// One entry, matching the node: `compatible = "img,rgx"`. Read by
/// [`crate::driver::PvrDriver::platform_identity`], which is what puts them in
/// `/sys/devices/platform/*/uevent` as `OF_COMPATIBLE_0`.
pub const GPU_OF_COMPATIBLE: &[&str] = &[GPU_COMPATIBLE];

/// The K1's half of the power sequence.
///
/// Upstream's equivalent is the `pvr_power_sequence_ops` instance a
/// `pvr_device_data` points at. Every step writes real registers through
/// `ostd`'s CCU except [`power_domain_off`](K1Ops::power_domain_off), which
/// says there why there is nothing for it to write.
pub struct K1Ops;

impl PlatformOps for K1Ops {
    fn power_domain_on(&self) -> PvrResult<()> {
        if ccu::power_domain_on(ccu::PowerDomain::Gpu) {
            Ok(())
        } else {
            // The domain never reported itself on within the vendor's bound.
            // (The other way this returns false, a missing CCU, cannot happen
            // here: `probe` only reaches the power sequence after finding the
            // GPU's device-tree node, and that means a K1.) Every register
            // below would read as garbage, so stopping here names the real
            // fault instead of letting the BVNC check report a nonsense ID.
            ostd::error!("K1X_PMU_GPU_PWR_DOMAIN did not come on");
            Err(PvrError::DeviceLost)
        }
    }

    fn clock_enable(&self) -> PvrResult<()> {
        // What the mux selects is only as live as the tree behind it.
        ccu::enable_gpu_parent_clock();

        if !ccu::ungate(ccu::Periph::Gpu) {
            ostd::error!("CLK_GPU could not be ungated: no CCU on this platform");
            return Err(PvrError::DeviceLost);
        }

        // Committing the mux and divider is deliberately *not* done here; see
        // [`Self::reset_deassert`].
        Ok(())
    }

    fn reset_deassert(&self) -> PvrResult<()> {
        if !ccu::deassert_reset(ccu::Periph::Gpu) {
            ostd::error!("RESET_GPU could not be released: no CCU on this platform");
            return Err(PvrError::DeviceLost);
        }

        // Commit the clock mux/divider after reset release. The frequency-change
        // handshake requires the block to be out of reset; ungating alone does not
        // commit the selected rate. The BSP likewise releases reset before setting it.
        //
        // Default selection preserves mux `pll1_d4_614p4` and divider field 1.
        // The divider is field + 1, so this selects 307.2 MHz. `k1.gpuclk=614` sets
        // divider field 0 before the strobe, selecting 614.4 MHz from the same parent.
        //
        // This path does not change GPU rail voltage. The BSP GPU node has no supply
        // property, `stSetVoltage` is a no-op, and its optional DVFS table uses the same
        // voltage level for these rates. That is not a guarantee of frequency stability.
        program_requested_gpu_core_clock();
        if !ccu::commit_clock(ccu::Periph::Gpu) {
            ostd::error!("CLK_GPU was ungated but the frequency change never completed");
            return Err(PvrError::DeviceLost);
        }
        confirm_requested_gpu_core_clock();
        Ok(())
    }

    fn reset_assert(&self) {
        ccu::assert_reset(ccu::Periph::Gpu);
    }

    fn clock_disable(&self) {
        ccu::gate(ccu::Periph::Gpu);
    }

    fn power_domain_off(&self) {
        // Not implemented, and deliberately so. The BSP's
        // `spacemit_pd_power_off` writes *the same bits in the same order* as
        // its power-on sibling -- drop isolation, clear both sleep bits, wait
        // for the status bit to clear -- and only then disables the domain's
        // regulators. The register half therefore powers nothing down, and the
        // GPU domain declares no supply in `k1-x.dtsi`, so there is no
        // regulator half either. Copying that sequence would spend two delays
        // and a poll to leave the domain exactly as it was.
        //
        // The clock gate and the reset above are real, so a suspended GPU is
        // still clock-gated and held in reset; only the domain's own power
        // stays up.
        ostd::debug!("K1X_PMU_GPU_PWR_DOMAIN: no off sequence to copy; leaving the domain on");
    }

    fn core_clock_hz(&self) -> Option<u32> {
        // Read the rate selected by the GPU clock registers. The device-tree clock
        // references and operating-point table alone do not identify the active rate.
        // Firmware timers and power management require the actual nonzero clock value.
        ccu::gpu_core_clock_hz()
    }
}

/// Writes mux/div for 614.4 MHz when `k1.gpuclk=614`. No-op on the default path.
///
/// Register: `APMU_GPU_CLK_RES_CTRL` @ APMU+`0xcc`.
///
/// | Field | Bits | 614.4 MHz | Reset on Muse Book |
/// |---|---|---|---|
/// | mux | `[20:18]` | 0 = `pll1_d4_614p4` | 0 (same) |
/// | div | `[14:12]` | 0 ⇒ ÷1 | 1 ⇒ ÷2 = 307.2 MHz |
/// | fc | bit 15 | left for `ccu::commit_clock` | — |
/// | gate | bit 4 | untouched | — |
/// | `RESET_GPU` | bit 1 | untouched (already deasserted) | — |
///
/// Sources: BSP `ccu-spacemit-k1x.c` `gpu_clk` / `gpu_parent_names[]`;
/// DDK `spacemit_init.c:348-349` `clk_set_rate(gpu_clk, RGX_ST_CORE_CLOCK_SPEED)`.
fn program_requested_gpu_core_clock() {
    if !crate::knobs::wants_core_614() {
        return;
    }

    let clear = crate::knobs::GPU_CLK_MUX_MASK | crate::knobs::GPU_CLK_DIV_MASK;
    let set = crate::knobs::gpu_clk_614_word();
    if ccu::apmu_rmw(crate::knobs::APMU_GPU_CLK_RES_CTRL, clear, set).is_none() {
        ostd::warn!("[k1-gpu] gpuclk=614: APMU write failed (no CCU); leaving reset mux/div");
    }
}

/// Warns once if `k1.gpuclk=614` did not produce 614.4 MHz after the strobe.
///
/// Silent on the default path, and silent when the readout matches.
fn confirm_requested_gpu_core_clock() {
    use core::sync::atomic::{AtomicBool, Ordering};

    static CONFIRMED: AtomicBool = AtomicBool::new(false);

    if !crate::knobs::wants_core_614() {
        return;
    }
    if CONFIRMED.swap(true, Ordering::Relaxed) {
        return;
    }

    match ccu::gpu_core_clock_hz() {
        Some(crate::knobs::GPU_CLK_614_HZ) => {}
        Some(hz) => ostd::warn!(
            "[k1-gpu] gpuclk=614 requested but readout is {} Hz (wanted {})",
            hz,
            crate::knobs::GPU_CLK_614_HZ
        ),
        None => ostd::warn!("[k1-gpu] gpuclk=614 requested but the CCU could not name the parent"),
    }
}

/// A GPU that [`probe`] found and identified, waiting for a firmware image.
struct ProbedGpu {
    /// The mapped control-register window.
    regs: RegisterWindow,
    /// The BVNC the hardware reported, if the self-check could read one.
    gpu_id: Option<GpuId>,
    /// The interrupt this GPU raises, as named by the device tree.
    interrupt: InterruptSourceInFdt,
}

/// What [`probe`] left for [`attach`].
///
/// `Option` inside the lock because the register window is moved out when the
/// device is created: two [`IoMem`] windows cannot cover the same range, so there
/// is exactly one of these and it is handed over rather than copied.
static PROBED: Once<SpinLock<Option<ProbedGpu>>> = Once::new();

/// The GPU interrupt line.
///
/// Held for the lifetime of the kernel: dropping a [`MappedIrqLine`] unmaps the
/// interrupt from the PLIC and unregisters the callback.
static IRQ: Once<SpinLock<Option<MappedIrqLine>>> = Once::new();

/// The interrupt handler's deferred half.
static BOTTOM_HALF: Once<Arc<Taskless>> = Once::new();

/// Probes the GPU and retains its register window for firmware attachment.
///
/// An absent device-tree node is harmless on machines without this GPU. Other
/// probe failures are logged and also return `Ok(())` so the optional GPU cannot
/// prevent kernel boot. [`attach`] loads the firmware and registers the device;
/// firmware boot occurs on first use through [`crate::power::get`].
pub fn probe() -> Result<(), ComponentInitError> {
    let Some(dt) = DEVICE_TREE.get() else {
        return Ok(());
    };

    // Look the node up by path first: that is the string this driver publishes as
    // `OF_FULLNAME`, and `libdrm` folds two platform devices that share one into
    // a single `drmDevice` with no diagnostic on either side, so a path that does
    // not exist is worth a warning even when the `compatible` match saves us.
    let node = match dt.find_node(GPU_OF_FULLNAME) {
        Some(node) => {
            ostd::info!("found the GPU node at {}", GPU_OF_FULLNAME);
            node
        }
        None => {
            let Some(node) = dt.find_compatible(&[GPU_COMPATIBLE]) else {
                ostd::debug!(
                    "no \"{}\" node in the device tree; this kernel is not running on a K1 with a GPU",
                    GPU_COMPATIBLE
                );
                return Ok(());
            };
            ostd::warn!(
                "the GPU node is not at {}, so the OF_FULLNAME published in sysfs is wrong; \
                 libdrm may fold this device together with another one",
                GPU_OF_FULLNAME
            );
            node
        }
    };

    // The `reg` property is the authority; the constants are the cross-check.
    let region = node.reg().and_then(|mut regions| regions.next());
    let (base, size) = match region {
        Some(region) => (
            region.starting_address as usize,
            region.size.unwrap_or(GPU_REG_SIZE),
        ),
        None => {
            ostd::warn!(
                "the GPU node has no reg property; falling back to {:#x}+{:#x}",
                GPU_REG_BASE,
                GPU_REG_SIZE
            );
            (GPU_REG_BASE, GPU_REG_SIZE)
        }
    };
    if base != GPU_REG_BASE || size != GPU_REG_SIZE {
        ostd::warn!(
            "the device tree says the GPU registers are at {:#x}+{:#x}, not {:#x}+{:#x}; \
             trusting the device tree",
            base,
            size,
            GPU_REG_BASE,
            GPU_REG_SIZE
        );
    }

    // A node without an explicit `interrupt-parent` inherits its parent's, which
    // this does not walk: the GPU node names one, and guessing a phandle would
    // map the handler onto the wrong interrupt controller.
    let interrupt = match (
        node.interrupts().and_then(|mut pins| pins.next()),
        node.property("interrupt-parent")
            .and_then(|parent| parent.as_usize()),
    ) {
        (Some(interrupt), Some(parent)) => InterruptSourceInFdt {
            arguments: [interrupt as u32],
            interrupt_parent: parent as u32,
        },
        _ => {
            ostd::error!("the GPU node names no usable interrupt; not bringing the GPU up");
            return Ok(());
        }
    };
    if interrupt.arguments[0] != GPU_IRQ {
        ostd::warn!(
            "the device tree says the GPU interrupt is {}, not {}; trusting the device tree",
            interrupt.arguments[0],
            GPU_IRQ
        );
    }
    // Component initialization runs after the kernel logger is initialized.
    ostd::info!(
        "[k1-gpu] node ok: regs {:#x}+{:#x}, irq {} on phandle {:#x}",
        base,
        size,
        interrupt.arguments[0],
        interrupt.interrupt_parent
    );

    ostd::info!("[k1-gpu] acquiring the register window");
    let Ok(mmio) = IoMem::acquire(base..base + size) else {
        ostd::error!(
            "could not acquire the GPU register window {:#x}+{:#x}; something else claimed it",
            base,
            size
        );
        return Ok(());
    };
    let regs = RegisterWindow::mmio(mmio);
    ostd::info!("[k1-gpu] register window mapped; running the power sequence");

    power::install(Pm::new(Arc::new(K1Ops)));
    if let Err(err) = power::get_unattached() {
        ostd::error!("could not power the GPU up: {:?}", err);
        return Ok(());
    }

    ostd::info!("[k1-gpu] powered; reading the BVNC");
    let gpu_id = self_check(&regs);

    // Nothing holds the GPU until a client does, so let go of the reference the
    // self-check needed. This also puts the reverse sequence in the boot log,
    // which is the only place its order can be observed on hardware.
    power::put_unattached();

    PROBED.call_once(|| {
        SpinLock::new(Some(ProbedGpu {
            regs,
            gpu_id,
            interrupt,
        }))
    });

    // Retain the probed register window until firmware attachment can consume it.
    ostd::info!(
        "GPU probed; waiting for {} before registering a render node",
        FIRMWARE_NAME
    );
    Ok(())
}

/// Checks register access and the expected BVNC. This does not validate
/// firmware execution or its clock, reset, and address-translation configuration.
fn self_check(regs: &RegisterWindow) -> Option<GpuId> {
    // Keep a marker before the first GPU register read: an inaccessible or
    // unclocked register window may stall the bus instead of returning an error.
    ostd::info!(
        "[k1-gpu] reading ROGUE_CR_CORE_ID at offset {:#x}",
        cr::core_id_pbvnc::OFFSET
    );
    let packed = match regs.read64(cr::core_id_pbvnc::OFFSET) {
        Ok(packed) => packed,
        Err(err) => {
            ostd::error!("could not read RGX_CR_CORE_ID: {:?}", err);
            return None;
        }
    };

    // The raw value goes out before it is decoded: all-zeroes and all-ones each
    // name a different wrong thing, and both are more useful than the decoded
    // BVNC they would produce.
    ostd::info!("[k1-gpu] ROGUE_CR_CORE_ID = {:#018x}", packed);

    let gpu_id = GpuId::from_packed(packed);
    if gpu_id == GpuId::K1_BXE_2_32 {
        ostd::info!(
            "[k1-gpu] BVNC self-check passed: {} — power, clock, reset and the \
             register window are all correct",
            gpu_id
        );
        return Some(gpu_id);
    }

    ostd::error!(
        "BVNC self-check FAILED: RGX_CR_CORE_ID reads {:#018x}, decoding to {}, expected {}",
        packed,
        gpu_id,
        GpuId::K1_BXE_2_32
    );
    match packed {
        0 => ostd::error!(
            "  all-zeroes usually means the GPU is unpowered or its clock is still gated"
        ),
        u64::MAX => ostd::error!(
            "  all-ones usually means the GPU is still in reset, or the register window is wrong"
        ),
        _ => ostd::error!(
            "  a value that is neither all-zeroes nor all-ones means the window reaches \
             registers, so suspect the base address before the power sequence"
        ),
    }
    None
}

/// Attaches a firmware image, installs the IRQ handler, and registers the GPU.
///
/// The image is validated against the hardware BVNC before registration because
/// device queries depend on its feature and quirk masks. The identity read is
/// bracketed by a power reference; firmware boots on first runtime-PM use.
pub fn attach(image: Vec<u8>) -> PvrResult<Arc<PvrDevice>> {
    let probed = PROBED
        .get()
        .and_then(|probed| probed.lock().take())
        .ok_or_else(|| {
            ostd::error!("no probed GPU to attach a firmware image to");
            PvrError::Unsupported
        })?;

    match probed.gpu_id {
        Some(gpu_id) => ostd::info!("attaching firmware to the GPU probed as BVNC {}", gpu_id),
        None => ostd::warn!(
            "the BVNC self-check failed during probe; if the GPU is still unreadable, \
             PvrDevice::probe will reject this image as built for other silicon"
        ),
    }

    power::get_unattached()?;
    let attached = attach_powered(probed.regs, probed.interrupt, image);
    power::put_unattached();
    attached
}

/// The body of [`attach`], with the GPU powered.
fn attach_powered(
    regs: RegisterWindow,
    interrupt: InterruptSourceInFdt,
    image: Vec<u8>,
) -> PvrResult<Arc<PvrDevice>> {
    let firmware = fw::validate_and_init_device_info(&image)?;
    let device = PvrDevice::probe(regs, &firmware)?;

    let core_clock_hz = K1Ops.core_clock_hz().filter(|hz| *hz != 0).ok_or_else(|| {
        ostd::error!("cannot attach firmware without a valid GPU core clock rate");
        PvrError::Unsupported
    })?;
    fw::init(
        &device,
        image,
        fw::FwInitParams {
            core_clock_hz,
            // Upstream passes `regs_resource->start`, the bus address of the
            // register window. The GPU sits behind no IOMMU on the K1, so the
            // physical address is the bus address.
            regs_daddr: GPU_REG_BASE as ostd::mm::Daddr,
            trace_group_mask: fw::DEFAULT_TRACE_GROUP_MASK,
        },
    )?;

    request_irq(device.clone(), interrupt)?;

    crate::driver::register(device.clone()).map_err(|err| {
        ostd::error!("could not register the GPU with aster-drm: {:?}", err);
        PvrError::Busy
    })?;
    ostd::info!("registered the GPU as a DRM render node");

    Ok(device)
}

/// Installs the GPU interrupt handler.
///
/// The handler itself must do almost nothing: read and acknowledge the status
/// register, then hand off to [`crate::sched::ccb::process_fwccb`] on a deferred
/// path. Free-list growth and hang reporting both arrive this way and neither
/// belongs in interrupt context.
pub fn request_irq(device: Arc<PvrDevice>, interrupt: InterruptSourceInFdt) -> PvrResult<()> {
    if IRQ.is_completed() {
        ostd::warn!("the GPU interrupt handler is already installed");
        return Err(PvrError::Busy);
    }

    // The deferred half is created before the line is mapped, so that an
    // interrupt arriving between the two has somewhere to go.
    let deferred = device.clone();
    let bottom_half = Taskless::new(move || bottom_half(&deferred));
    BOTTOM_HALF.call_once(|| bottom_half);

    let line = IrqLine::alloc().map_err(|err| {
        ostd::error!("could not allocate an IRQ line: {:?}", err);
        PvrError::NoMemory
    })?;
    let chip = IRQ_CHIP.get().ok_or_else(|| {
        ostd::error!("no interrupt controller to map the GPU interrupt onto");
        PvrError::Unsupported
    })?;
    let mut mapped = chip.map_fdt_pin_to(interrupt, line).map_err(|err| {
        ostd::error!(
            "could not map GPU interrupt {} onto the interrupt controller: {:?}",
            interrupt.arguments[0],
            err
        );
        PvrError::Unsupported
    })?;

    mapped.on_active(move |_trap| handle_irq(&device));
    ostd::info!(
        "GPU interrupt {} installed on IRQ line {}",
        interrupt.arguments[0],
        mapped.num()
    );
    IRQ.call_once(|| SpinLock::new(Some(mapped)));

    Ok(())
}

/// The interrupt handler's top half: read the status, acknowledge it, defer.
///
/// Three things and no more. Reading the firmware's trace, growing a free list
/// and reporting a hang all arrive through this interrupt and all of them can
/// block on the firmware, which is why they run in [`bottom_half`] instead.
///
/// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_device.c>
/// (`pvr_device_irq_handler`, which calls the processor-specific
/// `check_and_ack_irqs` — for MIPS, the pair of registers
/// [`crate::fw::mips::irq_pending`] and [`crate::fw::mips::irq_clear`] use).
fn handle_irq(device: &PvrDevice) {
    match fw::mips::irq_pending(device) {
        // Nothing pending: either a shared line or a powered-down GPU, whose
        // window reads back all-zeroes. Both mean "not ours", and neither is an
        // error worth a log line in interrupt context.
        Ok(false) => return,
        Ok(true) => {}
        Err(err) => {
            ostd::error!("could not read the GPU interrupt status: {:?}", err);
            return;
        }
    }

    if let Err(err) = fw::mips::irq_clear(device) {
        // An unacknowledged level-triggered interrupt re-fires immediately, so
        // this would otherwise be a live-lock rather than a lost event.
        ostd::error!("could not acknowledge the GPU interrupt: {:?}", err);
        return;
    }

    if let Some(bottom_half) = BOTTOM_HALF.get() {
        bottom_half.schedule();
    }
}

/// The interrupt handler's deferred half.
///
/// Everything that may touch the firmware or take a lock the top half must not.
/// Upstream's equivalent is the threaded IRQ handler,
/// `pvr_device_irq_thread_handler`.
fn bottom_half(device: &PvrDevice) {
    // Upstream brackets a reset in `disable_irq()`, so none of this can run
    // while one is in progress. `MappedIrqLine` cannot mask, so the deferred
    // half stands off instead: the rings this reads are being put back to their
    // initial offsets and the firmware structures are being zeroed, and a
    // late interrupt must not be answered out of either.
    if power::manager().is_some_and(Pm::is_resetting) {
        return;
    }

    // `process_fwccb` only makes sense once the FWCCB exists, and its presence
    // is what says whether the firmware has anything to report. Free-list growth
    // and reconstruction are handled from there, which is why they must not wait
    // on the firmware: see `crate::render::free_list`.
    if device.fwccb.get().is_some() {
        crate::sched::ccb::process_fwccb(device);
    }

    // Retire every live queue after draining firmware notifications, matching
    // upstream's `pvr_device_process_active_queues`. KCCB completion waits poll
    // their return slots, so there is no separate IRQ-side waiter wake here.
    crate::sched::queue::process_all(device);

    // Upstream marks the device busy once per drained event, at the bottom of the
    // same loop (`pvr_device_irq_thread_handler`, `pvr_device.c:222`). A job that
    // retires just above already marks it, from `Pm::put_deferred`; what this adds
    // is every *other* reason the firmware raises an interrupt — an FWCCB
    // notification, a free-list grow, a trace — none of which touches the
    // reference count, and each of which is the GPU saying it is in the middle of
    // something.
    power::mark_last_busy();

    // Sample the hang detector only from the periodic service worker. Its two
    // stalled samples are spaced 500 ms apart; sampling on every interrupt would
    // turn that elapsed-time policy into an interrupt-frequency-dependent reset.
}

/// Tests for the facts a board cannot check for us.
#[cfg(any(test, ktest))]
mod tests {
    #[cfg(ktest)]
    use ostd::prelude::*;

    use super::*;

    /// The GPU's `OF_FULLNAME` must differ from every other DRM device's.
    ///
    /// `drmDevicesEqual` compares platform devices by that string alone and
    /// `drmFoldDuplicatedDevices` then merges the equal ones, so a collision
    /// makes `libdrm` report two devices as one with several nodes — and Mesa
    /// loads a user-space driver by the wrong name. Neither side produces a
    /// diagnostic, which is why this is asserted here rather than discovered on
    /// hardware. The other two strings are duplicated from
    /// `drm/src/drivers/soft.rs` and
    /// `framebuffer/src/arch/riscv/k1_kms.rs` because neither crate can be
    /// depended on from here; the table in `ABI.md` device identity contract is the shared
    /// source.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn the_gpu_is_a_distinct_platform_device() {
        const SOFT_OF_FULLNAME: &str = "/soft/aster-drm";
        const DPU_OF_FULLNAME: &str = "/soc/dpu@c0340000";

        assert_ne!(GPU_OF_FULLNAME, SOFT_OF_FULLNAME);
        assert_ne!(GPU_OF_FULLNAME, DPU_OF_FULLNAME);
        // An absolute device tree path, as `of_device_uevent` emits it.
        assert!(GPU_OF_FULLNAME.starts_with('/'));
    }

    /// The constants and the device tree node they were transcribed from.
    ///
    /// The register base is the one mistake this driver cannot diagnose, so the
    /// three ways it is spelled — the path's unit address, the `reg` base, and
    /// the sysfs directory name Linux would derive — have to agree.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn the_constants_agree_with_the_node_name() {
        assert_eq!(GPU_OF_FULLNAME, "/soc/imggpu@cac00000");
        assert!(GPU_OF_FULLNAME.ends_with(GPU_DEVICE_NAME));
        assert!(GPU_OF_FULLNAME.ends_with(&alloc::format!("@{:x}", GPU_REG_BASE)));
        assert_eq!(GPU_OF_COMPATIBLE.len(), 1);
        assert_eq!(GPU_OF_COMPATIBLE[0], GPU_COMPATIBLE);
        // `reg = <0x0 0xcac00000 0x0 0x80000>`, so the window ends below the
        // next 1 MiB boundary and covers every register `rogue::cr` names
        // (highest allow-listed offset `0xf3b8`).
        assert_eq!(GPU_REG_SIZE, 0x8_0000);
        const { assert!(GPU_REG_SIZE > 0xf3b8) };
        assert_eq!(GPU_IRQ, 75);
    }

    /// The firmware file name this platform asks for must be the one the driver
    /// derives from the BVNC, or the image would be looked up under a name
    /// nothing installs.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn the_firmware_name_matches_the_bvnc() {
        assert_eq!(fw::firmware_name(&GpuId::K1_BXE_2_32), FIRMWARE_NAME);
    }
}
