// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: xHCI host controller: rings, slots, transfers.

//! MMIO xHCI host. Rings and contexts live in one `DmaStream`.

use core::sync::atomic::{Ordering, fence};

use aster_block::bio::SubmittedBio;
use ostd::{
    io::IoMem,
    mm::{
        HasDaddr, HasSize, PAGE_SIZE, VmIo, VmIoOnce,
        dma::{DmaStream, dma_io_fence},
    },
};

use super::{
    completion::{ExpectedTransfer, TransferEvent as XferDone},
    context::{
        EP_BULK_IN, EP_BULK_OUT, EP_CONTROL, EP_INT_IN, EP0_DCI, SPEED_SS, context_bytes, dci_of,
        decode_ep0_mps, ep_context, ep0_max_packet, input_control, slot_context, xhci_ep_interval,
    },
    ownership::{DmaRetention, FailureState},
    regs::*,
    ring::{Consumer, Place, Producer},
    trb::{
        self, COMP_SHORT_PACKET, COMP_SUCCESS, TRB_COMPLETION, TRB_PORT_STATUS, TRB_SIZE,
        TRB_TRANSFER, TRT_IN_DATA, TRT_NO_DATA, TRT_OUT_DATA, cmd_address_device, cmd_configure_ep,
        cmd_disable_slot, cmd_enable_slot, cmd_eval_context, completion_code, cycle, data_trb,
        endpoint_id, link_trb, normal_trb, param, residual, setup_trb, slot_id, status_trb,
        trb_type,
    },
};
use crate::delay::{delay_ms, delay_us};

const NFRAMES: usize = 16;
const PAGE_CMD: usize = 1;
const PAGE_EVT: usize = 2;
const PAGE_SCRATCH0: usize = 3;
const PAGE_INPUT: usize = 4;
const PAGE_OUTPUT: usize = 5;
const MAX_SLOTS: u8 = 4;
const PAGE_EP0: usize = 9;
const PAGE_BULK: usize = 10;
const PAGE_BUF: usize = 11;
const PAGE_SCRATCH_EXTRA: usize = 12;
/// Interrupt IN rings + report bounce (last DMA page; scratch extra uses 12..14).
const PAGE_INT: usize = 15;
const MAX_INT: usize = 2;
const INT_BUF_BASE: usize = 2048;
const INT_BUF_BYTES: usize = 64;
const XFER_Q: usize = 8;
const CMD_TRBS: usize = 256;
const EVT_TRBS: usize = 256;
const XFER_TRBS: usize = 64; // last is Link
const EP0_BYTES: usize = XFER_TRBS * TRB_SIZE; // 1024
const OFF_DCBAA: usize = 0x0000;
const OFF_ERST: usize = 0x0800;
const OFF_SCRATCH_PTR: usize = 0x0810;

const CMD_TIMEOUT_US: u64 = 1_000_000;
const XFER_TIMEOUT_US: u64 = 5_000_000;
const HCRST_TIMEOUT_US: u64 = 1_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Cap,
    Halt,
    Timeout,
    ControllerFailed,
    Protocol,
    Dma,
    NoSlot,
    Stall,
    Port,
}

