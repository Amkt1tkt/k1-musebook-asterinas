#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
# K1 MUSE Book: RVV opcode scanner used by check-no-rvv.sh.
"""Scan riscv64 ELF for vector opcodes, using instruction encodings.

ISA attributes are supplementary evidence: they can be absent, have several
valid spellings, or omit instructions inserted with .option arch, .word or
.insn. A disassembler without RVV support may also print unknown mnemonics.
The encoding scan therefore checks RVV 1.0's 32-bit instruction forms:

* OP-V = 0x57: vector ALU and vsetvl-family instructions.
* LOAD-FP = 0x07 with width in {000,101,110,111}: vector loads.
* STORE-FP = 0x27 with the same widths: vector stores.

Executable segments can also contain ELF headers, symbol and relocation
tables, read-only data and binary-wrapper strings. Select scan ranges using
section metadata when available:

1. Scan SHF_EXECINSTR sections except SHT_NOBITS.
2. Also scan executable PT_LOAD gaps outside sections and ELF tables,
   tagged phdr-gap, so unsectioned instructions are not silently skipped.
3. With no section headers, scan executable segments, tagged phdr-only.
   This fallback can report data bytes as instructions.

Non-SHF_EXECINSTR sections are excluded. Instruction-like data inside a code
section can still cause false positives. The scan trusts linker annotations;
it does not prove that control flow cannot execute data or generated code.
Invalid range tables and unsupported container forms fail instead of skipping.
Missing tools or an incomplete scan must not be treated as a pass.
"""

from __future__ import annotations

import argparse
import os
import re
import stat
import struct
import subprocess
import sys
import tempfile


EM_RISCV = 243
PT_LOAD = 1
PF_X = 1
SHF_EXECINSTR = 0x4
SHT_NOBITS = 8
SHT_RISCV_ATTRIBUTES = 0x70000003

# Vector load/store widths; scalar FP load/store uses the other four values.
VECTOR_FP_WIDTHS = frozenset((0b000, 0b101, 0b110, 0b111))

# Optional objdump belt: dotted vector mnemonics and vsetvl* only.
# A bare `\bv[a-z]+` hits labels, scalar `vor`, and comments.
# Encoding is the authority; this is a second pass when the disassembler knows RVV.
VECTOR_MNEMONIC_RE = re.compile(
    r"\b("
    r"vsetvli|vsetivli|vsetvl|"
    r"vle(8|16|32|64)(?:ff)?\.v|vse(8|16|32|64)\.v|"
    r"vl(ux|ox)ei(8|16|32|64)\.v|vs(ux|ox)ei(8|16|32|64)\.v|"
    r"vlse(8|16|32|64)\.v|vsse(8|16|32|64)\.v|"
    r"vlm\.v|vsm\.v|"
    r"vl([1248])re(8|16|32|64)\.v|vs([1248])r\.v|"
    r"v(add|sub|waddu|wadd|wsubu|wsub|adc|sbc)\.|"
    r"v(mul|mulh|mulhu|mulhsu|div|divu|rem|remu)\.|"
    r"v(and|or|xor|min|minu|max|maxu)\.|"
    r"v(slideup|slidedown|slide1up|slide1down|rgather|rgatherei16|compress)\.|"
    r"v(merge|mv|sext|zext|id|iota)\.|"
    r"vf(add|sub|mul|div|min|max|sgnj|sgnjn|sgnjx|cvt|mv|class|merge)\.|"
    r"vm(seq|sne|sltu|slt|sleu|sle|sgtu|sgt|adc|sbc|and|or|xor|nand|nor|xnor|andnot|ornot)\.|"
    r"vred(sum|and|or|xor|min|minu|max|maxu)\.|"
    r"vfred(usum|osum|min|max)\.|"
    r"vamo[a-z0-9]+\.|"
    r"vfirst\.|vcpop\.|vpopc\.|"
    r"vmsbf\.|vmsif\.|vmsof\."
    r")\b"
)


# ---------------------------------------------------------------------------
# ISA string: prefer a false positive
# ---------------------------------------------------------------------------

_RV_PREFIX = re.compile(r"^rv(32|64|128)", re.IGNORECASE)
_VERSION = re.compile(r"[0-9]+p[0-9]+", re.IGNORECASE)


def isa_declares_vector(arch: str) -> bool:
    """Whether Tag_RISCV_arch / an equivalent ISA string declares vector.

    Recognize single-letter prefix extensions, versioned or unversioned
    Zv* extensions, and vendor vector names such as xtheadvector.
    """
    s = arch.strip().lower()
    if not s:
        return False
    # Vendor extensions often spell "vector" rather than the single letter v.
    if "vector" in s or "xtheadv" in s:
        return True
    parts = re.split(r"[_\s]+", s)
    if not parts:
        return False
    prefix = parts[0]
    m = _RV_PREFIX.match(prefix)
    letters = prefix[m.end() :] if m else prefix
    letters = _VERSION.sub("", letters)
    # Single-letter extensions in the prefix: gcv → has v. Versions stripped
    # so the `p` in `2p1` is not treated as an extension.
    if "v" in letters:
        return True
    for p in parts[1:]:
        if not p:
            continue
        if p.startswith("zve") or p.startswith("zvl") or p.startswith("zv"):
            return True
        core = _VERSION.sub("", p)
        if core == "v" or (core.startswith("v") and len(core) > 1 and core[1].isdigit()):
            return True
    return False


# ---------------------------------------------------------------------------
# Instruction stream
# ---------------------------------------------------------------------------

def insn_length(half: int) -> int:
    """RISC-V instruction length in bytes. Use the spec low bits so C does not desync the stream."""
    if half & 0x3 != 0x3:
        return 2
    if half & 0x1C != 0x1C:
        return 4
    if half & 0x20 == 0:
        return 6
    if half & 0x40 == 0:
        return 8
    return 2


def classify_vector_insn(insn: int) -> str | None:
    opcode = insn & 0x7F
    if opcode == 0x57:
        funct3 = (insn >> 12) & 7
        if funct3 == 0b111:
            return "vsetvl*"
        return "OP-V"
    if opcode in (0x07, 0x27):
        width = (insn >> 12) & 7
        if width in VECTOR_FP_WIDTHS:
            return "VLE" if opcode == 0x07 else "VSE"
    return None


