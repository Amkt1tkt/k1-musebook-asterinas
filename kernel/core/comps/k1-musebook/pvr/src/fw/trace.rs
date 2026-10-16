// SPDX-License-Identifier: MIT OR GPL-2.0-only
// K1 MUSE Book: bounded firmware failure trace decoding.

//! The firmware trace buffer.
//!
//! Ports `pvr_fw_trace.c` and `pvr_fw_trace.h` (636 lines).
//!
//! Ordinarily this would be deferred as a debugging luxury. It is not, here:
//! BXE-2-32 is untested on this driver, and when the firmware misbehaves the
//! trace buffer is the only account of what it thought it was doing.
//!
//! Bring this up alongside [`super::startstop`], not after it.
//!
//! # Format
//!
//! Each firmware thread owns a ring of 32-bit words. An entry is
//!
//! ```text
//! word 0     id: 0x7 marker | params(4 bits) | group(4 bits) | message(12 bits)
//! word 1..2  64-bit timestamp, high word first
//! word 3..   `params` argument words
//! ```
//!
//! `trace_pointer` in the control structure is where the firmware will write
//! next, so it is also the oldest entry: the reader starts there and wraps.
//! Words that fail [`abi::log_valid_id`] are parts of the ring the firmware has
//! not reached yet, and are skipped rather than treated as an error.
//!
//! # Format strings come from the generated table
//!
//! Upstream resolves each id against `stid_fmts[]` in `pvr_rogue_fwif_sf.h`
//! (1,648 lines) and hands the format string to `seq_printf`. `pvr-regen` emits
//! the `(id, group, params, fmt)` columns into [`crate::rogue::sf`], sorted for
//! binary search. A small formatter walks those C templates so a dump prints
//! the firmware's message, not just raw words. The raw `GROUP:index` and the
//! argument words stay on the line so a table/firmware disagreement is still
//! greppable against the upstream header.
//!
//! The one entry that matters most, a failed firmware assertion, carries its
//! text in the control structure rather than in the table, and *is* printed in
//! full. Recorded in `ABI.md` compatibility limits.
//!
//! The table also restores upstream's corruption check in
//! `pvr_fw_trace.c:fw_trace_get_next()`: an id with the marker bits that is
//! absent from `stid_fmts[]` means the trace is corrupt from that point, and the
//! walk stops rather than treating it as an ordinary unknown message. The
//! structural checks (marker bits; the entry fitting inside the ring) remain.

use alloc::vec::Vec;

use super::{
    abi,
    heap::{FwCache, FwHeap, FwObject},
};
use crate::{
    error::{PvrError, PvrResult},
    rogue::{
        fwif::{FwAddr, RogueFwifFileInfoBuf, RogueFwifTracebuf, RogueFwifTracebufSpace},
        sf,
    },
};

/// Upstream's `BUILD_BUG_ON` in `pvr_fw_trace_init()`: the control structure has
/// one slot per firmware thread and the driver must allocate exactly that many
/// buffers. A mismatch would leave one thread writing to firmware address zero.
const _: () = assert!(abi::THREAD_MAX == 2);

/// Most parameter words an entry can carry. Four bits of `params`.
pub const MAX_PARAMS: usize = 15;

/// How many entries [`FwTrace::dump`] prints per thread.
///
/// The ring holds up to 12,000 words, which is thousands of entries — more than
/// a serial console is worth spending on. The most recent entries are the ones
/// that describe the failure, so the dump keeps a window of the newest and
/// reports the total it walked.
pub const MAX_LOGGED_ENTRIES: usize = 64;

/// The firmware's shared trace buffers and their control structure.
///
/// Ports upstream `struct pvr_fw_trace`. Upstream exposes these through debugfs,
/// which Asterinas does not have; [`Self::dump`] writes to the kernel log
/// instead, which is also the only channel available on a board being brought up
/// over a serial cable.
pub struct FwTrace {
    /// One ring per firmware thread. Always [`abi::THREAD_MAX`] long.
    buffers: Vec<FwObject>,
    /// The `rogue_fwif_tracebuf` the firmware reads to find the rings.
    ctrl: FwObject,
    /// Which log groups are enabled.
    group_mask: u32,
}

impl FwTrace {
    /// Allocates the rings and their control structure.
    ///
    /// Ports upstream `pvr_fw_trace_init()`. `group_mask` selects which log
    /// groups the firmware emits; zero means tracing off, which is upstream's
    /// default (`init_fw_trace_mask=0`).
    ///
    /// Both allocations are device-uncached (MIPS-side). The control structure
    /// is also CPU-uncached: the firmware writes it and the dump path reads it.
    /// The rings stay CPU-cached — they are 2×48 KiB and dumped occasionally,
    /// not polled.
    ///
    /// Neither is cleared on a hard reset, matching upstream's
    /// `PVR_BO_FW_NO_CLEAR_ON_RESET`: the whole point is that the trace survives
    /// the reset that the failure caused.
    pub fn new(heap: &mut FwHeap, group_mask: u32) -> PvrResult<Self> {
        validate_group_mask(group_mask)?;

        let mut buffers = Vec::with_capacity(abi::THREAD_MAX);
        for _ in 0..abi::THREAD_MAX {
            buffers.push(heap.alloc(abi::TRACE_BUF_SIZE_IN_DWORDS * 4, FwCache::DEVICE_UNCACHED)?);
        }
        let ctrl = heap.alloc(size_of::<RogueFwifTracebuf>(), FwCache::POLLED)?;

        let trace = Self {
            buffers,
            ctrl,
            group_mask,
        };
        trace.init_ctrl()?;
        Ok(trace)
    }

