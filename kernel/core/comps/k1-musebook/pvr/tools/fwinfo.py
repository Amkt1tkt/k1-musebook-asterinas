#!/usr/bin/env python3
# K1 MUSE Book: offline firmware container validation and reporting.
# SPDX-License-Identifier: MIT OR GPL-2.0-only
"""Dump the device-info block of a PowerVR firmware image.

Host-side only; no code is built into the kernel. Decode the firmware image
and derive the same runtime limits as the current driver. This is a container
inspection tool, not proof that the real GPU boots or executes commands.

Bit indices and valued features are read from src/rogue/fwif.rs and
src/device_info.rs. Sharing those tables avoids transcription drift, but is
not an independent oracle for whether the driver's own tables are correct.
Unsupported/truncated layouts and inconsistent parameter counts fail before
runtime values are printed.

Usage:

    ./fwinfo.py rogue_36.29.52.182_v1.fw
    ./fwinfo.py --json rogue_36.29.52.182_v1.fw
"""

from __future__ import annotations

import argparse
import json
import os
import re
import struct
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
SRC = os.path.join(HERE, "..", "src")

FW_BLOCK_SIZE = 4096
INFO_VERSION = 3
FLAG_OPEN_SOURCE = 1 << 0

SECTION_IDS = {
    0: "META_CODE",
    1: "META_PRIVATE_DATA",
    2: "META_COREMEM_CODE",
    3: "META_COREMEM_DATA",
    4: "MIPS_CODE",
    5: "MIPS_EXCEPTIONS_CODE",
    6: "MIPS_BOOT_CODE",
    7: "MIPS_PRIVATE_DATA",
    8: "MIPS_BOOT_DATA",
    9: "MIPS_STACK",
    10: "RISCV_UNCACHED_CODE",
    11: "RISCV_CACHED_CODE",
    12: "RISCV_PRIVATE_DATA",
    13: "RISCV_COREMEM_CODE",
    14: "RISCV_COREMEM_DATA",
}

SECTION_TYPES = {0: "NONE", 1: "FW_CODE", 2: "FW_DATA", 3: "FW_COREMEM_CODE", 4: "FW_COREMEM_DATA"}


# --- reading the port's own tables ------------------------------------------


def _read(path: str) -> str:
    with open(path, "r", encoding="utf-8") as handle:
        return handle.read()


def load_bit_tables() -> tuple[dict[int, str], dict[int, str], dict[int, str]]:
    """Bit index -> name, for quirks, enhancements and features."""
    text = _read(os.path.join(SRC, "rogue", "fwif.rs"))
    start = text.index("pub mod dev_info_bits {")
    body = text[start:]

    out = []
    for submod in ("quirks", "enhancements", "features"):
        match = re.search(
            r"pub mod " + submod + r" \{(.*?)\n    \}", body, re.DOTALL
        )
        if match is None:
            sys.exit(f"fwinfo: could not find dev_info_bits::{submod} in fwif.rs")
        table = {}
        for name, value in re.findall(
            r"pub const (\w+): u32 = (\d+);", match.group(1)
        ):
            if name == "MAX":
                continue
            table[int(value)] = name
        if not table:
            sys.exit(f"fwinfo: dev_info_bits::{submod} parsed empty")
        out.append(table)
    return out[0], out[1], out[2]


def load_valued_features(features: dict[int, str]) -> set[int]:
    """The feature bits the firmware follows with a u64 parameter."""
    text = _read(os.path.join(SRC, "device_info.rs"))
    match = re.search(
        r"pub const VALUED_FEATURES: &\[u32\] = \{(.*?)\n\};", text, re.DOTALL
    )
    if match is None:
        sys.exit("fwinfo: could not find VALUED_FEATURES in device_info.rs")
    names = re.findall(r"F::(FEATURE_\w+),", match.group(1))
    if not names:
        sys.exit("fwinfo: VALUED_FEATURES parsed empty")

    by_name = {name: bit for bit, name in features.items()}
    bits = set()
    for name in names:
        if name not in by_name:
            sys.exit(f"fwinfo: VALUED_FEATURES names {name}, absent from dev_info_bits")
        bits.add(by_name[name])
    return bits


# --- the image ---------------------------------------------------------------