def scan_code_bytes(
    data: bytes, base_off: int, limit: int = 256
) -> tuple[list[tuple[int, int, str]], int]:
    """Walk executable bytes as an insn stream. Returns (first `limit` hits, total).

    Skip 16-bit C insns: RVV 1.0 has no compressed encoding. Cap the stored
    hits so a 50 MB .text does not allocate tens of thousands of tuples;
    the verdict uses the total.
    """
    hits: list[tuple[int, int, str]] = []
    total = 0
    i = 0
    n = len(data)
    while i + 2 <= n:
        half = data[i] | (data[i + 1] << 8)
        length = insn_length(half)
        if length == 4 and i + 4 <= n:
            insn = struct.unpack_from("<I", data, i)[0]
            kind = classify_vector_insn(insn)
            if kind:
                total += 1
                if len(hits) < limit:
                    hits.append((base_off + i, insn, kind))
        i += length if i + length <= n else 2
    return hits, total


# ---------------------------------------------------------------------------
# ELF / ar
# ---------------------------------------------------------------------------

class ToolError(Exception):
    pass


def _u(fmt_le: str, fmt_be: str, le: bool, buf: bytes, off: int):
    fmt = fmt_le if le else fmt_be
    size = struct.calcsize(fmt)
    if off + size > len(buf):
        raise ValueError("truncated ELF")
    return struct.unpack_from(fmt, buf, off)[0]


def _subtract(base: tuple[int, int], holes: list[tuple[int, int]]) -> list[tuple[int, int]]:
    """Subtract `holes` from base=(off,size); return the leftover ranges."""
    b_off, b_size = base
    b_end = b_off + b_size
    pieces: list[tuple[int, int]] = []
    cur = b_off
    for h_off, h_size in sorted(holes):
        h_end = h_off + h_size
        if h_end <= cur or h_off >= b_end:
            continue
        if h_off > cur:
            pieces.append((cur, h_off - cur))
        cur = max(cur, h_end)
        if cur >= b_end:
            break
    if cur < b_end:
        pieces.append((cur, b_end - cur))
    return pieces


class Section:
    __slots__ = ("name", "name_off", "type", "flags", "off", "size")

    def __init__(self, name_off: int, type_: int, flags: int, off: int, size: int):
        self.name = ""
        self.name_off = name_off
        self.type = type_
        self.flags = flags
        self.off = off
        self.size = size


def _parse_sections(
    buf: bytes,
    e_shoff: int,
    e_shnum: int,
    e_shentsize: int,
    e_shstrndx: int,
    is64: bool,
    le: bool,
) -> tuple[list[Section], bytes]:
    secs: list[Section] = []
    if not (e_shoff and e_shnum and e_shentsize):
        return secs, b""
    for i in range(e_shnum):
        o = e_shoff + i * e_shentsize
        if o + e_shentsize > len(buf):
            break
        try:
            if is64:
                sh_name = _u("<I", ">I", le, buf, o)
                sh_type = _u("<I", ">I", le, buf, o + 4)
                sh_flags = _u("<Q", ">Q", le, buf, o + 8)
                sh_offset = _u("<Q", ">Q", le, buf, o + 24)
                sh_size = _u("<Q", ">Q", le, buf, o + 32)
            else:
                sh_name = _u("<I", ">I", le, buf, o)
                sh_type = _u("<I", ">I", le, buf, o + 4)
                sh_flags = _u("<I", ">I", le, buf, o + 8)
                sh_offset = _u("<I", ">I", le, buf, o + 16)
                sh_size = _u("<I", ">I", le, buf, o + 20)
        except (ValueError, struct.error):
            continue
        secs.append(Section(sh_name, sh_type, sh_flags, sh_offset, sh_size))

    shstr = b""
    if 0 <= e_shstrndx < len(secs):
        s = secs[e_shstrndx]
        if s.type != SHT_NOBITS:
            shstr = buf[s.off : s.off + s.size]
    for s in secs:
        if shstr and s.name_off < len(shstr):
            s.name, _ = _read_ntbs(shstr, s.name_off)
    return secs, shstr