    /// The control structure, so [`super`] can put its firmware address into
    /// `rogue_fwif_sysinit`.
    pub fn ctrl(&self) -> &FwObject {
        &self.ctrl
    }

    /// The per-thread rings, for the map loop and for teardown.
    pub fn buffers(&self) -> &[FwObject] {
        &self.buffers
    }

    /// Which log groups are enabled.
    pub fn group_mask(&self) -> u32 {
        self.group_mask
    }

    /// (Re-)writes the control structure the firmware reads.
    ///
    /// Ports upstream `tracebuf_ctrl_init()`. Also the hard-reset path's
    /// initialiser, which is why it is idempotent and separate from
    /// [`Self::new`].
    pub fn init_ctrl(&self) -> PvrResult<()> {
        let mut ctrl = RogueFwifTracebuf {
            log_type: build_log_type(self.group_mask()),
            __pad0: [0; 4],
            tracebuf: [empty_space(); abi::THREAD_MAX],
            tracebuf_size_in_dwords: abi::TRACE_BUF_SIZE_IN_DWORDS as u32,
            tracebuf_flags: 0,
        };
        for (slot, buffer) in ctrl.tracebuf.iter_mut().zip(self.buffers.iter()) {
            slot.trace_buffer_fw_addr = buffer.fw_addr();
            slot.trace_pointer = 0;
            // Divergence: upstream caches the host's CPU pointer here
            // (`tracebuf_space->trace_buffer = trace_buffer->buf`) purely for its
            // own convenience; the firmware never dereferences it. Publishing a
            // kernel address into firmware-visible memory buys nothing and gives
            // a compromised firmware a KASLR oracle, so it stays zero. Recorded
            // in `ABI.md` compatibility limits.
            slot.trace_buffer = 0;
        }
        self.ctrl.write_val(0, &ctrl)
    }

    /// Reads and logs everything the firmware has written.
    ///
    /// The post-mortem entry point. Returns a summary so that
    /// [`super::BootDiagnosis`] can say whether the firmware got far enough to
    /// write anything, which is the difference between "never started" and
    /// "started and died".
    pub fn dump(&self) -> PvrResult<TraceSummary> {
        let ctrl: RogueFwifTracebuf = self.ctrl.read_val(0)?;
        let mut summary = TraceSummary {
            log_type: ctrl.log_type,
            threads: Vec::with_capacity(abi::THREAD_MAX),
        };

        for (thread, buffer) in self.buffers.iter().enumerate() {
            let space = &ctrl.tracebuf[thread];
            let mut words = alloc::vec![0u32; abi::TRACE_BUF_SIZE_IN_DWORDS];
            // One `sync_from_device` then a bulk read: the firmware may still be
            // writing, so this is a snapshot, exactly as upstream's `memcpy` in
            // `fw_trace_open()` is.
            read_words(buffer, &mut words)?;

            let mut thread_summary = decode(thread, space.trace_pointer, &words);
            thread_summary.assertion = if thread_summary.assertion_failed {
                Some(Assertion::from_buf(&space.assert_buf))
            } else {
                None
            };
            thread_summary.log();
            summary.threads.push(thread_summary);
        }

        Ok(summary)
    }
}

/// A zeroed `rogue_fwif_tracebuf_space`.
///
/// Written out rather than derived because `[u8; 200]` has no `Default`, and
/// spelling the padding explicitly is what `CONVENTIONS.md` layout contract asks for
/// anyway.
fn empty_space() -> RogueFwifTracebufSpace {
    RogueFwifTracebufSpace {
        trace_pointer: 0,
        trace_buffer_fw_addr: FwAddr(0),
        trace_buffer: 0,
        assert_buf: RogueFwifFileInfoBuf {
            path: [0; 200],
            info: [0; 200],
            line_num: 0,
            padding: 0,
        },
    }
}

/// Turns a group mask into a `log_type`.
///
/// Ports upstream `build_log_type()`. An empty mask is `LOG_TYPE_NONE` rather
/// than `LOG_TYPE_TRACE` with no groups, because the firmware treats the latter
/// as "trace everything you were told to" and there is nothing to tell it.
pub fn build_log_type(group_mask: u32) -> u32 {
    if group_mask == 0 {
        abi::LOG_TYPE_NONE
    } else {
        group_mask | abi::LOG_TYPE_TRACE
    }
}

/// Rejects a group mask with bits outside [`abi::LOG_TYPE_GROUP_MASK`].
///
/// Ports upstream `validate_group_mask()`, which warns and returns `-EINVAL`
/// rather than truncating: a bit the driver does not recognise would land in the
/// `log_type` field's control bits.
pub fn validate_group_mask(group_mask: u32) -> PvrResult<()> {
    if group_mask & !abi::LOG_TYPE_GROUP_MASK != 0 {
        ostd::warn!(
            "invalid firmware trace group mask {:#010x} (must be a subset of {:#010x})",
            group_mask,
            abi::LOG_TYPE_GROUP_MASK
        );
        return Err(PvrError::InvalidArgument);
    }
    Ok(())
}

/// Copies a ring out of firmware memory, syncing first.
///
/// One invalidate and one bulk read, not one per word: the firmware may still be
/// writing, so this is a snapshot either way, and 12,000 individual reads on a
/// diagnosis path is a lot of nothing.
fn read_words(buffer: &FwObject, words: &mut [u32]) -> PvrResult<()> {
    buffer.sync_from_device(0..buffer.allocated_size())?;

    let mut bytes = alloc::vec![0u8; words.len() * 4];
    // The sync above covers the whole ring, so go straight at the allocation
    // rather than through `FwObject::read_bytes`, which would sync again.
    ostd::mm::VmIo::read_bytes(buffer.dma(), 0, &mut bytes)
        .map_err(|_| PvrError::InvalidArgument)?;
    for (word, chunk) in words.iter_mut().zip(bytes.as_chunks::<4>().0) {
        *word = u32::from_le_bytes(*chunk);
    }
    Ok(())
}