def parse_info_header(image: bytes) -> dict:
    if len(image) < FW_BLOCK_SIZE:
        sys.exit("fwinfo: image shorter than one block")
    off = len(image) - FW_BLOCK_SIZE
    fields = struct.unpack_from("<IIIIQIIHHIII", image, off)
    if fields[0] != INFO_VERSION:
        sys.exit(f"fwinfo: unsupported info version {fields[0]}")
    header_len, entry_count, entry_size = fields[1:4]
    if not 48 <= header_len <= FW_BLOCK_SIZE:
        sys.exit("fwinfo: info header length outside its final block")
    if entry_size != 24 or entry_count > (FW_BLOCK_SIZE - header_len) // entry_size:
        sys.exit("fwinfo: invalid/truncated layout table")
    if fields[10] < 32 or fields[10] > off:
        sys.exit("fwinfo: device-info size outside image payload")
    header = {
        "info_version": fields[0],
        "header_len": fields[1],
        "layout_entry_num": fields[2],
        "layout_entry_size": fields[3],
        "bvnc": fields[4],
        "fw_page_size": fields[5],
        "flags": fields[6],
        "fw_version_major": fields[7],
        "fw_version_minor": fields[8],
        "fw_version_build": fields[9],
        "device_info_size": fields[10],
    }
    packed = header["bvnc"]
    header["bvnc_str"] = "{}.{}.{}.{}".format(
        (packed >> 48) & 0xFFFF,
        (packed >> 32) & 0xFFFF,
        (packed >> 16) & 0xFFFF,
        packed & 0xFFFF,
    )

    entries = []
    entry_off = off + header["header_len"]
    for _ in range(header["layout_entry_num"]):
        raw = struct.unpack_from("<IIIIII", image, entry_off)
        entries.append(
            {
                "id": SECTION_IDS.get(raw[0], f"UNKNOWN({raw[0]})"),
                "type": SECTION_TYPES.get(raw[1], f"UNKNOWN({raw[1]})"),
                "base_addr": raw[2],
                "max_size": raw[3],
                "alloc_size": raw[4],
                "alloc_offset": raw[5],
            }
        )
        entry_off += header["layout_entry_size"]
    header["layout"] = entries
    return header


def set_bits(words: list[int]) -> list[int]:
    bits = []
    for index, word in enumerate(words):
        for bit in range(64):
            if word & (1 << bit):
                bits.append(index * 64 + bit)
    return bits


def decode_device_info(block: bytes, valued: set[int]) -> dict:
    # Four u64s, matching pvr_fw_info.h and the current DeviceInfoBlockHeader.
    if len(block) < 32:
        sys.exit("fwinfo: device-info header truncated")
    brn_words, ern_words, feat_words, param_words = struct.unpack_from("<QQQQ", block, 0)
    off = 32

    def take(count: int) -> list[int]:
        nonlocal off
        end = off + count * 8
        if end > len(block):
            sys.exit("fwinfo: device-info block truncated")
        words = list(struct.unpack_from("<" + "Q" * count, block, off))
        off = end
        return words

    quirks = take(brn_words)
    enhancements = take(ern_words)
    features = take(feat_words)
    params = take(param_words)

    feature_bits = set_bits(features)
    # One parameter per *present* valued feature, in bit order, with nothing
    # between them. This walk is the whole reason VALUED_FEATURES must be
    # complete; see the module docstring.
    values = {}
    consumed = 0
    for bit in feature_bits:
        if bit in valued:
            if consumed >= len(params):
                sys.exit("fwinfo: ran off the end of the feature parameter array")
            values[bit] = params[consumed]
            consumed += 1

    if consumed != len(params):
        sys.exit("fwinfo: unconsumed feature parameters; tables may be incomplete, runtime values withheld")
    return {
        "header": {
            "brn_mask_size": brn_words,
            "ern_mask_size": ern_words,
            "feature_mask_size": feat_words,
            "feature_param_size": param_words,
        },
        "quirk_bits": set_bits(quirks),
        "enhancement_bits": set_bits(enhancements),
        "feature_bits": feature_bits,
        "feature_values": values,
        "params_consumed": consumed,
        "params_present": len(params),
    }


# --- the two derived answers -------------------------------------------------