def parse_elf_ranges(
    buf: bytes,
) -> tuple[list[tuple[int, bytes, str]], list[str], str | None]:
    """Return (executable slices, ISA strings, error). Slice[2] is the source tag.

    Non-RISC-V returns ([], [], "not-riscv"); the caller skips.
    """
    if len(buf) < 16 or buf[:4] != b"\x7fELF":
        return [], [], "not ELF"
    ei_class = buf[4]
    ei_data = buf[5]
    if ei_class not in (1, 2) or ei_data not in (1, 2):
        return [], [], "bad ELF ident"
    le = ei_data == 1
    is64 = ei_class == 2

    if is64:
        if len(buf) < 64:
            return [], [], "truncated ELF64 header"
        e_machine = _u("<H", ">H", le, buf, 18)
        e_phoff = _u("<Q", ">Q", le, buf, 32)
        e_shoff = _u("<Q", ">Q", le, buf, 40)
        e_ehsize = _u("<H", ">H", le, buf, 52)
        e_phentsize = _u("<H", ">H", le, buf, 54)
        e_phnum = _u("<H", ">H", le, buf, 56)
        e_shentsize = _u("<H", ">H", le, buf, 58)
        e_shnum = _u("<H", ">H", le, buf, 60)
        e_shstrndx = _u("<H", ">H", le, buf, 62)
    else:
        if len(buf) < 52:
            return [], [], "truncated ELF32 header"
        e_machine = _u("<H", ">H", le, buf, 18)
        e_phoff = _u("<I", ">I", le, buf, 28)
        e_shoff = _u("<I", ">I", le, buf, 32)
        e_ehsize = _u("<H", ">H", le, buf, 40)
        e_phentsize = _u("<H", ">H", le, buf, 42)
        e_phnum = _u("<H", ">H", le, buf, 44)
        e_shentsize = _u("<H", ">H", le, buf, 46)
        e_shnum = _u("<H", ">H", le, buf, 48)
        e_shstrndx = _u("<H", ">H", le, buf, 50)

    if e_machine != EM_RISCV:
        return [], [], "not-riscv"
    if not le:
        return [], [], "big-endian RISC-V ELF is unsupported"

    n = len(buf)
    header_size = 64 if is64 else 52
    if e_ehsize != header_size:
        return [], [], "invalid ELF header size"
    # Extended counts require section-zero decoding, which this guard does not
    # implement. Never reinterpret them as a missing/empty table.
    if e_phnum == 0xFFFF or e_shstrndx == 0xFFFF or (e_shoff and not e_shnum):
        return [], [], "extended ELF table numbering is unsupported"
    for name, off, count, stride, minimum in (
        ("program", e_phoff, e_phnum, e_phentsize, 56 if is64 else 32),
        ("section", e_shoff, e_shnum, e_shentsize, 64 if is64 else 40),
    ):
        if count and (off < header_size or stride < minimum or off + count * stride > n):
            return [], [], f"invalid/truncated ELF {name} table"
        if off and not count:
            return [], [], f"ELF {name} table offset without entries"
    if e_shstrndx and e_shstrndx >= e_shnum:
        return [], [], "invalid ELF section-name table index"

    def clamp(off: int, size: int) -> tuple[int, int] | None:
        if size <= 0 or off < 0 or off >= n:
            return None
        return (off, min(size, n - off))

    # Executable PT_LOAD. This is an upper bound, not the insn stream:
    # headers / .dynstr / .rela / .rodata often share the segment. Scan
    # the whole segment only when there are no section headers.
    exec_segs: list[tuple[int, int]] = []
    programs: list[tuple[int, int, int, int]] = []
    if e_phoff and e_phnum and e_phentsize:
        for i in range(e_phnum):
            o = e_phoff + i * e_phentsize
            if o + e_phentsize > n:
                break
            try:
                if is64:
                    p_type = _u("<I", ">I", le, buf, o)
                    p_flags = _u("<I", ">I", le, buf, o + 4)
                    p_offset = _u("<Q", ">Q", le, buf, o + 8)
                    p_filesz = _u("<Q", ">Q", le, buf, o + 32)
                else:
                    p_type = _u("<I", ">I", le, buf, o)
                    p_offset = _u("<I", ">I", le, buf, o + 4)
                    p_filesz = _u("<I", ">I", le, buf, o + 16)
                    p_flags = _u("<I", ">I", le, buf, o + 24)
            except (ValueError, struct.error):
                continue
            programs.append((p_type, p_flags, p_offset, p_filesz))
            if p_type == PT_LOAD and p_filesz and (p_offset > n or p_filesz > n - p_offset):
                return [], [], "truncated ELF load segment"
            if p_type == PT_LOAD and (p_flags & PF_X):
                r = clamp(p_offset, p_filesz)
                if r:
                    exec_segs.append(r)

    secs, _shstr = _parse_sections(buf, e_shoff, e_shnum, e_shentsize, e_shstrndx, is64, le)
    for sec in secs:
        if sec.type != SHT_NOBITS and sec.size and (sec.off > n or sec.size > n - sec.off):
            return [], [], "truncated ELF section"

    ranges: list[tuple[int, int, str]] = []  # (off, size, where)
    if secs:
        for i, s in enumerate(secs):
            if s.type == SHT_NOBITS or not (s.flags & SHF_EXECINSTR):
                continue
            r = clamp(s.off, s.size)
            if r:
                ranges.append((r[0], r[1], s.name or f"sh[{i}]"))

        # Bytes in an executable segment claimed by no section and no ELF
        # table. Linker output is padding there; real insns mean a
        # hand-rolled segment and must be seen.
        holes: list[tuple[int, int]] = []
        for s in secs:
            if s.type == SHT_NOBITS:
                continue
            c = clamp(s.off, s.size)
            if c:
                holes.append(c)
        for off, size in (
            (0, e_ehsize or (64 if is64 else 52)),
            (e_phoff, e_phnum * e_phentsize),
            (e_shoff, e_shnum * e_shentsize),
        ):
            c = clamp(off, size)
            if c:
                holes.append(c)
        for seg in exec_segs:
            for off, size in _subtract(seg, holes):
                ranges.append((off, size, f"phdr-gap@0x{off:x}"))
    else:
        for off, size in exec_segs:
            ranges.append((off, size, "phdr-only"))

    # Data-only ET_REL and non-executable DSOs (such as ICU's data library)
    # are legitimate. Missing code in other runnable objects is not a pass.
    elf_type = _u("<H", ">H", le, buf, 16)
    if elf_type in (2, 3) and not ranges:
        entry = _u("<Q" if is64 else "<I", ">Q" if is64 else ">I", le, buf, 24)
        data_only = (elf_type == 3 and entry == 0
                     and not any(s.flags & SHF_EXECINSTR for s in secs)
                     and is_data_only_dso(buf, programs, is64))
        if not data_only:
            return [], [], "executable/shared ELF has no scannable executable bytes"
    ranges.sort()
    slices = [(off, buf[off : off + size], where) for off, size, where in ranges]
    archs = parse_riscv_attributes(buf, secs)
    return slices, archs, None


def is_data_only_dso(
    buf: bytes, programs: list[tuple[int, int, int, int]], is64: bool
) -> bool:
    """Recognize a mapped data DSO, without accepting a damaged executable."""
    loads = [p for p in programs if p[0] == PT_LOAD]
    if not any(p[3] for p in loads) or any(p[1] & PF_X for p in loads):
        return False
    if any(p[0] == 3 for p in programs):  # PT_INTERP
        return False
    dynamic = [p for p in programs if p[0] == 2]  # PT_DYNAMIC
    if len(dynamic) != 1:
        return False
    _, _, off, size = dynamic[0]
    stride = 16 if is64 else 8
    if not size or size % stride or off > len(buf) or size > len(buf) - off:
        return False
    if not any(p[2] <= off and off + size <= p[2] + p[3] for p in loads):
        return False
    for pos in range(off, off + size, stride):
        tag, value = struct.unpack_from("<QQ" if is64 else "<II", buf, pos)
        if tag == 0:  # DT_NULL
            return True
        # INIT/FINI, their arrays and PREINIT_ARRAY (including array sizes).
        if tag in (12, 13, 25, 26, 27, 28, 32, 33) and value:
            return False
        if tag == 0x6FFFFFFB and value & 0x08000000:  # DT_FLAGS_1: DF_1_PIE
            return False
    return False  # An unterminated dynamic table is not a valid data DSO.


def _read_ntbs(buf: bytes, off: int) -> tuple[str, int]:
    end = buf.find(b"\0", off)
    if end < 0:
        return buf[off:].decode("ascii", "replace"), len(buf)
    return buf[off:end].decode("ascii", "replace"), end + 1