/// One decoded trace entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TraceEntry {
    /// The raw id word, so it can be matched against `pvr_rogue_fwif_sf.h`.
    pub id: u32,
    /// The firmware's timestamp, with the debug-info bits masked off.
    pub timestamp: u64,
    /// Argument words. Only the first [`Self::param_count`] are meaningful.
    pub params: [u32; MAX_PARAMS],
    /// How many arguments the id declares.
    pub param_count: usize,
}

impl TraceEntry {
    /// Log group name, from the id.
    pub fn group(&self) -> &'static str {
        abi::LOG_GROUP_NAMES[abi::sf_group_id(self.id) as usize % abi::LOG_GROUP_NAMES.len()]
    }

    /// Message number within the group, i.e. what to look up in
    /// `pvr_rogue_fwif_sf.h`.
    pub fn message(&self) -> u32 {
        abi::sf_message_id(self.id)
    }
}

/// A firmware assertion, from the control structure rather than the ring.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Assertion {
    /// Source path the firmware reported, as UTF-8 up to the first NUL.
    pub path: alloc::string::String,
    /// The assertion's text.
    pub info: alloc::string::String,
    /// Line number.
    pub line: u32,
}

impl Assertion {
    /// Decodes the fixed-size NUL-padded strings the firmware writes.
    fn from_buf(buf: &RogueFwifFileInfoBuf) -> Self {
        Self {
            path: nul_terminated(&buf.path),
            info: nul_terminated(&buf.info),
            line: buf.line_num,
        }
    }
}

/// Reads a NUL-padded firmware string, replacing anything that is not UTF-8.
///
/// The firmware writes ASCII, but it is firmware-writable memory: treating it as
/// trusted UTF-8 would make a corrupt trace a panic.
fn nul_terminated(bytes: &[u8]) -> alloc::string::String {
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    bytes[..end]
        .iter()
        .map(|b| {
            if b.is_ascii_graphic() || *b == b' ' {
                *b as char
            } else {
                '?'
            }
        })
        .collect()
}

/// What one thread's ring contained.
#[derive(Clone, Debug)]
pub struct ThreadTrace {
    /// Which firmware thread.
    pub thread: usize,
    /// Where the firmware would write next, as a word index.
    pub write_pointer: u32,
    /// How many valid entries the walk found.
    pub entries: usize,
    /// Whether the walk ended on a failed assertion.
    pub assertion_failed: bool,
    /// The assertion's text, when [`Self::assertion_failed`].
    pub assertion: Option<Assertion>,
    /// Whether the walk stopped because an entry did not make sense.
    pub truncated: bool,
    /// When set, the walk stopped because this id is absent from
    /// [`sf::STID_FMTS`] — upstream's "trace is most likely corrupt from this
    /// point" in `pvr_fw_trace.c:fw_trace_get_next()`.
    pub corrupt_id: Option<u32>,
    /// The newest entries, at most [`MAX_LOGGED_ENTRIES`].
    pub recent: Vec<TraceEntry>,
}

impl ThreadTrace {
    /// Whether the firmware wrote anything at all to this ring.
    ///
    /// The single most useful bit in the whole module: a firmware that never
    /// started cannot have written a trace entry.
    pub fn is_empty(&self) -> bool {
        self.entries == 0
    }

    /// Writes the thread's trace to the kernel log.
    pub fn log(&self) {
        if self.entries == 0 {
            ostd::info!(
                "firmware trace thread {}: empty (write pointer {})",
                self.thread,
                self.write_pointer
            );
            return;
        }

        let stop_reason = if let Some(id) = self.corrupt_id {
            alloc::format!(", trace corrupt from here (id {id:#010x} not in stid_fmts)")
        } else if self.truncated {
            alloc::string::String::from(", walk stopped early (malformed entry)")
        } else {
            alloc::string::String::new()
        };
        ostd::info!(
            "firmware trace thread {}: {} entries, write pointer {}{}{}",
            self.thread,
            self.entries,
            self.write_pointer,
            stop_reason,
            if self.assertion_failed {
                ", ended on an assertion failure"
            } else {
                ""
            }
        );
        if self.entries > self.recent.len() {
            ostd::info!(
                "firmware trace thread {}: showing the last {} of {}",
                self.thread,
                self.recent.len(),
                self.entries
            );
        }
        for entry in &self.recent {
            ostd::info!("{}", format_entry(entry));
        }
        if let Some(assertion) = &self.assertion {
            ostd::error!(
                "firmware assertion failed: {} at {}:{}",
                assertion.info,
                assertion.path,
                assertion.line
            );
        }
    }