impl From<Error> for &'static str {
    fn from(e: Error) -> Self {
        match e {
            Error::Cap => "xhci cap",
            Error::Halt => "xhci halt",
            Error::Timeout => "xhci timeout",
            Error::ControllerFailed => "xhci rejected after prior timeout",
            Error::Protocol => "xhci protocol",
            Error::Dma => "xhci dma",
            Error::NoSlot => "xhci no slot",
            Error::Stall => "xhci stall",
            Error::Port => "xhci port",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct PortInfo {
    pub port: u8,
    pub speed: u8,
    pub portsc: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct SlotInfo {
    pub slot: u8,
    pub speed: u8,
    pub port: u8,
    pub route: u32,
    pub ep0_mps: u16,
    pub tt_hub: u8,
    pub tt_port: u8,
}

#[derive(Clone, Copy)]
struct IntIn {
    dci: u8,
    ep_addr: u8,
    idx: u8,
    prod: Producer,
    pending: bool,
    trb_dma: u64,
    retired: bool,
    mps: u16,
}

#[derive(Clone, Copy)]
struct Slot {
    info: SlotInfo,
    ep0: Producer,
    bulk_out: Option<(u8, Producer)>, // dci, prod
    bulk_in: Option<(u8, Producer)>,
    max_dci: u8,
    int_in: [Option<IntIn>; MAX_INT],
}

pub struct Host {
    io: DmaRetention<IoMem>,
    dma: DmaRetention<DmaStream>,
    failure: FailureState<SubmittedBio>,
    ops: usize,
    rt: usize,
    db: usize,
    ir: usize,
    csz64: bool,
    ppc: bool,
    max_ports: u8,
    max_slots: u8,
    cmd: Producer,
    evt: Consumer,
    slots: [Option<Slot>; MAX_SLOTS as usize + 1],
    last_cmd_code: u8,
    last_cmd_slot: u8,
    last_cmd_dma: u64,
    last_xfer_code: u8,
    last_xfer_resid: u32,
    xfer_q: [Option<XferDone>; XFER_Q],
    int_used: [bool; MAX_INT],
    // PAGE_BULK contains one shared IN/OUT ring pair: only one MSC may own it.
    bulk_owner: Option<u8>,
}

impl Host {
    pub(crate) fn check_running(&self) -> Result<(), Error> {
        self.failure.check_running()
    }

    pub(crate) fn retain_bio(&mut self, bio: SubmittedBio) {
        self.failure.retain(bio);
    }

    pub fn start(io: IoMem) -> Result<Self, Error> {
        let cap0: u32 = io.read_once(0).map_err(|_| Error::Cap)?;
        if cap0 == 0 || cap0 == 0xffff_ffff {
            log::debug!("[k1-usb] xhci CAPLENGTH dead ({:#010x})", cap0);
            return Err(Error::Cap);
        }
        let caplength = (cap0 & 0xff) as usize;
        let hciversion = ((cap0 >> 16) & 0xffff) as u16;
        let hcs1: u32 = io.read_once(CAP_HCSPARAMS1).map_err(|_| Error::Cap)?;
        let hcs2: u32 = io.read_once(CAP_HCSPARAMS2).map_err(|_| Error::Cap)?;
        let hcc1: u32 = io.read_once(CAP_HCCPARAMS1).map_err(|_| Error::Cap)?;
        let dboff = (io.read_once::<u32>(CAP_DBOFF).map_err(|_| Error::Cap)? & !0x3) as usize;
        let rtsoff = (io.read_once::<u32>(CAP_RTSOFF).map_err(|_| Error::Cap)? & !0x1f) as usize;
        let max_ports = max_ports(hcs1);
        let hw_slots = max_slots(hcs1);
        let slots = hw_slots.min(MAX_SLOTS);
        let scratch = max_scratchpad(hcs2);
        let csz64 = hcc1 & HCC_CSZ != 0;
        let ppc = hcc1 & HCC_PPC != 0;
        log::debug!(
            "[k1-usb] xhci CAPLENGTH={:#x} HCIVERSION={:#x} slots={}/{} ports={} scratch={} csz64={} ppc={} ac64={}",
            caplength,
            hciversion,
            slots,
            hw_slots,
            max_ports,
            scratch,
            csz64 as u8,
            ppc as u8,
            (hcc1 & HCC_AC64 != 0) as u8
        );
        if caplength < 0x20
            || !caplength.is_multiple_of(4)
            || max_ports == 0
            || slots == 0
            || scratch > 4
            || caplength + portsc_off(max_ports) + 4 > io.size()
            || dboff + (usize::from(slots) + 1) * 4 > io.size()
            || rtsoff + 0x40 > io.size()
        {
            return Err(Error::Cap);
        }

        let dma = DmaStream::alloc_uncached(NFRAMES).map_err(|_| Error::Dma)?;
        // Rings share cache lines with hardware-owned TRBs and contexts. A
        // cached mapping cannot safely clean the whole allocation concurrently.
        if !dma.is_cpu_uncached()
            || dma
                .daddr()
                .checked_add(dma.size())
                .is_none_or(|end| end > (1usize << 32))
            || dma.size() < NFRAMES * PAGE_SIZE
        {
            log::warn!("[k1-usb] xhci DMA not usable daddr={:#x}", dma.daddr());
            return Err(Error::Dma);
        }
        log::info!(
            "[k1-usb] xhci DMA pages={} daddr={:#x} paddr-ok",
            NFRAMES,
            dma.daddr()
        );

        let ops = caplength;
        let pagesize: u32 = io.read_once(ops + OP_PAGESIZE).map_err(|_| Error::Cap)?;
        if pagesize & 1 == 0 {
            log::debug!("[k1-usb] xhci PAGESIZE={:#x} (want 4K bit0)", pagesize);
            return Err(Error::Cap);
        }

        let mut host = Self {
            io: DmaRetention::new(io),
            dma: DmaRetention::new(dma),
            failure: FailureState::new(),
            ops,
            rt: rtsoff,
            db: dboff,
            ir: rtsoff + 0x20,
            csz64,
            ppc,
            max_ports,
            max_slots: slots,
            cmd: Producer::new(CMD_TRBS),
            evt: Consumer::new(EVT_TRBS),
            slots: [None; MAX_SLOTS as usize + 1],
            last_cmd_code: 0,
            last_cmd_slot: 0,
            last_cmd_dma: 0,
            last_xfer_code: 0,
            last_xfer_resid: 0,
            xfer_q: [None; XFER_Q],
            int_used: [false; MAX_INT],
            bulk_owner: None,
        };
        host.reset_and_run(scratch)?;
        Ok(host)
    }

    fn reset_and_run(&mut self, scratch: u32) -> Result<(), Error> {
        // Halt if running (5.4.1).
        let mut cmd = self.op_r(OP_USBCMD);
        if self.op_r(OP_USBSTS) & STS_HCH == 0 {
            cmd &= !CMD_RS;
            self.op_w(OP_USBCMD, cmd);
            if !self.handshake(OP_USBSTS, STS_HCH, STS_HCH, 100_000) {
                self.dump("halt");
                return Err(Error::Halt);
            }
        }
        log::info!(
            "[k1-usb] xhci pre-hcrst USBCMD={:#010x} USBSTS={:#010x}",
            self.op_r(OP_USBCMD),
            self.op_r(OP_USBSTS)
        );
        self.op_w(OP_USBCMD, CMD_HCRST);
        if !self.handshake(OP_USBCMD, CMD_HCRST, 0, HCRST_TIMEOUT_US) {
            self.dump("hcrst");
            return Err(Error::Halt);
        }
        if !self.handshake(OP_USBSTS, STS_CNR, 0, HCRST_TIMEOUT_US) {
            self.dump("cnr");
            return Err(Error::Halt);
        }

        let base = self.dma.daddr();
        // DCBAA zeros; entry 0 = scratchpad array if needed.
        if scratch > 0 {
            for i in 0..scratch as usize {
                let page = if i == 0 {
                    PAGE_SCRATCH0
                } else {
                    PAGE_SCRATCH_EXTRA + i - 1
                };
                let ptr = (base + page * PAGE_SIZE) as u64;
                self.dma_w64(OFF_SCRATCH_PTR + i * 8, ptr);
            }
            self.dma_w64(OFF_DCBAA, (base + OFF_SCRATCH_PTR) as u64);
        }

        // Command ring: Link TRB at last slot.
        let cmd_dma = (base + PAGE_CMD * PAGE_SIZE) as u64;
        self.write_trb(
            PAGE_CMD * PAGE_SIZE + (CMD_TRBS - 1) * TRB_SIZE,
            link_trb(cmd_dma, true, true),
        );
        // Event ring: no Link (ERST single segment).
        // ERST[0]: RS addr + size.
        self.dma_w64(OFF_ERST, (base + PAGE_EVT * PAGE_SIZE) as u64);
        let _ = self.dma.write_val(OFF_ERST + 8, &(EVT_TRBS as u32));
        let _ = self.dma.write_val(OFF_ERST + 12, &0u32);

        self.sync_to_dev();

        // From the first device-visible pointer onward, even a failed Run
        // handshake cannot prove that this allocation is no longer DMA-owned.
        self.dma.publish();
        self.io.publish();
        self.op_w64(OP_DCBAAP, (base + OFF_DCBAA) as u64);
        self.op_w64(OP_CRCR, cmd_dma | CRCR_RCS);
        self.op_w(OP_CONFIG, self.max_slots as u32);
        self.op_w(OP_DNCTRL, 0);

        // ERSTSZ counts segments, not TRBs (xHCI 5.5.2.3.1). The table has
        // one segment; a larger count would make the controller fetch
        // nonexistent entries when the event ring wraps.
        self.ir_w(IR_ERSTSZ, 1);
        self.ir_w64(IR_ERSTBA, (base + OFF_ERST) as u64);
        self.ir_w64(IR_ERDP, (base + PAGE_EVT * PAGE_SIZE) as u64);
        // No IRQ handler is installed: keep both interrupt enables clear.
        self.ir_w(IR_IMAN, 0);
        self.op_w(OP_USBCMD, CMD_RS);
        if !self.handshake(OP_USBSTS, STS_HCH, 0, 100_000) {
            self.dump("run");
            return Err(Error::Halt);
        }

        if self.ppc {
            for p in 1..=self.max_ports {
                let sc = port_neutral(self.portsc(p)) | PORT_PP;
                self.op_w(portsc_off(p), sc);
            }
            delay_ms(20);
        }
        log::info!(
            "[k1-usb] xhci running USBSTS={:#010x} USBCMD={:#010x}",
            self.op_r(OP_USBSTS),
            self.op_r(OP_USBCMD)
        );
        Ok(())
    }

    pub fn max_ports(&self) -> u8 {
        self.max_ports
    }

    pub fn portsc(&self, port: u8) -> u32 {
        self.op_r(portsc_off(port))
    }

    pub fn dump(&self, why: &str) {
        log::warn!(
            "[k1-usb] xhci FAIL[{}] USBCMD={:#010x} USBSTS={:#010x} CRCR={:#x} DCBAAP={:#x} RTSOFF={:#x}",
            why,
            self.op_r(OP_USBCMD),
            self.op_r(OP_USBSTS),
            self.op_r64(OP_CRCR),
            self.op_r64(OP_DCBAAP),
            self.rt
        );
        for p in 1..=self.max_ports {
            log::warn!(
                "[k1-usb] xhci FAIL[{}] PORTSC[{}]={:#010x}",
                why,
                p,
                self.portsc(p)
            );
        }
        let evtoff = PAGE_EVT * PAGE_SIZE + self.evt.index * TRB_SIZE;
        let ev = self.read_trb(evtoff);
        log::debug!(
            "[k1-usb] xhci FAIL[{}] evt[{}] {:#x} {:#x} {:#x} {:#x} (type={} cc={})",
            why,
            self.evt.index,
            ev[0],
            ev[1],
            ev[2],
            ev[3],
            trb_type(ev[3]),
            completion_code(ev[2])
        );
    }

    pub fn reset_port(&mut self, port: u8) -> Result<PortInfo, Error> {
        self.check_running()?;
        if port == 0 || port > self.max_ports {
            return Err(Error::Port);
        }
        let mut sc = self.portsc(port);
        log::info!(
            "[k1-usb] xhci port{} before reset PORTSC={:#010x}",
            port,
            sc
        );
        if sc & PORT_CCS == 0 {
            return Err(Error::Port);
        }
        let pls = (sc & PORT_PLS_MASK) >> 5;
        // xHCI 4.3.1: a USB3 protocol port goes to Enabled (PED=1, PLS=U0)
        // on its own after link training; software must *not* reset it
        // before Address Device: resetting an enabled link disrupts enumeration.
        if port_speed(sc) >= SPEED_SS && sc & PORT_PED != 0 && pls == 0 {
            // Acknowledge the change bits without touching PED/PR.
            self.op_w(portsc_off(port), port_neutral(sc) | (sc & PORT_CHANGE));
            let sc = self.portsc(port);
            log::info!(
                "[k1-usb] xhci port{} SS link already enabled PORTSC={:#010x}; no reset",
                port,
                sc
            );
            return Ok(PortInfo {
                port,
                speed: port_speed(sc),
                portsc: sc,
            });
        }
        // Warm reset only for a SuperSpeed link stuck in an error state
        // (Inactive / Compliance / Disabled); everything else -- USB2 ports
        // and a SS port still training -- gets the ordinary hot reset, like
        // Linux `hub_port_reset(..., warm=false)`.
        let ss_stuck = port_speed(sc) >= SPEED_SS && matches!(pls, 4 | 6 | 10);
        sc = port_neutral(sc);
        if ss_stuck {
            sc |= PORT_WPR;
        } else {
            sc |= PORT_PR;
        }
        self.op_w(portsc_off(port), sc);
        delay_ms(50);
        let want = if ss_stuck { PORT_WRC } else { PORT_PRC };
        let mut ok = false;
        // Linux gives a root-port reset 800 ms in total.
        for _ in 0..75 {
            sc = self.portsc(port);
            if sc & want != 0 || (sc & PORT_PR == 0 && sc & PORT_PED != 0) {
                ok = true;
                break;
            }
            delay_ms(10);
        }
        // Clear change bits (and only those: writing PED=1 back would
        // disable the port we just enabled).
        let now = self.portsc(port);
        self.op_w(portsc_off(port), port_neutral(now) | (now & PORT_CHANGE));
        delay_ms(10);
        sc = self.portsc(port);
        log::info!(
            "[k1-usb] xhci port{} after reset PORTSC={:#010x} speed={} ped={}",
            port,
            sc,
            port_speed(sc),
            (sc & PORT_PED != 0) as u8
        );
        if !ok && sc & PORT_PED == 0 {
            self.dump("port-reset");
            return Err(Error::Port);
        }
        let speed = port_speed(sc);
        if speed == 0 {
            return Err(Error::Port);
        }
        Ok(PortInfo {
            port,
            speed,
            portsc: sc,
        })
    }

    pub fn enable_slot(&mut self) -> Result<u8, Error> {
        self.check_running()?;
        self.issue_cmd(cmd_enable_slot(true))?;
        let slot = self.last_cmd_slot;
        if slot == 0 || slot > self.max_slots {
            log::info!("[k1-usb] xhci Enable Slot gave slot={}", slot);
            return Err(Error::NoSlot);
        }
        log::info!("[k1-usb] xhci Enable Slot → {}", slot);
        Ok(slot)
    }

    pub fn disable_slot(&mut self, slot: u8) -> Result<(), Error> {
        self.check_running()?;
        self.issue_cmd(cmd_disable_slot(slot, true))?;
        // Disable completion confirms the old endpoints are stopped. Events
        // already drained into the software queue must not survive reuse of
        // this slot and its transfer-ring addresses.
        for event in &mut self.xfer_q {
            if event.is_some_and(|event| event.slot == slot) {
                *event = None;
            }
        }
        // Only this successful command establishes that endpoint rings can be
        // reused. A timeout or failed completion leaves their reservation intact.
        self.release_int_eps(slot);
        if self.bulk_owner == Some(slot) {
            self.bulk_owner = None;
        }
        if (slot as usize) < self.slots.len() {
            self.slots[slot as usize] = None;
        }
        Ok(())
    }

    pub fn address_device(
        &mut self,
        slot: u8,
        port: PortInfo,
        route: u32,
    ) -> Result<SlotInfo, Error> {
        self.address_child(slot, port.port, port.speed, route, 0, 0)
    }

    pub fn address_child(
        &mut self,
        slot: u8,
        rh_port: u8,
        speed: u8,
        route: u32,
        tt_hub: u8,
        tt_port: u8,
    ) -> Result<SlotInfo, Error> {
        self.check_running()?;
        let mps = ep0_max_packet(speed);
        let info = SlotInfo {
            slot,
            speed,
            port: rh_port,
            route,
            ep0_mps: mps,
            tt_hub,
            tt_port,
        };
        self.program_input_addr(&info, false)?;
        self.issue_cmd(cmd_address_device(
            (self.dma.daddr() + PAGE_INPUT * PAGE_SIZE) as u64,
            slot,
            false,
            true,
        ))?;
        self.slots[slot as usize] = Some(Slot {
            info,
            ep0: Producer::new(XFER_TRBS),
            bulk_out: None,
            bulk_in: None,
            max_dci: EP0_DCI,
            int_in: [None; MAX_INT],
        });
        log::info!(
            "[k1-usb] xhci Address Device slot={} port={} speed={} mps={}",
            slot,
            rh_port,
            speed,
            mps
        );
        Ok(info)
    }

    pub fn eval_ep0_mps(&mut self, slot: u8, b_mps0: u8) -> Result<(), Error> {
        self.check_running()?;
        let Some(s) = self.slots.get(slot as usize).copied().flatten() else {
            return Err(Error::NoSlot);
        };
        let mps = decode_ep0_mps(s.info.speed, b_mps0);
        if mps == 0 || mps == s.info.ep0_mps {
            return Ok(());
        }
        log::info!(
            "[k1-usb] xhci Evaluate Context slot={} ep0 {} → {}",
            slot,
            s.info.ep0_mps,
            mps
        );
        let mut info = s.info;
        info.ep0_mps = mps;
        self.program_input_addr(&info, true)?;
        self.issue_cmd(cmd_eval_context(
            (self.dma.daddr() + PAGE_INPUT * PAGE_SIZE) as u64,
            slot,
            true,
        ))?;
        if let Some(st) = &mut self.slots[slot as usize] {
            st.info.ep0_mps = mps;
        }
        Ok(())
    }

    pub fn configure_bulk(
        &mut self,
        slot: u8,
        ep_out: u8,
        ep_in: u8,
        mps: u16,
    ) -> Result<(), Error> {
        self.check_running()?;
        let Some(s) = self.slots.get(slot as usize).copied().flatten() else {
            return Err(Error::NoSlot);
        };
        if self.bulk_owner.is_some() {
            log::warn!("[k1-usb] only one MSC is supported by the shared bulk rings");
            return Err(Error::NoSlot);
        }
        // Reserve before the command: a timeout cannot prove the device did
        // not accept these rings. Only a successful Disable Slot releases them.
        self.bulk_owner = Some(slot);
        let dci_out = dci_of(ep_out);
        let dci_in = dci_of(ep_in);
        let entries = dci_out.max(dci_in);
        let stride = context_bytes(self.csz64);
        let in_off = PAGE_INPUT * PAGE_SIZE;
        self.zero(in_off, PAGE_SIZE);
        let add = (1u32) | (1 << dci_out) | (1 << dci_in); // A0 + bulk
        self.write_words(in_off, &input_control(add, 0), stride);
        let sc = slot_context(
            s.info.speed,
            entries,
            s.info.port,
            s.info.route,
            false,
            0,
            s.info.tt_hub,
            s.info.tt_port,
        );
        self.write_words(in_off + stride, &sc, stride);

        let out_ring = PAGE_BULK * PAGE_SIZE;
        let in_ring = PAGE_BULK * PAGE_SIZE + 2048;
        self.init_xfer_ring(out_ring);
        self.init_xfer_ring(in_ring);
        let out_deq = (self.dma.daddr() + out_ring) as u64 | 1;
        let in_deq = (self.dma.daddr() + in_ring) as u64 | 1;
        let ep_o = ep_context(EP_BULK_OUT, mps, out_deq, 0, 0, 512, 3);
        let ep_i = ep_context(EP_BULK_IN, mps, in_deq, 0, 0, 512, 3);
        self.write_words(in_off + stride * (1 + dci_out as usize), &ep_o, stride);
        self.write_words(in_off + stride * (1 + dci_in as usize), &ep_i, stride);
        self.sync_to_dev();
        self.issue_cmd(cmd_configure_ep(
            (self.dma.daddr() + in_off) as u64,
            slot,
            true,
        ))?;
        if let Some(st) = &mut self.slots[slot as usize] {
            st.bulk_out = Some((dci_out, Producer::new(XFER_TRBS)));
            st.bulk_in = Some((dci_in, Producer::new(XFER_TRBS)));
            st.max_dci = st.max_dci.max(entries);
        }
        log::info!(
            "[k1-usb] xhci Configure Endpoint slot={} out={:#x}/dci{} in={:#x}/dci{} mps={}",
            slot,
            ep_out,
            dci_out,
            ep_in,
            dci_in,
            mps
        );
        Ok(())
    }

    /// Add one interrupt IN endpoint (xHCI 4.8.3 / 6.2.3). Ring lives on
    /// [`PAGE_INT`]; up to [`MAX_INT`] HID devices share the page.
    pub fn configure_int_in(
        &mut self,
        slot: u8,
        ep_addr: u8,
        mps: u16,
        b_interval: u8,
    ) -> Result<(), Error> {
        self.check_running()?;
        let Some(s) = self.slots.get(slot as usize).copied().flatten() else {
            return Err(Error::NoSlot);
        };
        if ep_addr & 0x80 == 0 || (ep_addr & 0x0f) == 0 {
            return Err(Error::Protocol);
        }
        let idx = self.int_used.iter().position(|&u| !u).ok_or(Error::Dma)?;
        let dci = dci_of(ep_addr);
        let entries = s.max_dci.max(dci);
        let interval = xhci_ep_interval(s.info.speed, b_interval);
        let mps = mps.clamp(3, 64);
        let stride = context_bytes(self.csz64);
        let in_off = PAGE_INPUT * PAGE_SIZE;
        self.zero(in_off, PAGE_SIZE);
        let add = (1u32) | (1 << dci); // A0 + interrupt IN
        self.write_words(in_off, &input_control(add, 0), stride);
        let sc = slot_context(
            s.info.speed,
            entries,
            s.info.port,
            s.info.route,
            false,
            0,
            s.info.tt_hub,
            s.info.tt_port,
        );
        self.write_words(in_off + stride, &sc, stride);

        let ring_off = self.int_ring_off(idx);
        self.init_xfer_ring(ring_off);
        let deq = (self.dma.daddr() + ring_off) as u64 | 1;
        let ep = ep_context(EP_INT_IN, mps, deq, interval, 0, mps, 3);
        self.write_words(in_off + stride * (1 + dci as usize), &ep, stride);
        self.sync_to_dev();
        self.issue_cmd(cmd_configure_ep(
            (self.dma.daddr() + in_off) as u64,
            slot,
            true,
        ))?;
        self.int_used[idx] = true;
        if let Some(st) = &mut self.slots[slot as usize] {
            st.max_dci = entries;
            if let Some(slot) = st.int_in.iter_mut().find(|e| e.is_none()) {
                *slot = Some(IntIn {
                    dci,
                    ep_addr,
                    idx: idx as u8,
                    prod: Producer::new(XFER_TRBS),
                    pending: false,
                    trb_dma: 0,
                    retired: false,
                    mps,
                });
            }
        }
        log::info!(
            "[k1-usb] xhci Configure Endpoint slot={} int-in={:#x}/dci{} interval={} mps={} ring{}",
            slot,
            ep_addr,
            dci,
            interval,
            mps,
            idx
        );
        Ok(())
    }

    /// Queue one interrupt IN Normal TRB if none is outstanding (xHCI 4.11.2.1).
    pub fn int_in_arm(&mut self, slot: u8, ep_addr: u8) -> Result<(), Error> {
        self.check_running()?;
        let Some(ep) = self.int_ep(slot, ep_addr) else {
            return Err(Error::Protocol);
        };
        if ep.retired {
            return Err(Error::Protocol);
        }
        if ep.pending {
            return Ok(());
        }
        let dci = ep.dci;
        let idx = ep.idx as usize;
        let mut prod = ep.prod;
        let mps = ep.mps as u32;
        let ring_off = self.int_ring_off(idx);
        let buf_off = self.int_buf_off(idx);
        for i in 0..INT_BUF_BYTES {
            let _ = self.dma.write_val(buf_off + i, &0u8);
        }
        let ring_dma = (self.dma.daddr() + ring_off) as u64;
        let trb_off = self.place_trb(
            &mut prod,
            ring_off,
            ring_dma,
            normal_trb((self.dma.daddr() + buf_off) as u64, mps, true),
        )?;
        let trb_dma = (self.dma.daddr() + trb_off) as u64;
        if let Some(ep) = self.int_ep_mut(slot, ep_addr) {
            ep.trb_dma = trb_dma;
            ep.prod = prod;
            ep.pending = true;
        }
        self.sync_to_dev();
        self.doorbell(slot, dci);
        Ok(())
    }

    /// Pump the event ring; `None` if the outstanding interrupt IN is still pending.
    pub fn int_in_poll(
        &mut self,
        slot: u8,
        ep_addr: u8,
        buf: &mut [u8],
    ) -> Option<Result<usize, Error>> {
        if let Err(error) = self.check_running() {
            return Some(Err(error));
        }
        self.pump_events();
        let Some(ep) = self.int_ep(slot, ep_addr) else {
            return Some(Err(Error::Protocol));
        };
        if !ep.pending {
            return None;
        }
        let dci = ep.dci;
        let idx = ep.idx as usize;
        let mps = ep.mps as usize;
        let mut expected = ExpectedTransfer::normal(slot, dci, ep.trb_dma);
        let (code, resid) = loop {
            let event = self.take_xfer(slot, dci)?;
            if let Some(completion) = expected.observe(event) {
                break completion;
            }
        };
        if let Some(ep) = self.int_ep_mut(slot, ep_addr) {
            ep.pending = false;
        }
        self.last_xfer_code = code;
        self.last_xfer_resid = resid;
        if code != COMP_SUCCESS && code != COMP_SHORT_PACKET {
            log::debug!(
                "[k1-usb] xhci int-in slot{} ep={:#x} cc={}",
                slot,
                ep_addr,
                code
            );
            return Some(Err(if code == trb::COMP_STALL {
                Error::Stall
            } else {
                Error::Protocol
            }));
        }
        let Some(n) = mps.checked_sub(resid as usize) else {
            return Some(Err(Error::Protocol));
        };
        let n = n.min(buf.len());
        let buf_off = self.int_buf_off(idx);
        let _ = self.dma.sync_from_device(buf_off..buf_off + INT_BUF_BYTES);
        for (i, b) in buf.iter_mut().enumerate().take(n) {
            *b = self.dma.read_val(buf_off + i).unwrap_or(0);
        }
        Some(Ok(n))
    }

    /// Stop software rearming, but keep the hardware ring reserved until a
    /// successful Disable Slot. No Stop Endpoint command is implemented yet.
    pub fn release_int_in(&mut self, slot: u8, ep_addr: u8) {
        if self.check_running().is_err() {
            return;
        }
        if let Some(ep) = self.int_ep_mut(slot, ep_addr) {
            ep.retired = true;
        }
    }

    pub fn pump(&mut self) {
        self.pump_events();
    }

    /// Control transfer. `data` is copied through the bounce page.
    pub fn ctrl(
        &mut self,
        slot: u8,
        setup: [u8; 8],
        data: Option<&mut [u8]>,
        data_in: bool,
    ) -> Result<usize, Error> {
        self.check_running()?;
        let Some(st) = self.slots.get(slot as usize).copied().flatten() else {
            return Err(Error::NoSlot);
        };
        let buf_off = PAGE_BUF * PAGE_SIZE;
        let data_len = data.as_ref().map(|d| d.len()).unwrap_or(0);
        if data_len > PAGE_SIZE {
            return Err(Error::Dma);
        }
        if let Some(d) = data.as_ref() {
            if !data_in {
                for (i, b) in d.iter().enumerate() {
                    let _ = self.dma.write_val(buf_off + i, b);
                }
            } else {
                for i in 0..data_len {
                    let _ = self.dma.write_val(buf_off + i, &0u8);
                }
            }
        }
        let trt = if data_len == 0 {
            TRT_NO_DATA
        } else if data_in {
            TRT_IN_DATA
        } else {
            TRT_OUT_DATA
        };
        let ring_off = self.ep0_off(slot);
        let ring_dma = (self.dma.daddr() + ring_off) as u64;
        let mut prod = st.ep0;
        let s_off = self.place_trb(&mut prod, ring_off, ring_dma, setup_trb(setup, trt, true))?;
        let d_off = if data_len > 0 {
            Some(self.place_trb(
                &mut prod,
                ring_off,
                ring_dma,
                data_trb(
                    (self.dma.daddr() + buf_off) as u64,
                    data_len as u32,
                    data_in,
                    true,
                ),
            )?)
        } else {
            None
        };
        // Status DIR is IN when there was no data or OUT-data (device ACKs);
        // OUT when there was IN-data.
        let status_in = !data_in || data_len == 0;
        let status_off =
            self.place_trb(&mut prod, ring_off, ring_dma, status_trb(status_in, true))?;
        if let Some(s) = &mut self.slots[slot as usize] {
            s.ep0 = prod;
        }
        self.sync_to_dev();
        self.doorbell(slot, EP0_DCI);
        let base = self.dma.daddr();
        self.wait_xfer(
            ExpectedTransfer::control(
                slot,
                EP0_DCI,
                (base + s_off) as u64,
                d_off.map(|off| (base + off) as u64),
                (base + status_off) as u64,
            ),
            XFER_TIMEOUT_US,
        )?;
        let n = if data_in {
            data_len
                .checked_sub(self.last_xfer_resid as usize)
                .ok_or(Error::Protocol)?
        } else {
            data_len
        };
        if data_in && let Some(d) = data {
            let _ = self.dma.sync_from_device(buf_off..buf_off + data_len);
            for (i, b) in d.iter_mut().enumerate() {
                *b = self.dma.read_val(buf_off + i).unwrap_or(0);
            }
        }
        Ok(n)
    }

    /// Bulk transfer to/from an arbitrary DMA address (bio segment).
    pub fn bulk_daddr(
        &mut self,
        slot: u8,
        dir_in: bool,
        daddr: u64,
        len: u32,
    ) -> Result<usize, Error> {
        self.check_running()?;
        let Some(st) = self.slots.get(slot as usize).copied().flatten() else {
            return Err(Error::NoSlot);
        };
        let (dci, mut prod, ring_off) = if dir_in {
            let (dci, p) = st.bulk_in.ok_or(Error::Protocol)?;
            (dci, p, PAGE_BULK * PAGE_SIZE + 2048)
        } else {
            let (dci, p) = st.bulk_out.ok_or(Error::Protocol)?;
            (dci, p, PAGE_BULK * PAGE_SIZE)
        };
        let ring_dma = (self.dma.daddr() + ring_off) as u64;
        let trb_off =
            self.place_trb(&mut prod, ring_off, ring_dma, normal_trb(daddr, len, true))?;
        if dir_in {
            if let Some(s) = &mut self.slots[slot as usize] {
                s.bulk_in = Some((dci, prod));
            }
        } else if let Some(s) = &mut self.slots[slot as usize] {
            s.bulk_out = Some((dci, prod));
        }
        self.sync_to_dev();
        self.doorbell(slot, dci);
        self.wait_xfer(
            ExpectedTransfer::normal(slot, dci, (self.dma.daddr() + trb_off) as u64),
            XFER_TIMEOUT_US,
        )?;
        (len as usize)
            .checked_sub(self.last_xfer_resid as usize)
            .ok_or(Error::Protocol)
    }

    /// Bulk via the bounce page (CBW/CSW/small SCSI).
    pub fn bulk_buf(&mut self, slot: u8, dir_in: bool, data: &mut [u8]) -> Result<usize, Error> {
        self.check_running()?;
        let off = PAGE_BUF * PAGE_SIZE;
        if data.len() > PAGE_SIZE {
            return Err(Error::Dma);
        }
        if !dir_in {
            for (i, b) in data.iter().enumerate() {
                let _ = self.dma.write_val(off + i, b);
            }
            self.sync_to_dev();
        } else {
            for i in 0..data.len() {
                let _ = self.dma.write_val(off + i, &0u8);
            }
            self.sync_to_dev();
        }
        let n = self.bulk_daddr(
            slot,
            dir_in,
            (self.dma.daddr() + off) as u64,
            data.len() as u32,
        )?;
        if dir_in {
            let _ = self.dma.sync_from_device(off..off + data.len());
            for (i, b) in data.iter_mut().enumerate() {
                *b = self.dma.read_val(off + i).unwrap_or(0);
            }
        }
        Ok(n)
    }

    pub fn slot_info(&self, slot: u8) -> Option<SlotInfo> {
        self.slots
            .get(slot as usize)
            .and_then(|s| s.map(|s| s.info))
    }

    fn program_input_addr(&mut self, info: &SlotInfo, eval_only: bool) -> Result<(), Error> {
        self.check_running()?;
        if info.slot == 0 || info.slot > self.max_slots {
            return Err(Error::NoSlot);
        }
        let stride = context_bytes(self.csz64);
        let off = PAGE_INPUT * PAGE_SIZE;
        self.zero(off, PAGE_SIZE);
        let add = if eval_only { 1 << 1 } else { 0b11 }; // A1 or A0|A1
        self.write_words(off, &input_control(add, 0), stride);
        let sc = slot_context(
            info.speed,
            1,
            info.port,
            info.route,
            false,
            0,
            info.tt_hub,
            info.tt_port,
        );
        self.write_words(off + stride, &sc, stride);
        if eval_only {
            // xHCI 4.6.7: Evaluate Context changes EP0's Max Packet Size,
            // preserving the live endpoint state and dequeue pointer. The
            // output context and transfer ring remain controller-owned.
            let source = self.output_off(info.slot) + stride;
            self.dma
                .sync_from_device(source..source + stride)
                .map_err(|_| Error::Dma)?;
            fence(Ordering::Acquire);
            let mut ep = [0u32; 8];
            for (i, word) in ep.iter_mut().enumerate() {
                *word = self.dma.read_val(source + i * 4).map_err(|_| Error::Dma)?;
            }
            ep[1] = (ep[1] & 0xffff) | (u32::from(info.ep0_mps) << 16);
            self.write_words(off + stride * 2, &ep, stride);
        } else {
            let ep0_off = self.ep0_off(info.slot);
            self.init_xfer_ring(ep0_off);
            let deq = (self.dma.daddr() + ep0_off) as u64 | 1;
            let ep = ep_context(EP_CONTROL, info.ep0_mps, deq, 0, 0, 8, 3);
            self.write_words(off + stride * 2, &ep, stride);
            let out_off = self.output_off(info.slot);
            self.zero(out_off, PAGE_SIZE);
            self.dma_w64(
                OFF_DCBAA + info.slot as usize * 8,
                (self.dma.daddr() + out_off) as u64,
            );
        }
        self.sync_to_dev();
        Ok(())
    }

    fn init_xfer_ring(&mut self, off: usize) {
        self.zero(off, XFER_TRBS * TRB_SIZE);
        let dma = (self.dma.daddr() + off) as u64;
        self.write_trb(off + (XFER_TRBS - 1) * TRB_SIZE, link_trb(dma, true, true));
    }

    fn place_trb(
        &mut self,
        prod: &mut Producer,
        ring_off: usize,
        ring_dma: u64,
        mut words: [u32; 4],
    ) -> Result<usize, Error> {
        self.check_running()?;
        let place = prod.place();
        match place {
            Place::Wrap {
                link_index,
                link_cycle,
                index,
                cycle: cyc,
                ..
            } => {
                self.write_trb(
                    ring_off + link_index * TRB_SIZE,
                    link_trb(ring_dma, link_cycle, true),
                );
                words[3] = trb::set_cycle(words[3] & !1, cyc);
                let off = ring_off + index * TRB_SIZE;
                self.write_trb(off, words);
                prod.apply(place);
                Ok(off)
            }
            Place::Slot {
                index, cycle: cyc, ..
            } => {
                words[3] = trb::set_cycle(words[3] & !1, cyc);
                let off = ring_off + index * TRB_SIZE;
                self.write_trb(off, words);
                prod.apply(place);
                Ok(off)
            }
        }
    }

    fn issue_cmd(&mut self, words: [u32; 4]) -> Result<(), Error> {
        self.check_running()?;
        self.last_cmd_code = 0;
        self.last_cmd_slot = 0;
        let ring_off = PAGE_CMD * PAGE_SIZE;
        let ring_dma = (self.dma.daddr() + ring_off) as u64;
        let mut cmd = self.cmd;
        let off = self.place_trb(&mut cmd, ring_off, ring_dma, words)?;
        self.cmd = cmd;
        self.last_cmd_dma = (self.dma.daddr() + off) as u64;
        self.sync_to_dev();
        self.doorbell(0, 0);
        self.wait_cmd(self.last_cmd_dma)?;
        if self.last_cmd_code != COMP_SUCCESS {
            log::debug!(
                "[k1-usb] xhci cmd cc={} slot={}",
                self.last_cmd_code,
                self.last_cmd_slot
            );
            self.dump("cmd");
            return Err(if self.last_cmd_code == trb::COMP_STALL {
                Error::Stall
            } else {
                Error::Protocol
            });
        }
        Ok(())
    }

    fn wait_cmd(&mut self, trb_dma: u64) -> Result<(), Error> {
        let mut waited = 0u64;
        while waited < CMD_TIMEOUT_US {
            self.pump_events();
            if self.last_cmd_code != 0 && self.last_cmd_dma == trb_dma {
                return Ok(());
            }
            delay_us(20);
            waited += 20;
        }
        self.dump("cmd-timeout");
        self.failure.fail();
        Err(Error::Timeout)
    }

    fn wait_xfer(&mut self, mut expected: ExpectedTransfer, timeout: u64) -> Result<(), Error> {
        self.last_xfer_code = 0;
        self.last_xfer_resid = 0;
        let mut waited = 0u64;
        while waited < timeout {
            self.pump_events();
            while let Some(event) = self.take_xfer(expected.slot, expected.dci) {
                let Some((code, resid)) = expected.observe(event) else {
                    continue;
                };
                self.last_xfer_code = code;
                self.last_xfer_resid = resid;
                if code == COMP_SUCCESS || code == COMP_SHORT_PACKET {
                    return Ok(());
                }
                log::debug!(
                    "[k1-usb] xhci xfer slot{} dci{} cc={}",
                    expected.slot,
                    expected.dci,
                    code
                );
                if code == trb::COMP_STALL {
                    return Err(Error::Stall);
                }
                self.dump("xfer");
                return Err(Error::Protocol);
            }
            delay_us(20);
            waited += 20;
        }
        self.dump("xfer-timeout");
        self.failure.fail();
        Err(Error::Timeout)
    }

    fn push_xfer(&mut self, ev: XferDone) {
        for slot in self.xfer_q.iter_mut() {
            if slot.as_ref().is_some_and(|e| {
                e.slot == ev.slot
                    && e.dci == ev.dci
                    && e.trb_dma == ev.trb_dma
                    && e.event_data == ev.event_data
            }) {
                *slot = Some(ev);
                return;
            }
        }
        for slot in self.xfer_q.iter_mut() {
            if slot.is_none() {
                *slot = Some(ev);
                return;
            }
        }
        self.xfer_q.rotate_left(1);
        self.xfer_q[XFER_Q - 1] = Some(ev);
    }

    fn take_xfer(&mut self, slot: u8, dci: u8) -> Option<XferDone> {
        for q in self.xfer_q.iter_mut() {
            if q.as_ref().is_some_and(|e| e.slot == slot && e.dci == dci) {
                return q.take();
            }
        }
        None
    }

    fn pump_events(&mut self) {
        // K1 currently has no xHCI IRQ handler and enables neither INTE nor
        // IMAN.IE. A failed polling controller must not recycle its event ring.
        if self.check_running().is_err() {
            return;
        }
        // Bound every polling step so unrelated/malformed CQEs cannot prevent
        // the command/transfer deadline from being checked.
        for _ in 0..EVT_TRBS {
            let off = PAGE_EVT * PAGE_SIZE + self.evt.index * TRB_SIZE;
            let _ = self.dma.sync_from_device(off..off + TRB_SIZE);
            let control = self
                .dma
                .read_val::<u32>(off + 12)
                .expect("event TRB in DMA allocation");
            if !self.evt.owns(cycle(control)) {
                break;
            }
            // Hardware publishes the Cycle bit last (xHCI 4.9.4). Acquire
            // ownership before reading the remaining DMA-written payload.
            fence(Ordering::Acquire);
            let mut w = self.read_trb(off);
            w[3] = control;
            let ty = trb_type(control);
            match ty {
                TRB_COMPLETION => {
                    if param(&w) == self.last_cmd_dma && self.last_cmd_code == 0 {
                        self.last_cmd_code = completion_code(w[2]);
                        self.last_cmd_slot = slot_id(w[3]);
                    }
                }
                TRB_TRANSFER => {
                    let ev = XferDone {
                        slot: slot_id(w[3]),
                        dci: endpoint_id(w[3]),
                        trb_dma: param(&w),
                        event_data: w[3] & (1 << 2) != 0,
                        code: completion_code(w[2]),
                        resid: residual(w[2]),
                    };
                    self.last_xfer_code = ev.code;
                    self.last_xfer_resid = ev.resid;
                    self.push_xfer(ev);
                }
                TRB_PORT_STATUS => {
                    let port = ((param(&w) >> 24) & 0xff) as u8;
                    log::info!(
                        "[k1-usb] xhci port-status event port={} PORTSC={:#010x}",
                        port,
                        if port > 0 && port <= self.max_ports {
                            self.portsc(port)
                        } else {
                            0
                        }
                    );
                }
                _ => {
                    log::debug!(
                        "[k1-usb] xhci event type={} cc={}",
                        ty,
                        completion_code(w[2])
                    );
                }
            }
            self.evt.advance();
            // ERDP = next TRB | EHB (5.5.2.3.3)
            let next = (self.dma.daddr() + PAGE_EVT * PAGE_SIZE + self.evt.index * TRB_SIZE) as u64;
            self.ir_w64(IR_ERDP, next | ERDP_EHB);
            // Clear EINT (RW1C).
            let sts = self.op_r(OP_USBSTS);
            if sts & STS_EINT != 0 {
                self.op_w(OP_USBSTS, STS_EINT);
            }
        }
    }

    fn doorbell(&self, slot: u8, target: u8) {
        let off = self.db + slot as usize * 4;
        let val = target as u32;
        dma_io_fence();
        self.io
            .write_once(off, &val)
            .expect("validated xHCI doorbell offset");
        dma_io_fence();
    }

    fn ep0_off(&self, slot: u8) -> usize {
        PAGE_EP0 * PAGE_SIZE + (slot as usize - 1) * EP0_BYTES
    }

    fn int_ring_off(&self, idx: usize) -> usize {
        PAGE_INT * PAGE_SIZE + idx * EP0_BYTES
    }

    fn int_buf_off(&self, idx: usize) -> usize {
        PAGE_INT * PAGE_SIZE + INT_BUF_BASE + idx * INT_BUF_BYTES
    }

    fn int_ep(&self, slot: u8, ep_addr: u8) -> Option<IntIn> {
        self.slots
            .get(slot as usize)?
            .as_ref()?
            .int_in
            .iter()
            .flatten()
            .copied()
            .find(|e| e.ep_addr == ep_addr)
    }

    fn int_ep_mut(&mut self, slot: u8, ep_addr: u8) -> Option<&mut IntIn> {
        self.slots
            .get_mut(slot as usize)?
            .as_mut()?
            .int_in
            .iter_mut()
            .filter_map(|e| e.as_mut())
            .find(|e| e.ep_addr == ep_addr)
    }

    fn release_int_eps(&mut self, slot: u8) {
        let idxs: [Option<u8>; MAX_INT] = self
            .slots
            .get(slot as usize)
            .and_then(|s| s.as_ref())
            .map(|s| s.int_in.map(|e| e.map(|i| i.idx)))
            .unwrap_or([None; MAX_INT]);
        for idx in idxs.into_iter().flatten() {
            if (idx as usize) < MAX_INT {
                self.int_used[idx as usize] = false;
            }
        }
        if let Some(st) = self.slots.get_mut(slot as usize).and_then(|s| s.as_mut()) {
            st.int_in = [None; MAX_INT];
        }
    }

    fn output_off(&self, slot: u8) -> usize {
        (PAGE_OUTPUT + slot as usize - 1) * PAGE_SIZE
    }

    fn op_r(&self, off: usize) -> u32 {
        dma_io_fence();
        self.io
            .read_once(self.ops + off)
            .expect("validated xHCI operational offset")
    }
    fn op_w(&self, off: usize, v: u32) {
        dma_io_fence();
        self.io
            .write_once(self.ops + off, &v)
            .expect("validated xHCI operational offset");
        dma_io_fence();
    }
    fn op_r64(&self, off: usize) -> u64 {
        let lo: u32 = self
            .io
            .read_once(self.ops + off)
            .expect("validated xHCI operational offset");
        let hi: u32 = self
            .io
            .read_once(self.ops + off + 4)
            .expect("validated xHCI operational offset");
        lo as u64 | ((hi as u64) << 32)
    }
    fn op_w64(&self, off: usize, v: u64) {
        self.op_w(off, v as u32);
        self.op_w(off + 4, (v >> 32) as u32);
    }
    fn ir_w(&self, off: usize, v: u32) {
        dma_io_fence();
        self.io
            .write_once(self.ir + off, &v)
            .expect("validated xHCI interrupter offset");
        dma_io_fence();
    }
    fn ir_w64(&self, off: usize, v: u64) {
        self.ir_w(off, v as u32);
        self.ir_w(off + 4, (v >> 32) as u32);
    }

    fn handshake(&self, off: usize, mask: u32, done: u32, timeout_us: u64) -> bool {
        let mut w = 0u64;
        while w < timeout_us {
            if self.op_r(off) & mask == done {
                return true;
            }
            delay_us(20);
            w += 20;
        }
        false
    }

    fn write_trb(&self, off: usize, w: [u32; 4]) {
        for (i, v) in w[..3].iter().enumerate() {
            self.dma
                .write_val(off + i * 4, v)
                .expect("TRB in DMA allocation");
        }
        // Publish ownership only after the payload is visible to the xHC.
        dma_io_fence();
        self.dma
            .write_val(off + 12, &w[3])
            .expect("TRB in DMA allocation");
    }
    fn read_trb(&self, off: usize) -> [u32; 4] {
        let mut w = [0u32; 4];
        for (i, slot) in w.iter_mut().enumerate() {
            *slot = self.dma.read_val(off + i * 4).unwrap_or(0);
        }
        w
    }
    fn dma_w64(&self, off: usize, v: u64) {
        let _ = self.dma.write_val(off, &(v as u32));
        let _ = self.dma.write_val(off + 4, &((v >> 32) as u32));
    }
    fn write_words(&self, off: usize, words: &[u32; 8], stride: usize) {
        for (i, v) in words.iter().enumerate() {
            let _ = self.dma.write_val(off + i * 4, v);
        }
        for i in 8..(stride / 4) {
            let _ = self.dma.write_val(off + i * 4, &0u32);
        }
    }
    fn zero(&self, off: usize, len: usize) {
        for i in 0..(len / 4) {
            let _ = self.dma.write_val(off + i * 4, &0u32);
        }
    }
    fn sync_to_dev(&self) {
        let _ = self.dma.sync_to_device(0..self.dma.size());
        dma_io_fence();
    }
}