def parse_riscv_attributes(buf: bytes, secs: list[Section]) -> list[str]:
    """Extract Tag_RISCV_arch from .riscv.attributes / SHT_RISCV_ATTRIBUTES.

    Parse failure returns []. That is not "no vector" — the encoding scan remains.
    """
    archs: list[str] = []
    for s in secs:
        if s.type != SHT_RISCV_ATTRIBUTES and s.name != ".riscv.attributes":
            continue
        archs.extend(extract_arch_tags(buf[s.off : s.off + s.size]))
    return archs


def extract_arch_tags(blob: bytes) -> list[str]:
    if not blob or blob[0:1] != b"A":
        return []
    archs: list[str] = []
    i = 1
    while i + 4 <= len(blob):
        sub_len = struct.unpack_from("<I", blob, i)[0]
        if sub_len < 5 or i + sub_len > len(blob):
            break
        chunk = blob[i : i + sub_len]
        vendor, j = _read_ntbs(chunk, 4)
        if vendor != "riscv":
            i += sub_len
            continue
        # tag=1 File / 2 Section / 3 Symbol, then a u32 size.
        while j + 5 <= len(chunk):
            tag = chunk[j]
            if tag in (1, 2, 3) and j + 5 <= len(chunk):
                sz = struct.unpack_from("<I", chunk, j + 1)[0]
                body = chunk[j + 5 : j + sz] if sz >= 5 else b""
                archs.extend(_attrs_from_body(body))
                j += max(sz, 1)
            else:
                break
        i += sub_len
    return archs


def _uleb128(buf: bytes, off: int) -> tuple[int, int]:
    shift = 0
    val = 0
    while off < len(buf):
        b = buf[off]
        off += 1
        val |= (b & 0x7F) << shift
        if b & 0x80 == 0:
            return val, off
        shift += 7
    return val, off


def _attrs_from_body(body: bytes) -> list[str]:
    """RISC-V attributes do not follow ARM's odd-uleb / even-string rule. Tag 5 is NTBS."""
    archs: list[str] = []
    i = 0
    while i < len(body):
        tag, i = _uleb128(body, i)
        if tag == 5:
            s, i = _read_ntbs(body, i)
            if s:
                archs.append(s)
        elif tag in (4, 6, 8, 10, 11, 14, 16):
            _, i = _uleb128(body, i)
        else:
            # Unknown tag: guess NTBS. A wrong guess drops a few attrs; encoding scan covers.
            if i < len(body) and body[i] >= 32:
                _, i = _read_ntbs(body, i)
            else:
                _, i = _uleb128(body, i)
    return archs


def parse_ar_members(buf: bytes) -> list[tuple[str, bytes]]:
    if not buf.startswith(b"!<arch>\n"):
        return []
    members: list[tuple[str, bytes]] = []
    i = 8
    strtab = b""
    while i < len(buf):
        if i + 60 > len(buf):
            raise ToolError("truncated archive member header")
        hdr = buf[i : i + 60]
        if hdr[58:60] != b"`\n":
            raise ToolError("invalid archive member header")
        name = hdr[0:16].decode("ascii", "replace").strip()
        try:
            size = int(hdr[48:58].decode("ascii", "replace").strip() or "0")
        except ValueError as e:
            raise ToolError("invalid archive member size") from e
        i += 60
        if size < 0 or size > len(buf) - i or i + size + (size & 1) > len(buf):
            raise ToolError("negative or truncated archive member size")
        data = buf[i : i + size]
        i += size + (size & 1)
        if name == "//":
            strtab = data
            continue
        if name in ("/", "/SYM64/"):
            continue
        if name.startswith("#1/"):
            try:
                nlen = int(name[3:])
            except ValueError as e:
                raise ToolError("invalid BSD archive member name size") from e
            if nlen < 0 or nlen > len(data):
                raise ToolError("truncated BSD archive member name")
            name = data[:nlen].split(b"\0", 1)[0].decode("ascii", "replace")
            data = data[nlen:]
        elif name.startswith("/") and name[1:].isdigit() and strtab:
            off = int(name[1:])
            name, _ = _read_ntbs(strtab, off)
        if data[:4] == b"\x7fELF":
            members.append((name, data))
    return members


# ---------------------------------------------------------------------------
# Single file
# ---------------------------------------------------------------------------

class FileResult:
    __slots__ = (
        "path",
        "kind",
        "insn_hits",
        "insn_total",
        "where_counts",
        "attr_hits",
        "archs",
        "mnemonic_hits",
        "error",
        "scanned",
    )

    def __init__(self, path: str):
        self.path = path
        self.kind = ""  # elf / ar / skip / error
        # (label, file-offset, insn, kind, where)
        self.insn_hits: list[tuple[str, int, int, str, str]] = []
        self.insn_total = 0
        self.where_counts: dict[str, int] = {}
        self.attr_hits: list[tuple[str, str]] = []
        self.archs: list[str] = []
        self.mnemonic_hits: list[str] = []
        self.error: str | None = None
        self.scanned = False


def scan_elf_blob(path: str, buf: bytes, member: str = "") -> FileResult:
    label = f"{path}({member})" if member else path
    res = FileResult(label)
    slices, archs, err = parse_elf_ranges(buf)
    if err == "not-riscv":
        res.kind = "skip"
        return res
    if err:
        res.kind = "error"
        res.error = err
        return res
    res.kind = "elf"
    res.scanned = True
    res.archs = archs
    for off, data, where in slices:
        hits, total = scan_code_bytes(data, off)
        if not total:
            continue
        res.insn_total += total
        res.where_counts[where] = res.where_counts.get(where, 0) + total
        for at, insn, kind in hits:
            res.insn_hits.append((label, at, insn, kind, where))
    for a in archs:
        if isa_declares_vector(a):
            res.attr_hits.append((label, a))
    return res


def read_file_limited(path: str, limit: int = 256 * 1024 * 1024) -> bytes:
    st = os.stat(path)
    if st.st_size > limit:
        raise ToolError(f"{path}: {st.st_size} bytes, refusing to slurp (>{limit}). This is not a pass.")
    with open(path, "rb") as f:
        data = f.read(limit + 1)
    if len(data) > limit:
        raise ToolError(f"{path}: grew beyond scan limit while being read")
    return data