    /// Writes a bounded decoded trace to the kernel log.
    ///
    /// Keep the newest entries so failure reporting cannot flood the console with
    /// the full firmware ring. The control structure and ring are snapshots; the
    /// firmware may continue writing while they are read.
    pub fn log_early(&self) {
        if self.entries == 0 {
            ostd::debug!(
                "[k1-gpu]   fw trace thread {}: empty (write pointer {})",
                self.thread,
                self.write_pointer
            );
            return;
        }

        ostd::debug!(
            "[k1-gpu]   fw trace thread {}: {} entries, write pointer {}{}{}{}",
            self.thread,
            self.entries,
            self.write_pointer,
            if let Some(id) = self.corrupt_id {
                alloc::format!(", corrupt from here (id {id:#010x} not in stid_fmts)")
            } else {
                alloc::string::String::new()
            },
            if self.truncated {
                ", walk stopped early (malformed entry)"
            } else {
                ""
            },
            if self.assertion_failed {
                ", ended on an assertion failure"
            } else {
                ""
            }
        );
        if self.entries > self.recent.len() {
            ostd::debug!(
                "[k1-gpu]   fw trace thread {}: showing the last {} of {}",
                self.thread,
                self.recent.len(),
                self.entries
            );
        }
        for entry in &self.recent {
            ostd::debug!("[k1-gpu] {}", format_entry(entry));
        }
        if let Some(assertion) = &self.assertion {
            ostd::debug!(
                "[k1-gpu]   fw assertion failed: {} at {}:{}",
                assertion.info,
                assertion.path,
                assertion.line
            );
        }
    }
}

/// Renders one entry: raw `GROUP:index` and argument words, then the formatted
/// message.
///
/// The raw form is what lets a human cross-check against
/// `pvr_rogue_fwif_sf.h` when the table and the firmware disagree. Dropping it
/// would recreate the throwaway-script problem this formatter exists to end.
fn format_entry(entry: &TraceEntry) -> alloc::string::String {
    use core::fmt::Write as _;

    let mut line = alloc::format!(
        "  [{}] {}:{} (id {:#010x}, {} args)",
        entry.timestamp,
        entry.group(),
        entry.message(),
        entry.id,
        entry.param_count
    );
    for value in &entry.params[..entry.param_count] {
        // Writing into a `String` cannot fail; there is nothing to report.
        let _ = write!(line, " {value:#x}");
    }
    if let Some(fmt) = sf::lookup(entry.id) {
        let _ = write!(
            line,
            ": {}",
            format_printf(fmt.fmt, &entry.params[..entry.param_count])
        );
    }
    line
}

/// Widest pad this formatter will honour.
///
/// A `u32` needs 11 decimal characters with sign, or 8 hex digits. Upstream
/// templates use at most 8. Anything larger is treated as 32 so a wild `%`
/// width cannot grow the `String` without bound.
const MAX_PAD: usize = 32;

/// Substitutes `args` into a C printf template.
///
/// Does not panic. Allocation is O(template + args): upstream caps templates at
/// `PVR_SF_STRING_MAX_SIZE` (256) and an entry carries at most [`MAX_PARAMS`]
/// words. Width and precision are clamped to [`MAX_PAD`].
///
/// `%s` prints `<no-string>` and still consumes one argument word. The ring
/// holds only `u32`s; there is no host pointer to dereference, and a firmware
/// VA would not be a Rust `&str`. Upstream's `seq_printf` in
/// `pvr_fw_trace.c:fw_trace_seq_show()` is in the same position — the header
/// has no `%s` at the pinned revision, and if one appeared the word would be equally useless.
/// `%%` is a literal `%` and does not consume an argument.
fn format_printf(template: &str, args: &[u32]) -> alloc::string::String {
    use core::fmt::Write as _;

    let bytes = template.as_bytes();
    let mut out = alloc::string::String::with_capacity(template.len() + args.len() * 11);
    let mut i = 0usize;
    let mut arg_i = 0usize;
    let mut specs = 0usize;
    let mut needed = 0usize;

    while i < bytes.len() {
        if bytes[i] != b'%' {
            push_utf8_char(&mut out, bytes, &mut i);
            continue;
        }
        i += 1;
        if i < bytes.len() && bytes[i] == b'%' {
            out.push('%');
            i += 1;
            continue;
        }
        let Some(spec) = parse_spec(bytes, &mut i) else {
            out.push('%');
            continue;
        };
        specs += 1;

        let width = match spec.width {
            Field::Star => {
                needed += 1;
                match take_arg(args, &mut arg_i) {
                    Some(w) => Some(clamp_pad(w as usize)),
                    None => {
                        out.push_str("<missing>");
                        continue;
                    }
                }
            }
            Field::Exact(w) => Some(clamp_pad(w)),
            Field::None => None,
        };
        let prec = match spec.prec {
            Field::Star => {
                needed += 1;
                match take_arg(args, &mut arg_i) {
                    Some(p) => Some(clamp_pad(p as usize)),
                    None => {
                        out.push_str("<missing>");
                        continue;
                    }
                }
            }
            Field::Exact(p) => Some(clamp_pad(p)),
            Field::None => None,
        };

        needed += 1;
        let Some(value) = take_arg(args, &mut arg_i) else {
            out.push_str("<missing>");
            continue;
        };

        match spec.conv {
            b'd' | b'i' => {
                let signed = value as i32;
                let negative = signed < 0;
                let digits = alloc::format!("{}", signed.unsigned_abs());
                out.push_str(&pad_signed(
                    &digits,
                    SignedFormat {
                        negative,
                        plus: spec.plus,
                        space: spec.space,
                        width,
                        prec,
                        zero: spec.zero,
                        left: spec.left,
                    },
                ));
            }
            b'u' => out.push_str(&pad_digits(
                &alloc::format!("{value}"),
                "",
                width,
                prec,
                spec.zero,
                spec.left,
            )),
            b'x' | b'X' => {
                let upper = spec.conv == b'X';
                let digits = if upper {
                    alloc::format!("{value:X}")
                } else {
                    alloc::format!("{value:x}")
                };
                let prefix = if spec.alt {
                    if upper { "0X" } else { "0x" }
                } else {
                    ""
                };
                out.push_str(&pad_digits(
                    &digits, prefix, width, prec, spec.zero, spec.left,
                ));
            }
            b'c' => {
                let ch = value as u8;
                if ch.is_ascii_graphic() || ch == b' ' {
                    out.push(ch as char);
                } else {
                    out.push('?');
                }
            }
            b's' => out.push_str("<no-string>"),
            other => {
                let _ = write!(out, "<bad:%{} {value:#x}>", other as char);
            }
        }
    }

    if needed != args.len() {
        if arg_i < args.len() {
            out.push_str(" [unused");
            for value in &args[arg_i..] {
                let _ = write!(out, " {value:#x}");
            }
            out.push(']');
        }
        let _ = write!(out, " [arg mismatch: {specs} specs, {} args]", args.len());
    }
    out
}