def runtime_info(feat_names: set[str], vals: dict[str, int], quirks: set[str], erns: set[str]) -> dict:
    """`DevQueryRuntimeInfo`, transcribed from `pvr/src/driver.rs`.

    Kept in step with `driver.rs`'s `common_store_*`, `max_coeffs`,
    `cdm_max_local_mem_size_regs` and `render::free_list::min_pages`.
    """
    ROGUE_MAX_VERTEX_SHARED_REGISTERS = 0x400
    ROGUE_MAX_PIXEL_SHARED_REGISTERS = 0x400
    ROGUE_MAX_COMPUTE_SHARED_REGISTERS = 0x800
    ROGUE_CSRM_LINE_SIZE_IN_DWORDS = 0x400
    ROGUE_MAX_PER_KERNEL_LOCAL_MEM_SIZE_REGS = 0x1000

    tile_x = vals.get("TILE_SIZE_X", 0)
    tile_y = vals.get("TILE_SIZE_Y", 0)
    max_partitions = vals.get("MAX_PARTITIONS", 0)
    usc_min_out = vals.get("USC_MIN_OUTPUT_REGISTERS_PER_PIX", 0)
    common_store = vals.get("COMMON_STORE_SIZE_IN_DWORDS", 0)
    isp_max_tiles = vals.get("ISP_MAX_TILES_IN_FLIGHT", 0)
    num_clusters = vals.get("NUM_CLUSTERS", 0)

    if tile_x == 16 and tile_y == 16:
        partition_space = tile_x * tile_y * max_partitions * usc_min_out
    else:
        partition_space = max_partitions * 1024

    alloc_region = max(0, common_store - 256 * 4 - partition_space)
    if "BRN_44079" in quirks:
        alloc_region = min(768 * 4 * 4 - 256 * 4, alloc_region)

    phantoms = max(1, -(-num_clusters // 4))
    max_coeff_additional = ROGUE_MAX_VERTEX_SHARED_REGISTERS
    pending_shared = 2 * 1024
    pending_coeff = 0
    max_coeff_pixel = (-(-isp_max_tiles // phantoms)) * ROGUE_MAX_PIXEL_SHARED_REGISTERS
    if "BRN_48492" in quirks and "COMPUTE_OVERLAP" not in feat_names:
        pending_coeff = 2 * 1024
    if "ERN_38748" in erns:
        pending_shared = 0
    if "ERN_38020" in erns:
        max_coeff_additional += ROGUE_MAX_COMPUTE_SHARED_REGISTERS
    max_coeffs = max(
        0,
        (alloc_region + pending_coeff) - (max_coeff_pixel + max_coeff_additional + pending_shared),
    )

    cdm_local = max_coeffs
    if (
        "BRN_48492" in quirks
        and "ROGUEXE" in feat_names
        and "COMPUTE_OVERLAP" not in feat_names
    ):
        cdm_local = max(0, cdm_local - ROGUE_CSRM_LINE_SIZE_IN_DWORDS * 2)
    cdm_local = min(cdm_local, ROGUE_MAX_PER_KERNEL_LOCAL_MEM_SIZE_REGS)

    if "ROGUEXE" not in feat_names:
        min_pages = 50
    elif "BRN_66011" in quirks:
        min_pages = 40
    else:
        min_pages = 25

    return {
        "free_list_min_pages": min_pages,
        "free_list_max_pages": (2 * 1024 * 1024 * 1024) // 4096,
        "common_store_alloc_region_size": alloc_region,
        "common_store_partition_space_size": partition_space,
        "max_coeffs": max_coeffs,
        "cdm_max_local_mem_size_regs": cdm_local,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("image", help="path to a rogue_*.fw firmware image")
    parser.add_argument("--json", action="store_true", help="machine-readable output")
    args = parser.parse_args()

    with open(args.image, "rb") as handle:
        image = handle.read(16 * 1024 * 1024 + 1)
    if len(image) > 16 * 1024 * 1024:
        sys.exit("fwinfo: firmware exceeds the 16 MiB inspection limit")

    quirk_names, ern_names, feature_names = load_bit_tables()
    valued = load_valued_features(feature_names)

    header = parse_info_header(image)
    start = len(image) - FW_BLOCK_SIZE - header["device_info_size"]
    block = image[start : len(image) - FW_BLOCK_SIZE]
    decoded = decode_device_info(block, valued)

    quirks = {quirk_names.get(b, f"UNKNOWN_QUIRK_BIT_{b}") for b in decoded["quirk_bits"]}
    erns = {ern_names.get(b, f"UNKNOWN_ERN_BIT_{b}") for b in decoded["enhancement_bits"]}
    feats = {
        feature_names.get(b, f"UNKNOWN_FEATURE_BIT_{b}") for b in decoded["feature_bits"]
    }
    # Strip the FEATURE_ prefix so the names read like the port's field names.
    feat_short = {n[len("FEATURE_") :] if n.startswith("FEATURE_") else n for n in feats}
    vals = {}
    for bit, value in decoded["feature_values"].items():
        name = feature_names.get(bit, f"UNKNOWN_FEATURE_BIT_{bit}")
        vals[name[len("FEATURE_") :] if name.startswith("FEATURE_") else name] = value

    runtime = runtime_info(feat_short, vals, quirks, erns)

    if args.json:
        print(
            json.dumps(
                {
                    "file": os.path.basename(args.image),
                    "size": len(image),
                    "info_header": {
                        k: v for k, v in header.items() if k != "layout"
                    },
                    "layout": header["layout"],
                    "device_info_header": decoded["header"],
                    "quirks": sorted(quirks),
                    "enhancements": sorted(erns),
                    "features": sorted(feat_short),
                    "feature_values": dict(sorted(vals.items())),
                    "runtime_info": runtime,
                },
                indent=2,
            )
        )
        return 0

    print(f"image                {os.path.basename(args.image)}  ({len(image)} bytes)")
    print("")
    print("== firmware info header (image end - 4096) ==")
    for key in (
        "info_version",
        "header_len",
        "layout_entry_num",
        "layout_entry_size",
        "bvnc_str",
        "fw_page_size",
        "flags",
        "fw_version_major",
        "fw_version_minor",
        "fw_version_build",
        "device_info_size",
    ):
        print(f"  {key:<20} {header[key]}")
    ok = header["info_version"] == INFO_VERSION
    print(f"  {'accepted version':<20} {'yes' if ok else 'NO -- driver rejects'}")
    src = bool(header["flags"] & FLAG_OPEN_SOURCE)
    print(f"  {'open source flag':<20} {'set' if src else 'CLEAR -- driver rejects'}")

    print("")
    print("== layout table ==")
    for entry in header["layout"]:
        print(
            "  {id:<22} {type:<16} base=0x{base_addr:08x} "
            "alloc={alloc_size} @ {alloc_offset}".format(**entry)
        )

    print("")
    print("== device-info block ==")
    dh = decoded["header"]
    print(
        "  mask sizes (u64 words): brn={brn_mask_size} ern={ern_mask_size} "
        "feature={feature_mask_size} param={feature_param_size}".format(**dh)
    )
    print(
        f"  parameters consumed {decoded['params_consumed']} of "
        f"{decoded['params_present']} present"
    )
    print("")
    print(f"== quirks ({len(quirks)}) ==")
    for bit in decoded["quirk_bits"]:
        print(f"  bit {bit:>3}  {quirk_names.get(bit, '(not mapped by this port)')}")
    print(f"  BRN_47217: {'PRESENT' if 'BRN_47217' in quirks else 'ABSENT'}")
    print(f"  BRN_49927: {'PRESENT' if 'BRN_49927' in quirks else 'ABSENT'}")

    print("")
    print(f"== enhancements ({len(erns)}) ==")
    for bit in decoded["enhancement_bits"]:
        print(f"  bit {bit:>3}  {ern_names.get(bit, '(not mapped by this port)')}")

    print("")
    print(f"== features ({len(feats)}) ==")
    for bit in decoded["feature_bits"]:
        name = feature_names.get(bit, "(not mapped by this port)")
        short = name[len("FEATURE_") :] if name.startswith("FEATURE_") else name
        if short in vals:
            print(f"  bit {bit:>3}  {short} = {vals[short]}")
        else:
            print(f"  bit {bit:>3}  {short}")

    print("")
    print("== RUNTIME_INFO: derived from firmware and current source tables ==")
    for key, value in runtime.items():
        print(f"  {key:<36} {value:>10}")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