def scan_path_file(path: str) -> FileResult:
    try:
        with open(path, "rb") as f:
            head = f.read(8)
    except OSError as e:
        r = FileResult(path)
        r.kind = "error"
        r.error = str(e)
        return r
    if head[:4] == b"\x7fELF":
        try:
            buf = read_file_limited(path)
        except (ToolError, OSError) as e:
            r = FileResult(path)
            r.kind = "error"
            r.error = str(e)
            return r
        return scan_elf_blob(path, buf)
    if head == b"!<thin>\n":
        r = FileResult(path)
        r.kind, r.error = "error", "thin archives reference external members and are unsupported"
        return r
    if head.startswith(b"!<arch>"):
        try:
            buf = read_file_limited(path)
        except (ToolError, OSError) as e:
            r = FileResult(path)
            r.kind = "error"
            r.error = str(e)
            return r
        try:
            members = parse_ar_members(buf)
        except ToolError as e:
            r = FileResult(path)
            r.kind, r.error = "error", str(e)
            return r
        if not members:
            r = FileResult(path)
            r.kind = "skip"
            return r
        combined = FileResult(path)
        combined.kind = "ar"
        saw_riscv = False
        for name, data in members:
            one = scan_elf_blob(path, data, name)
            if one.kind == "skip":
                continue
            saw_riscv = True
            combined.scanned = combined.scanned or one.scanned
            combined.insn_hits.extend(one.insn_hits)
            combined.insn_total += one.insn_total
            for w, c in one.where_counts.items():
                combined.where_counts[w] = combined.where_counts.get(w, 0) + c
            combined.attr_hits.extend(one.attr_hits)
            combined.archs.extend(one.archs)
            if one.error:
                combined.error = one.error
                combined.kind = "error"
        if not saw_riscv:
            combined.kind = "skip"
        return combined
    r = FileResult(path)
    r.kind = "skip"
    return r


# ---------------------------------------------------------------------------
# Collect paths
# ---------------------------------------------------------------------------

def collect_files(roots: list[str]) -> tuple[list[str], list[str]]:
    files: list[str] = []
    errors: list[str] = []
    seen: set[tuple[int, int]] = set()

    def consider(path: str) -> None:
        try:
            st = os.stat(path, follow_symlinks=True)
        except OSError as e:
            errors.append(f"{path}: {e}")
            return
        key = (st.st_dev, st.st_ino)
        if key in seen:
            return
        seen.add(key)
        if stat.S_ISREG(st.st_mode):
            files.append(path)
        elif stat.S_ISDIR(st.st_mode):
            try:
                names = os.listdir(path)
            except OSError as e:
                errors.append(f"{path}: {e}")
                return
            for n in names:
                consider(os.path.join(path, n))

    for root in roots:
        if not os.path.lexists(root):
            errors.append(f"{root}: does not exist")
            continue
        consider(root)
    return files, errors


# ---------------------------------------------------------------------------
# Optional objdump second pass
# ---------------------------------------------------------------------------

def objdump_mnemonic_hits(objdump: str, path: str) -> tuple[list[str], str | None]:
    """objdump is mnemonic extra credit. Failure (host cannot disassemble riscv) is not a scan failure."""
    try:
        proc = subprocess.run(
            [objdump, "-d", path],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
            timeout=120,
        )
    except (OSError, subprocess.TimeoutExpired) as e:
        return [], f"objdump cannot run: {e}"
    err = proc.stderr.decode("utf-8", "replace")
    out = proc.stdout.decode("utf-8", "replace")
    if proc.returncode != 0 and not out.strip():
        return [], f"objdump exited {proc.returncode}: {err.strip()[:200]}"
    lowered = err.lower()
    if any(s in lowered for s in ("not recognized", "can't disassemble", "unrecognised", "unknown file format")):
        return [], f"objdump does not recognize this ELF (host llvm-objdump likely has no RISC-V): {err.strip()[:200]}"
    hits = []
    for line in out.splitlines():
        # Ignore symbol names/comments that happen to spell a vector mnemonic.
        # Both GNU and LLVM print address: encoding mnemonic for instructions.
        insn = re.match(r"^\s*[0-9a-fA-F]+:\s+(?:[0-9a-fA-F]{2,8}\s+)+(.+)$", line)
        if insn and VECTOR_MNEMONIC_RE.search(insn.group(1).split("#", 1)[0].split("<", 1)[0]):
            hits.append(line.strip())
            if len(hits) >= 8:
                break
    return hits, None


# ---------------------------------------------------------------------------
# Minimal ELF factory (self-test; no cross toolchain required)
# ---------------------------------------------------------------------------

def make_riscv64_elf(
    code: bytes,
    arch: str | None = None,
    rodata: bytes = b"",
    gap: bytes = b"",
    drop_shdrs: bool = False,
) -> bytes:
    """One PT_LOAD (R+X) covering file offset 0 through the end of `gap`.

    This is the real layout shape and the false-positive source: the
    executable segment also holds the ELF header, phdrs, shdrs, .shstrtab,
    .text, .rodata, and unclaimed alignment residue (`gap`). `rodata` holds
    data that looks like vector insns; `drop_shdrs` mimics an sstripped object.
    """
    EM = 243
    ET_DYN = 3
    PT_LOAD = 1
    PF_R, PF_X = 4, 1
    SHT_NULL, SHT_PROGBITS, SHT_STRTAB = 0, 1, 3
    SHF_ALLOC, SHF_EXEC = 2, 4

    shstr = b"\0.text\0.shstrtab\0.rodata\0"
    names = {".text": 1, ".shstrtab": 7, ".rodata": 17}
    if arch is not None:
        names[".riscv.attributes"] = len(shstr)
        shstr += b".riscv.attributes\0"

    attr = b""
    if arch is not None:
        vendor = b"riscv\0"
        file_attrs = bytes([5]) + arch.encode("ascii") + b"\0"
        file_sub = bytes([1]) + struct.pack("<I", 1 + 4 + len(file_attrs)) + file_attrs
        sub = struct.pack("<I", 4 + len(vendor) + len(file_sub)) + vendor + file_sub
        attr = b"A" + sub

    n_shdr = 3 + (1 if rodata else 0) + (1 if arch is not None else 0)
    off = 64 + 56
    shoff = off
    off += n_shdr * 64
    shstr_off = off
    off += len(shstr)
    text_off = off
    off += len(code)
    rodata_off = off
    off += len(rodata)
    gap_off = off
    off += len(gap)
    seg_end = off
    attr_off = off

    def shdr(name_off, typ, flags, addr, offset, size, addralign=1):
        return struct.pack(
            "<IIQQQQIIQQ",
            name_off, typ, flags, addr, offset, size, 0, 0, addralign, 0,
        )

    shdrs = b""
    shdrs += shdr(0, SHT_NULL, 0, 0, 0, 0)
    shdrs += shdr(names[".text"], SHT_PROGBITS, SHF_ALLOC | SHF_EXEC, text_off, text_off, len(code), 4)
    shdrs += shdr(names[".shstrtab"], SHT_STRTAB, 0, 0, shstr_off, len(shstr))
    if rodata:
        shdrs += shdr(names[".rodata"], SHT_PROGBITS, SHF_ALLOC, rodata_off, rodata_off, len(rodata), 4)
    if arch is not None:
        shdrs += shdr(
            names[".riscv.attributes"],
            SHT_RISCV_ATTRIBUTES,
            0, 0, attr_off, len(attr),
        )
    assert len(shdrs) == n_shdr * 64

    # Segment starts at 0: header / phdr / shdr / .shstrtab sit in the
    # executable segment, same as ld without -z separate-code.
    phdr = struct.pack(
        "<IIQQQQQQ",
        PT_LOAD, PF_R | PF_X,
        0, 0, 0,
        seg_end, seg_end, 4,
    )
    ehdr = b"\x7fELF" + bytes([2, 1, 1, 0]) + bytes(8)
    ehdr += struct.pack(
        "<HHIQQQIHHHHHH",
        ET_DYN, EM, 1,
        text_off, 64, 0 if drop_shdrs else shoff, 0,
        64, 56, 1, 64, 0 if drop_shdrs else n_shdr, 0 if drop_shdrs else 2,
    )
    assert len(ehdr) == 64 and len(phdr) == 56
    return ehdr + phdr + shdrs + shstr + code + rodata + gap + attr