#[derive(Clone, Copy)]
enum Field {
    None,
    Star,
    Exact(usize),
}

struct Spec {
    alt: bool,
    zero: bool,
    left: bool,
    plus: bool,
    space: bool,
    width: Field,
    prec: Field,
    conv: u8,
}

fn parse_spec(bytes: &[u8], i: &mut usize) -> Option<Spec> {
    let mut pos = *i;
    if pos >= bytes.len() {
        return None;
    }
    let mut spec = Spec {
        alt: false,
        zero: false,
        left: false,
        plus: false,
        space: false,
        width: Field::None,
        prec: Field::None,
        conv: 0,
    };
    loop {
        match bytes.get(pos).copied() {
            Some(b'#') => spec.alt = true,
            Some(b'0') => spec.zero = true,
            Some(b'-') => spec.left = true,
            Some(b'+') => spec.plus = true,
            Some(b' ') => spec.space = true,
            _ => break,
        }
        pos += 1;
    }
    if bytes.get(pos) == Some(&b'*') {
        spec.width = Field::Star;
        pos += 1;
    } else if bytes.get(pos).is_some_and(|b| b.is_ascii_digit()) {
        spec.width = Field::Exact(parse_uint(bytes, &mut pos));
    }
    if bytes.get(pos) == Some(&b'.') {
        pos += 1;
        if bytes.get(pos) == Some(&b'*') {
            spec.prec = Field::Star;
            pos += 1;
        } else {
            spec.prec = Field::Exact(parse_uint(bytes, &mut pos));
        }
    }
    match bytes.get(pos).copied() {
        Some(b'h') => {
            pos += 1;
            if bytes.get(pos) == Some(&b'h') {
                pos += 1;
            }
        }
        Some(b'l') => {
            pos += 1;
            if bytes.get(pos) == Some(&b'l') {
                pos += 1;
            }
        }
        Some(b'z' | b't' | b'j' | b'L') => pos += 1,
        _ => {}
    }
    let conv = bytes.get(pos).copied()?;
    if !conv.is_ascii_alphabetic() {
        return None;
    }
    pos += 1;
    spec.conv = conv;
    *i = pos;
    Some(spec)
}

fn parse_uint(bytes: &[u8], i: &mut usize) -> usize {
    let mut n = 0usize;
    while let Some(d) = bytes.get(*i).copied().filter(u8::is_ascii_digit) {
        n = n.saturating_mul(10).saturating_add(usize::from(d - b'0'));
        *i += 1;
    }
    n
}

fn take_arg(args: &[u32], arg_i: &mut usize) -> Option<u32> {
    let value = args.get(*arg_i).copied()?;
    *arg_i += 1;
    Some(value)
}

fn clamp_pad(n: usize) -> usize {
    n.min(MAX_PAD)
}

fn push_utf8_char(out: &mut alloc::string::String, bytes: &[u8], i: &mut usize) {
    let rest = &bytes[*i..];
    match core::str::from_utf8(rest) {
        Ok(s) => {
            if let Some(ch) = s.chars().next() {
                out.push(ch);
                *i += ch.len_utf8();
            } else {
                *i = bytes.len();
            }
        }
        Err(err) => {
            let valid = err.valid_up_to();
            if valid > 0 {
                let s = core::str::from_utf8(&rest[..valid]).unwrap();
                let ch = s.chars().next().unwrap();
                out.push(ch);
                *i += ch.len_utf8();
            } else {
                out.push('?');
                *i += 1;
            }
        }
    }
}

/// The sign and the padding flags of one `%d`-family conversion.
///
/// A struct rather than six more parameters: `pad_signed` had eight, which is
/// both over clippy's limit and past the point where a caller can get the order
/// right by looking at it.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct SignedFormat {
    /// The value was negative, so the sign is `-` whatever the flags say.
    pub negative: bool,
    /// `%+d`: print `+` for a non-negative value.
    pub plus: bool,
    /// `% d`: print a space for a non-negative value.
    pub space: bool,
    /// Minimum field width.
    pub width: Option<usize>,
    /// Minimum number of digits.
    pub prec: Option<usize>,
    /// `%0d`: pad with zeroes rather than spaces.
    pub zero: bool,
    /// `%-d`: pad on the right.
    pub left: bool,
}

fn pad_signed(digits: &str, fmt: SignedFormat) -> alloc::string::String {
    let sign = if fmt.negative {
        "-"
    } else if fmt.plus {
        "+"
    } else if fmt.space {
        " "
    } else {
        ""
    };
    pad_digits(digits, sign, fmt.width, fmt.prec, fmt.zero, fmt.left)
}

fn pad_digits(
    digits: &str,
    prefix: &str,
    width: Option<usize>,
    prec: Option<usize>,
    zero: bool,
    left: bool,
) -> alloc::string::String {
    let mut number = alloc::string::String::new();
    if let Some(p) = prec.filter(|p| digits.len() < *p) {
        number.extend(core::iter::repeat_n('0', p - digits.len()));
    }
    number.push_str(digits);
    let mut body = alloc::string::String::from(prefix);
    body.push_str(&number);

    let Some(w) = width else {
        return body;
    };
    if body.len() >= w {
        return body;
    }
    let pad = w - body.len();
    if left {
        body.extend(core::iter::repeat_n(' ', pad));
        return body;
    }
    // C: a precision means the `0` flag is ignored for integer conversions.
    if zero && prec.is_none() {
        let mut out = alloc::string::String::from(prefix);
        out.extend(core::iter::repeat_n('0', pad));
        out.push_str(&number);
        out
    } else {
        let mut out = alloc::string::String::new();
        out.extend(core::iter::repeat_n(' ', pad));
        out.push_str(&body);
        out
    }
}

/// What a whole [`FwTrace::dump`] found.
#[derive(Clone, Debug)]
pub struct TraceSummary {
    /// The `log_type` the control structure held, i.e. whether tracing was even
    /// enabled. An empty trace with `log_type == 0` says nothing about the
    /// firmware; an empty trace with tracing on says a great deal.
    pub log_type: u32,
    /// One entry per firmware thread.
    pub threads: Vec<ThreadTrace>,
}

impl TraceSummary {
    /// Whether tracing was enabled when this snapshot was taken.
    pub fn was_enabled(&self) -> bool {
        self.log_type & abi::LOG_TYPE_TRACE != 0
    }

    /// Whether any thread wrote anything.
    pub fn has_entries(&self) -> bool {
        self.threads.iter().any(|thread| !thread.is_empty())
    }

    /// The first assertion any thread reported.
    pub fn assertion(&self) -> Option<&Assertion> {
        self.threads
            .iter()
            .find_map(|thread| thread.assertion.as_ref())
    }
}

/// Walks one ring.
///
/// Ports upstream `fw_trace_get_first()` / `fw_trace_get_next()` /
/// `fw_trace_seq_show()`, collapsed into one pass because there is no `seq_file`
/// to drive it an entry at a time.
///
/// `write_pointer` is the firmware's next write position and therefore the oldest
/// entry, so the walk starts there and wraps.
pub fn decode(thread: usize, write_pointer: u32, words: &[u32]) -> ThreadTrace {
    let len = words.len();
    let start = if len == 0 {
        0
    } else {
        write_pointer as usize % len
    };
    let read = |index: usize| -> u32 {
        if index >= len {
            0
        } else {
            words[(index + start) % len]
        }
    };

    let mut trace = ThreadTrace {
        thread,
        write_pointer,
        entries: 0,
        assertion_failed: false,
        assertion: None,
        truncated: false,
        corrupt_id: None,
        recent: Vec::new(),
    };

    let mut index = 0usize;
    while index < len {
        let id = read(index);
        if !abi::log_valid_id(id) {
            // Not the start of an entry: a part of the ring the firmware has not
            // written, or padding. Upstream skips these one word at a time too.
            index += 1;
            continue;
        }
        if id == abi::SF_MAIN_ASSERT_FAILED {
            // An assertion terminates the trace: everything after it is stale.
            trace.assertion_failed = true;
            break;
        }
        if id == abi::SF_FIRST {
            index += 1;
            continue;
        }

        // Ports `find_sfid()` + the `ROGUE_FW_SF_LAST` branch of
        // `fw_trace_get_next()` in `pvr_fw_trace.c`: marker bits alone are not
        // enough — the id must be a real row of `stid_fmts[]`. A miss means the
        // buffer is corrupt from here; continuing would invent messages.
        let Some(fmt) = sf::lookup(id) else {
            trace.truncated = true;
            trace.corrupt_id = Some(id);
            break;
        };
        let param_count = fmt.params as usize;
        // Structural check: even a known id is corrupt if its arguments run
        // off the end of the ring snapshot.
        if index + 3 + param_count > len {
            trace.truncated = true;
            break;
        }

        let timestamp = ((read(index + 1) as u64) << 32) | read(index + 2) as u64;
        let timestamp =
            (timestamp & !abi::FWT_TIMESTAMP_TIME_CLRMSK) >> abi::FWT_TIMESTAMP_TIME_SHIFT;

        let mut params = [0u32; MAX_PARAMS];
        for (slot, offset) in params.iter_mut().zip(0..param_count) {
            *slot = read(index + 3 + offset);
        }

        trace.entries += 1;
        if trace.recent.len() == MAX_LOGGED_ENTRIES {
            trace.recent.remove(0);
        }
        trace.recent.push(TraceEntry {
            id,
            timestamp,
            params,
            param_count,
        });

        index += 3 + param_count;
    }

    trace
}

/// The control structure's on-the-wire bytes, for tests.
#[cfg(any(test, ktest))]
pub(crate) fn ctrl_bytes(ctrl: &RogueFwifTracebuf) -> &[u8] {
    ostd_pod::IntoBytes::as_bytes(ctrl)
}

#[cfg(any(test, ktest))]
mod tests {
    #[cfg(ktest)]
    use ostd::prelude::*;

    use super::*;