def _addi_nop() -> bytes:
    return struct.pack("<I", 0x00000013)  # addi x0, x0, 0


def _vsetvli() -> bytes:
    # vsetvli zero, zero, e8, m1, tu, mu — opcode 0x57; attribute-only checkers miss this
    return struct.pack("<I", 0x00007057)


def _vle8() -> bytes:
    # LOAD-FP + width=000: objdump often prints unknown; the encoding scan must report it
    return struct.pack("<I", 0x00000007)


def _flw() -> bytes:
    return struct.pack("<I", 0x00002007)  # scalar flw, width=010, must not report


def _c_nop() -> bytes:
    return struct.pack("<H", 0x0001)


def _x86_elf() -> bytes:
    """Minimal ELF64 x86-64: foreign arch must be skipped, not treated as a pass."""
    ehdr = b"\x7fELF" + bytes([2, 1, 1, 0]) + bytes(8)
    ehdr += struct.pack(
        "<HHIQQQIHHHHHH",
        3, 62, 1, 0, 0, 0, 0, 64, 0, 0, 64, 0, 0,
    )
    return ehdr


_REAL_GCC_SRC = r"""
typedef unsigned int u32;
typedef unsigned long uptr;
/* Instruction-like words and binary-wrapper strings in non-executable data
   must not be mistaken for vector instructions in executable sections. */
const u32 table[] = {
  0x70617257u, 0x00000007u, 0x00000187u, 0x00000327u, 0x00000557u,
  0x00000357u, 0xb61800d7u, 0x7c96f087u, 0xe4beff87u, 0x00000057u,
};
const char *const msgs[] = { "cosmic-comp-wrapped", "/nix/store/xxx-bash", "Wrap" };
int sum(const int *a, int n) { int s = 0; for (int i = 0; i < n; i++) s += a[i] * 3; return s; }
void _start(void) {
  int a[64];
  for (int i = 0; i < 64; i++) a[i] = (int)table[i % 10] ^ (int)(uptr)msgs[i % 3];
  volatile int r = sum(a, 64);
  (void)r;
  for (;;) {}
}
"""


def _find_riscv_gcc() -> str | None:
    import shutil

    env = os.environ.get("RISCV_GCC")
    if env:
        return shutil.which(env) or env
    for c in (
        "riscv64-elf-gcc",
        "riscv64-unknown-elf-gcc",
        "riscv64-linux-gnu-gcc",
        "riscv64-unknown-linux-gnu-gcc",
    ):
        p = shutil.which(c)
        if p:
            return p
    return None


def _real_toolchain_cases(td: str, expect) -> None:
    """Build real linker layouts with a real cross gcc; check both directions.

    Real toolchain outputs complement the synthetic malformed-container fixtures
    with linker-generated code/data sections and ISA attributes.
    """
    gcc = _find_riscv_gcc()
    if not gcc:
        print("  skip no real cross gcc on PATH (set RISCV_GCC= to enable)")
        return
    src = os.path.join(td, "real.c")
    with open(src, "w") as f:
        f.write(_REAL_GCC_SRC)
    base = ["-O3", "-ffreestanding", "-nostdlib", "-nostdinc", "-mabi=lp64d"]
    built: dict[str, str] = {}
    for tag, march in (("scalar", "rv64gc"), ("vector", "rv64gcv")):
        out = os.path.join(td, f"real-{tag}.elf")
        proc = subprocess.run(
            [gcc, *base, f"-march={march}", "-o", out, src],
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            check=False,
            timeout=180,
        )
        if proc.returncode != 0 or not os.path.exists(out):
            print(f"  skip {gcc} cannot build -march={march}: "
                  f"{proc.stdout.decode('utf-8', 'replace').strip()[:160]}")
            return
        built[tag] = out

    r = scan_path_file(built["scalar"])
    expect(
        f"real gcc rv64gc: fake vector words in .rodata are silent ({os.path.basename(gcc)})",
        r.scanned and r.insn_total == 0,
        f"{r.insn_total} hits, where={r.where_counts}",
    )
    r = scan_path_file(built["vector"])
    expect(
        "real gcc rv64gcv: autovec caught, and only in .text",
        r.scanned and r.insn_total > 0 and set(r.where_counts) == {".text"},
        f"{r.insn_total} hits, where={r.where_counts}",
    )
    expect(
        "real gcc rv64gcv: Tag_RISCV_arch recognized",
        bool(r.attr_hits),
        f"archs={r.archs[:1]}",
    )