    /// Builds a ring containing `entries`, starting at word `at`, and returns it
    /// with the write pointer that makes `at` the oldest entry.
    fn ring(len: usize, at: usize, entries: &[(u32, u64, &[u32])]) -> (Vec<u32>, u32) {
        let mut words = alloc::vec![0u32; len];
        let mut index = at;
        for (id, timestamp, params) in entries {
            words[index % len] = *id;
            words[(index + 1) % len] = (*timestamp >> 32) as u32;
            words[(index + 2) % len] = *timestamp as u32;
            for (offset, param) in params.iter().enumerate() {
                words[(index + 3 + offset) % len] = *param;
            }
            index += 3 + params.len();
        }
        (words, at as u32)
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn log_type_is_none_until_a_group_is_selected() {
        assert_eq!(build_log_type(0), abi::LOG_TYPE_NONE);
        assert_eq!(
            build_log_type(abi::LOG_TYPE_GROUP_MAIN),
            abi::LOG_TYPE_GROUP_MAIN | abi::LOG_TYPE_TRACE
        );
        assert_eq!(validate_group_mask(abi::LOG_TYPE_GROUP_MAIN), Ok(()));
        assert_eq!(validate_group_mask(abi::LOG_TYPE_GROUP_MASK), Ok(()));
        // `LOG_TYPE_TRACE` is a control bit, not a group.
        assert_eq!(
            validate_group_mask(abi::LOG_TYPE_TRACE),
            Err(PvrError::InvalidArgument)
        );
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn decodes_entries_and_their_arguments() {
        // MAIN message 1 with six arguments, as `ROGUE_FW_SF_MAIN_KICK3D` is.
        let kick_3d = abi::create_sfid(1, abi::LOG_GROUP_MAIN, 6);
        assert!(
            sf::lookup(kick_3d).is_some(),
            "KICK3D must be in the generated stid_fmts table"
        );
        let args = [0xaaaa_0001, 2, 0xbbbb_0003, 4, 5, 6];
        let (words, pointer) = ring(64, 0, &[(kick_3d, 0x1234_5678, &args)]);

        let trace = decode(0, pointer, &words);
        assert_eq!(trace.entries, 1);
        assert!(!trace.assertion_failed);
        assert!(!trace.truncated);
        assert!(trace.corrupt_id.is_none());

        let entry = trace.recent[0];
        assert_eq!(entry.id, kick_3d);
        assert_eq!(entry.timestamp, 0x1234_5678);
        assert_eq!(entry.param_count, 6);
        assert_eq!(&entry.params[..6], &args);
        assert_eq!(entry.group(), "MAIN");
        assert_eq!(entry.message(), 1);
    }

    /// An id with the marker bits but no row in `stid_fmts[]` is corruption,
    /// not an ordinary unknown message — ports the `ROGUE_FW_SF_LAST` branch of
    /// upstream `fw_trace_get_next()` in `pvr_fw_trace.c`.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn an_id_absent_from_stid_fmts_is_corruption() {
        // MAIN message 15 was never assigned in `pvr_rogue_fwif_sf.h`.
        let wild = abi::create_sfid(15, abi::LOG_GROUP_MAIN, 0);
        assert!(sf::lookup(wild).is_none());
        let (words, pointer) = ring(32, 0, &[(wild, 1, &[])]);

        let trace = decode(0, pointer, &words);
        assert!(trace.truncated);
        assert_eq!(trace.corrupt_id, Some(wild));
        assert_eq!(trace.entries, 0);
    }

    /// The matching happy path: a real table id must not be reported as corrupt.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn a_real_stid_is_not_reported_as_corruption() {
        let id = abi::create_sfid(4, abi::LOG_GROUP_MAIN, 0);
        assert!(sf::lookup(id).is_some());
        let (words, pointer) = ring(32, 0, &[(id, 7, &[])]);

        let trace = decode(0, pointer, &words);
        assert!(!trace.truncated);
        assert!(trace.corrupt_id.is_none());
        assert_eq!(trace.entries, 1);
        assert_eq!(trace.recent[0].message(), 4);
    }

    /// The ring is a ring: an entry that starts near the end wraps to the front,
    /// and the write pointer is where the walk starts.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn walks_from_the_write_pointer_and_wraps() {
        let id = abi::create_sfid(7, abi::LOG_GROUP_MTS, 2);
        let (words, pointer) = ring(16, 14, &[(id, 9, &[0x11, 0x22])]);

        let trace = decode(1, pointer, &words);
        assert_eq!(trace.entries, 1);
        assert_eq!(trace.recent[0].timestamp, 9);
        assert_eq!(&trace.recent[0].params[..2], &[0x11, 0x22]);
        assert_eq!(trace.recent[0].group(), "MTS");
    }

    /// An all-zero ring is what a firmware that never ran leaves behind, and it
    /// must read as "empty" rather than as garbage entries.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn an_untouched_ring_decodes_as_empty() {
        let words = alloc::vec![0u32; 128];
        let trace = decode(0, 0, &words);
        assert!(trace.is_empty());
        assert!(!trace.truncated);
        assert!(!trace.assertion_failed);
    }

    /// An assertion ends the trace. Entries after it are stale by definition, so
    /// they must not be reported as if the firmware had written them since.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn an_assertion_terminates_the_walk() {
        let before = abi::create_sfid(4, abi::LOG_GROUP_MAIN, 0);
        let after = abi::create_sfid(6, abi::LOG_GROUP_MAIN, 0);
        let (words, pointer) = ring(
            64,
            0,
            &[
                (before, 1, &[]),
                (abi::SF_MAIN_ASSERT_FAILED, 2, &[0]),
                (after, 3, &[]),
            ],
        );

        let trace = decode(0, pointer, &words);
        assert_eq!(trace.entries, 1);
        assert!(trace.assertion_failed);
        assert_eq!(trace.recent[0].message(), 4);
    }

    /// An entry whose declared argument count runs off the end of the ring is
    /// corruption, not something to read past. Uses a real `stid_fmts` id so the
    /// failure is the fit check rather than an unknown-id reject.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn an_entry_that_does_not_fit_truncates_the_walk() {
        // KICK3D: six args → nine words; an eight-word ring cannot hold it.
        let id = abi::create_sfid(1, abi::LOG_GROUP_MAIN, 6);
        let mut words = alloc::vec![0u32; 8];
        words[0] = id;
        let trace = decode(0, 0, &words);
        assert!(trace.truncated);
        assert!(trace.corrupt_id.is_none());
        assert_eq!(trace.entries, 0);
    }

    /// Only the newest [`MAX_LOGGED_ENTRIES`] are kept, but the total is still
    /// reported — the count is what distinguishes "died immediately" from "ran
    /// for a while".
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn keeps_the_newest_entries_and_counts_them_all() {
        let id = abi::create_sfid(8, abi::LOG_GROUP_MAIN, 0);
        let total = MAX_LOGGED_ENTRIES + 10;
        let entries: Vec<(u32, u64, &[u32])> =
            (0..total).map(|i| (id, i as u64, &[] as &[u32])).collect();
        let (words, pointer) = ring(4 * total, 0, &entries);

        let trace = decode(0, pointer, &words);
        assert_eq!(trace.entries, total);
        assert_eq!(trace.recent.len(), MAX_LOGGED_ENTRIES);
        // Newest kept, oldest dropped.
        assert_eq!(trace.recent[0].timestamp, 10);
        assert_eq!(
            trace.recent[MAX_LOGGED_ENTRIES - 1].timestamp,
            total as u64 - 1
        );
    }

    /// The top sixteen bits of the timestamp are debug info, not time.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn timestamp_drops_the_debug_info_bits() {
        // Use an id from the generated table so the fixture also checks that its
        // group and parameter count remain consistent with the source header.
        let id = abi::create_sfid(4, abi::LOG_GROUP_MAIN, 0);
        assert!(sf::lookup(id).is_some());
        let (words, pointer) = ring(32, 0, &[(id, 0xdead_0000_0000_0042, &[])]);
        let trace = decode(0, pointer, &words);
        assert_eq!(trace.recent[0].timestamp, 0x42);
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn assertion_text_survives_a_non_ascii_firmware_buffer() {
        let mut buf = RogueFwifFileInfoBuf {
            path: [0; 200],
            info: [0; 200],
            line_num: 1234,
            padding: 0,
        };
        buf.path[..7].copy_from_slice(b"fw/a.c\0");
        buf.info[..5].copy_from_slice(b"x>\x01y\0");
        let assertion = Assertion::from_buf(&buf);
        assert_eq!(assertion.path, "fw/a.c");
        assert_eq!(assertion.info, "x>?y");
        assert_eq!(assertion.line, 1234);
    }

    /// Upstream's `BUILD_BUG_ON`: the control structure's slot count and the
    /// number of rings the driver allocates must agree.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn control_structure_has_one_slot_per_thread() {
        let ctrl = RogueFwifTracebuf {
            log_type: abi::LOG_TYPE_NONE,
            __pad0: [0; 4],
            tracebuf: [empty_space(); abi::THREAD_MAX],
            tracebuf_size_in_dwords: abi::TRACE_BUF_SIZE_IN_DWORDS as u32,
            tracebuf_flags: 0,
        };
        assert_eq!(ctrl.tracebuf.len(), abi::THREAD_MAX);
        assert_eq!(ctrl_bytes(&ctrl).len(), size_of::<RogueFwifTracebuf>());
    }

    /// MAIN:14 is the line that made missing format strings expensive: the
    /// firmware said `cCCB Woff update = ...` and the console printed raw words.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn a_known_stid_formats_to_its_upstream_message() {
        let id = abi::create_sfid(14, abi::LOG_GROUP_MAIN, 3);
        let fmt = sf::lookup(id).expect("MAIN:14 must be in the generated table");
        assert_eq!(fmt.fmt, "cCCB Woff update = %d, DM = %d, FWCtx = 0x%08.8x");

        let mut params = [0u32; MAX_PARAMS];
        params[0] = 0x118;
        params[1] = 0x3;
        params[2] = 0xc004_4000;
        let entry = TraceEntry {
            id,
            timestamp: 34437,
            params,
            param_count: 3,
        };
        let line = format_entry(&entry);
        assert!(
            line.contains("MAIN:14"),
            "raw GROUP:index must remain: {line}"
        );
        assert!(
            line.contains("0x118") && line.contains("0x3") && line.contains("0xc0044000"),
            "raw argument words must remain: {line}"
        );
        assert!(
            line.contains("cCCB Woff update = 280, DM = 3, FWCtx = 0xc0044000"),
            "formatted message missing from: {line}"
        );
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn a_template_with_more_specifiers_than_arguments_is_reported() {
        let text = format_printf("%d %d %d", &[1, 2]);
        assert!(text.contains('1'), "{text}");
        assert!(text.contains('2'), "{text}");
        assert!(text.contains("<missing>"), "{text}");
        assert!(text.contains("arg mismatch"), "{text}");
        assert!(text.contains("3 specs, 2 args"), "{text}");
    }

    /// `%%` is a literal percent. Treating it as a specifier would invent a
    /// missing-argument mismatch on a template that is using no arguments.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn a_percent_percent_is_not_a_specifier() {
        assert_eq!(
            format_printf("100%% done, code %d", &[5]),
            "100% done, code 5"
        );
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn width_and_precision_hex_prints_the_expected_digits() {
        assert_eq!(format_printf("%08.8x", &[0x118]), "00000118");
        assert_eq!(format_printf("%08x", &[0x118]), "00000118");
        assert_eq!(format_printf("0x%08.8x", &[0xc004_4000]), "0xc0044000");
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn leftover_arguments_are_reported_rather_than_dropped() {
        let text = format_printf("%d", &[1, 2]);
        assert!(text.starts_with('1'), "{text}");
        assert!(text.contains("unused 0x2"), "{text}");
        assert!(text.contains("arg mismatch"), "{text}");
    }
}