def run_self_test() -> int:
    failures = 0

    def expect(name: str, cond: bool, detail: str = "") -> None:
        nonlocal failures
        if cond:
            print(f"  ok  {name}")
        else:
            failures += 1
            print(f"  FAIL {name}{': ' + detail if detail else ''}")

    cases = [
        ("rv64gc", False),
        ("rv64gcv", True),
        ("rv64gv", True),
        ("rv64imafdcv", True),
        ("rv64i2p1_m2p0_a2p1_f2p2_d2p2_c2p0_v1p0", True),
        ("rv64gc_zve64x", True),
        ("rv64gc_zvl128b", True),
        ("rv64gc_zvfhmin", True),
        ("rv64gc_zicsr_zifencei", False),
        ("rv64i2p1_m2p0_a2p1_f2p2_d2p2_c2p0_zicsr2p0", False),
        ("rv64gc_xtheadvector", True),
        ("rv64gcv_zicsr2p0", True),
    ]
    print("ISA strings (old regex missed rv64gcv / unversioned zve):")
    for s, want in cases:
        got = isa_declares_vector(s)
        expect(s, got == want, f"got {got} want {want}")

    print("Instruction encodings:")
    expect("addi is not vector", classify_vector_insn(0x00000013) is None)
    expect("vsetvli is OP-V", classify_vector_insn(0x00007057) == "vsetvl*")
    expect("vle8 width=000", classify_vector_insn(0x00000007) == "VLE")
    expect("flw width=010 is silent", classify_vector_insn(0x00002007) is None)
    expect("vse width=111", classify_vector_insn(0x00007027) == "VSE")

    print("Minimal ELF:")
    with tempfile.TemporaryDirectory() as td:
        good = os.path.join(td, "good")
        bad_insn = os.path.join(td, "bad-insn")
        bad_vle = os.path.join(td, "bad-vle")
        bad_attr = os.path.join(td, "bad-attr")
        rust_like = os.path.join(td, "rust-stripped")
        fp = os.path.join(td, "flw")
        compressed = os.path.join(td, "c-nop")
        x86 = os.path.join(td, "x86")
        sub = os.path.join(td, "rootfs", "lib")
        os.makedirs(sub)
        open(os.path.join(td, "rootfs", "README"), "w").write("not elf\n")
        with open(os.path.join(good), "wb") as f:
            f.write(make_riscv64_elf(_addi_nop(), "rv64gc"))
        with open(bad_insn, "wb") as f:
            f.write(make_riscv64_elf(_vsetvli(), "rv64gc"))
        with open(bad_vle, "wb") as f:
            # No attribute, like stripped Rust: the old script recorded unknown then PASS
            f.write(make_riscv64_elf(_vle8(), None))
        with open(bad_attr, "wb") as f:
            f.write(make_riscv64_elf(_addi_nop(), "rv64gcv"))
        with open(rust_like, "wb") as f:
            f.write(make_riscv64_elf(_addi_nop(), None))
        with open(fp, "wb") as f:
            f.write(make_riscv64_elf(_flw(), "rv64gc"))
        with open(compressed, "wb") as f:
            f.write(make_riscv64_elf(_c_nop() + _addi_nop(), "rv64gc"))
        with open(x86, "wb") as f:
            f.write(_x86_elf())
        with open(os.path.join(sub, "libc.so"), "wb") as f:
            f.write(make_riscv64_elf(_addi_nop(), "rv64gc"))

        r = scan_path_file(good)
        expect("scalar + rv64gc passes", r.scanned and not r.insn_hits and not r.attr_hits)
        r = scan_path_file(bad_insn)
        expect("attr says gc but encoding is vsetvli → hit", bool(r.insn_hits))
        r = scan_path_file(bad_vle)
        expect("no attr + vle8 (stripped Rust) → hit", bool(r.insn_hits), "old script: unknown+PASS")
        r = scan_path_file(bad_attr)
        expect("rv64gcv attr (old regex miss) → hit", bool(r.attr_hits))
        r = scan_path_file(rust_like)
        expect("clean scalar with no attr → still scanned and passes", r.scanned and not r.insn_hits and not r.attr_hits)
        r = scan_path_file(fp)
        expect("scalar flw is silent", r.scanned and not r.insn_hits)
        r = scan_path_file(compressed)
        expect("c.nop + addi is silent", r.scanned and not r.insn_hits)
        r = scan_path_file(x86)
        expect("x86 ELF skip", r.kind == "skip")

        # cosmic-comp once reported 18403 phantom hits from non-insn bytes
        # in an executable PT_LOAD: wrapper strings, small integers in
        # reloc tables. Those are not SHF_EXECINSTR and must stay silent.
        noise = b"".join(
            struct.pack("<I", w)
            for w in (0x70617257, 0x00000007, 0x00000187, 0x00000327, 0x00000557)
        ) + b"cosmic-comp-wrapped\0/nix/store/...-bash\0"
        fp_seg = os.path.join(td, "rodata-in-x-seg")
        with open(fp_seg, "wb") as f:
            f.write(make_riscv64_elf(_addi_nop() * 4, "rv64gc", rodata=noise))
        r = scan_path_file(fp_seg)
        expect(
            ".rodata/strings in an executable segment stay silent",
            r.scanned and not r.insn_hits,
            f"{r.insn_total} hits: {r.insn_hits[:3]}",
        )

        # Same object with the section table sstripped: whole-segment scan,
        # must be loud, must not silently pass.
        fp_stripped = os.path.join(td, "rodata-in-x-seg-nosh")
        with open(fp_stripped, "wb") as f:
            f.write(make_riscv64_elf(_addi_nop() * 4, "rv64gc", rodata=noise, drop_shdrs=True))
        r = scan_path_file(fp_stripped)
        expect(
            "no section table → fall back to whole-segment scan and report",
            r.scanned and bool(r.insn_hits) and "phdr-only" in r.where_counts,
            f"where={r.where_counts}",
        )

        # Residue in the segment that belongs to no section. The linker
        # only pads there; a real insn must be seen.
        stray = os.path.join(td, "phdr-gap")
        with open(stray, "wb") as f:
            f.write(make_riscv64_elf(_addi_nop(), "rv64gc", gap=_vsetvli()))
        r = scan_path_file(stray)
        expect(
            "vsetvli in unclaimed executable-segment residue still hits",
            bool(r.insn_hits) and any(w.startswith("phdr-gap") for w in r.where_counts),
            f"where={r.where_counts}",
        )

        # Section table present, real vector insn in .text: this is the catch.
        real = os.path.join(td, "real-rvv-in-text")
        with open(real, "wb") as f:
            f.write(make_riscv64_elf(_addi_nop() + _vsetvli(), "rv64gc", rodata=noise))
        r = scan_path_file(real)
        expect(
            "vsetvli in .text is reported, and only it",
            r.insn_total == 1 and r.where_counts.get(".text") == 1,
            f"where={r.where_counts}",
        )
        expect(
            "hit carries the section name",
            bool(r.insn_hits) and r.insn_hits[0][4] == ".text",
            f"{r.insn_hits[:1]}",
        )

        files, errs = collect_files([os.path.join(td, "rootfs"), good])
        expect("recursive dir + single file", len(files) >= 2 and not errs)
        scanned = 0
        for p in files:
            rr = scan_path_file(p)
            if rr.scanned:
                scanned += 1
        expect("ELFs in the directory are scanned", scanned >= 2)

        empty = os.path.join(td, "empty")
        os.makedirs(empty)
        files, _ = collect_files([empty])
        n = sum(1 for p in files if scan_path_file(p).scanned)
        expect("empty dir: 0 RISC-V ELFs (caller should FAIL)", n == 0)

        # Archive
        ar = os.path.join(td, "lib.a")
        elf = make_riscv64_elf(_vsetvli(), None)
        name = b"v.o/" + b" " * (16 - 4)
        # Standard ar header: 16 name, 12 mtime, 6 uid, 6 gid, 8 mode, 10 size, `\n
        size = f"{len(elf):<10}".encode("ascii")
        hdr = (b"v.o/" + b" " * 12)[:16] + b"0" * 12 + b" " * 6 + b" " * 6 + b" " * 8 + size + b"`\n"
        with open(ar, "wb") as f:
            f.write(b"!<arch>\n" + hdr + elf + (b"\0" if len(elf) & 1 else b""))
        r = scan_path_file(ar)
        expect("vsetvli inside .a hits", r.kind == "ar" and bool(r.insn_hits))

        print("Real cross toolchain (run if present; synthetic ELF cannot prove layout):")
        _real_toolchain_cases(td, expect)

    # rust-stripped / bad-vle above exercise missing attributes with real bytes.

    if failures:
        print(f"\nself-test FAIL: {failures} cases")
        return 1
    print("\nself-test PASS")
    return 0


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------

def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(
        description="Scan riscv64 ELF / archives for vector insns (encoding first, not mnemonics)."
    )
    p.add_argument("paths", nargs="*", help="files or directories; dirs recurse, follow symlinks")
    p.add_argument("--self-test", action="store_true", help="run the built-in regressions; ignore paths")
    p.add_argument("--objdump", default=os.environ.get("OBJDUMP", ""), help="optional mnemonic second pass")
    p.add_argument("--max-hits", type=int, default=6, help="max hits printed per file")
    args = p.parse_args(argv)

    if args.self_test:
        return run_self_test()
    if not args.paths:
        print("usage: check-no-rvv.py <file-or-dir> [file-or-dir...]", file=sys.stderr)
        print("       check-no-rvv.py --self-test", file=sys.stderr)
        return 2

    files, collect_errs = collect_files(args.paths)
    for e in collect_errs:
        print(f"error    {e}", file=sys.stderr)

    if collect_errs and not files:
        print("FAIL: none of the given paths are readable. This is not a pass.", file=sys.stderr)
        return 2

    results: list[FileResult] = []
    skipped = 0
    errors = list(collect_errs)
    for path in files:
        r = scan_path_file(path)
        if r.kind == "skip":
            skipped += 1
            continue
        if r.kind == "error":
            errors.append(f"{path}: {r.error}")
            print(f"error    {path}: {r.error}")
            continue
        if args.objdump and r.kind == "elf":
            mhits, merr = objdump_mnemonic_hits(args.objdump, path)
            if merr:
                # Host objdump not knowing RISC-V is not a failure: encoding scan already ran.
                r.mnemonic_hits = []
            else:
                r.mnemonic_hits = mhits
        results.append(r)

    vector_files = 0
    scanned = 0
    for r in results:
        if not r.scanned:
            continue
        scanned += 1
        hit = bool(r.insn_hits or r.attr_hits or r.mnemonic_hits)
        if not hit:
            continue
        vector_files += 1
        print(f"VECTOR   {r.path}")
        shown = 0
        for _label, at, insn, kind, where in r.insn_hits:
            if shown >= args.max_hits:
                break
            print(f"           +0x{at:x}  0x{insn:08x}  {kind}  in {where}")
            shown += 1
        if r.insn_total > shown:
            print(f"           … {r.insn_total - shown} more encoding hits")
        if r.where_counts:
            per = ", ".join(
                f"{w}={c}"
                for w, c in sorted(r.where_counts.items(), key=lambda kv: -kv[1])[:6]
            )
            print(f"           where: {per}")
        if any(w == "phdr-only" for w in r.where_counts):
            print("           note: this object has no section headers, so the")
            print("           whole segment was scanned and data bytes are read")
            print("           as insns. Confirm it was actually sstripped.")
        for _label, arch in r.attr_hits:
            print(f"           Tag_RISCV_arch  {arch}")
        for line in r.mnemonic_hits[: args.max_hits]:
            print(f"           objdump  {line}")
        print("           note: runtime dispatch may leave these instructions unused.")
        print("           Review the exact library and kernel capability detection;")
        print("           instruction presence still fails this strict scan.")

    print()
    print(f"scanned {scanned} RISC-V ELF files/archive containers ({skipped} other files ignored)")
    print(f"  with vector encodings or ISA attributes: {vector_files}")
    print(f"  unreadable / unparseable: {len(errors)}")
    if args.objdump:
        print(f"  objdump extra pass: {args.objdump}")
    else:
        print("  objdump extra pass: skipped (encoding scan is the authority)")

    if errors:
        print()
        print("FAIL: a file was unreadable or unparseable. Missing tools / bad ELF is not a pass.")
        return 2
    if scanned == 0:
        print()
        print("FAIL: no RISC-V ELF was scanned. Wrong directory or the tool did not scan; not clean.")
        return 2
    if vector_files:
        print()
        print("FAIL: objects above contain vector encodings or declare a vector ISA.")
        print("The kernel has no VectorContext; sstatus.VS is never set, so these")
        print("insns trap if executed in userspace.")
        return 1
    print()
    print("PASS: scanned RISC-V ELFs have no vector encodings and no vector ISA attribute.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
