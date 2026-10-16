// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: host-side PowerVR register and firmware ABI generation.

//! Host-side generator: upstream PowerVR headers → `src/rogue/*.rs`.
//!
//! See the crate-level docs on the `pvr-regen` binary for usage.

pub mod allow_list;
pub mod emit;
pub mod emit_fwif;
pub mod emit_sf;
pub mod parse;
pub mod parse_fwif;
pub mod parse_sf;

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use allow_list::REGISTER_ALLOW_LIST;
use emit::{GenMeta, emit_cr, emit_defs, emit_heap, emit_mips, emit_mmu};
use emit_fwif::{assert_coverage, emit_dev_info_bits, emit_fwif, emit_fwif_defines};
use emit_sf::emit_sf;
use parse::{ParseError, parse_cr_from_source};
use parse_fwif::{
    collect_fwif_env, parse_layout_asserts, parse_structs, preprocess_fwif, verify_asserts,
};
use parse_sf::parse_stid_fmts;

/// Filenames required under the imagination driver directory / header cache.
pub const REQUIRED_HEADERS: &[&str] = &[
    "pvr_rogue_cr_defs.h",
    "pvr_rogue_cr_defs_client.h",
    "pvr_rogue_defs.h",
    "pvr_rogue_mmu_defs.h",
    "pvr_rogue_mips.h",
    "pvr_rogue_heap_config.h",
    // Firmware-interface ABI
    "pvr_rogue_fwif.h",
    "pvr_rogue_fwif_common.h",
    "pvr_rogue_fwif_shared.h",
    "pvr_rogue_fwif_client.h",
    "pvr_rogue_fwif_dev_info.h",
    "pvr_rogue_fwif_resetframework.h",
    "pvr_rogue_fwif_sf.h",
    "pvr_rogue_fwif_stream.h",
    "pvr_rogue_fwif_check.h",
    "pvr_rogue_fwif_client_check.h",
    "pvr_rogue_fwif_shared_check.h",
    "pvr_rogue_mips_check.h",
];

/// Parsed `../UPSTREAM` commit line.
pub fn read_upstream_commit(upstream_file: &Path) -> Result<String, String> {
    let text = fs::read_to_string(upstream_file)
        .map_err(|e| format!("read {}: {e}", upstream_file.display()))?;
    let mut commits = text.lines().filter_map(|line| {
        let mut words = line.split_whitespace();
        (words.next() == Some("commit")).then(|| words.collect::<Vec<_>>())
    });
    let fields = commits
        .next()
        .ok_or_else(|| format!("{}: no `commit` line", upstream_file.display()))?;
    if commits.next().is_some()
        || fields.len() != 1
        || fields[0].len() != 40
        || !fields[0].bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(format!(
            "{}: expected one full 40-digit commit hash",
            upstream_file.display()
        ));
    }
    Ok(fields[0].to_string())
}

/// Resolve header directory from `--linux` tree or `--headers` cache.
pub fn resolve_header_dir(linux: Option<&Path>, headers: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(h) = headers {
        return Ok(h.to_path_buf());
    }
    if let Some(linux) = linux {
        return Ok(linux.join("drivers/gpu/drm/imagination"));
    }
    Err("specify --linux <tree> or --headers <dir>".into())
}

/// Fetch headers from raw.githubusercontent.com into `dest` when missing.
pub fn ensure_headers(dest: &Path, commit: &str, fetch: bool) -> Result<(), String> {
    fs::create_dir_all(dest).map_err(|e| format!("mkdir {}: {e}", dest.display()))?;
    let base = format!(
        "https://raw.githubusercontent.com/torvalds/linux/{commit}/drivers/gpu/drm/imagination"
    );
    for name in REQUIRED_HEADERS {
        let path = dest.join(name);
        if path.exists() {
            continue;
        }
        if !fetch {
            return Err(format!(
                "missing header {} (pass --fetch to download, or point --headers/--linux at a tree)",
                path.display()
            ));
        }
        let url = format!("{base}/{name}");
        let output = Command::new("curl")
            .args(["-fsSL"])
            .arg(&url)
            .output()
            .map_err(|e| format!("curl: {e}"))?;
        if !output.status.success() {
            return Err(format!("failed to fetch {url}"));
        }
        // Do not let a failed transfer become a cached header on the next run.
        let temporary = path.with_extension(format!("h.part.{}", std::process::id()));
        let result =
            fs::write(&temporary, output.stdout).and_then(|()| fs::rename(&temporary, &path));
        if let Err(err) = result {
            let _ = fs::remove_file(&temporary);
            return Err(format!("publish {}: {err}", path.display()));
        }
    }
    Ok(())
}

/// Generated outputs keyed by relative path under `src/rogue/`.
pub struct Generated {
    pub cr: String,
    pub defs: String,
    pub mmu: String,
    pub mips: String,
    pub heap: String,
    pub fwif: String,
    pub sf: String,
    /// `(offset_asserts, size_asserts)` transcribed from upstream `*_check.h`.
    pub fwif_assert_counts: (usize, usize),
    /// Rows parsed from `stid_fmts[]` (before sorting for emission).
    pub sf_entry_count: usize,
}

impl Generated {
    pub fn files(&self) -> [(&str, &str); 7] {
        [
            ("cr.rs", &self.cr),
            ("defs.rs", &self.defs),
            ("mmu.rs", &self.mmu),
            ("mips.rs", &self.mips),
            ("heap.rs", &self.heap),
            ("fwif.rs", &self.fwif),
            ("sf.rs", &self.sf),
        ]
    }
}

/// Run the generator against a header directory.
pub fn generate(header_dir: &Path, commit: &str) -> Result<Generated, ParseError> {
    let read = |name: &str| -> Result<String, ParseError> {
        fs::read_to_string(header_dir.join(name))
            .map_err(|e| ParseError(format!("read {name}: {e}")))
    };
    let cr_defs = read("pvr_rogue_cr_defs.h")?;
    let cr_client = read("pvr_rogue_cr_defs_client.h")?;
    let defs = read("pvr_rogue_defs.h")?;
    let mmu = read("pvr_rogue_mmu_defs.h")?;
    let mips = read("pvr_rogue_mips.h")?;
    let heap = read("pvr_rogue_heap_config.h")?;

    let fwif_h = read("pvr_rogue_fwif.h")?;
    let fwif_common = read("pvr_rogue_fwif_common.h")?;
    let fwif_shared = read("pvr_rogue_fwif_shared.h")?;
    let fwif_client = read("pvr_rogue_fwif_client.h")?;
    let fwif_dev_info = read("pvr_rogue_fwif_dev_info.h")?;
    let fwif_reset = read("pvr_rogue_fwif_resetframework.h")?;
    let fwif_check = read("pvr_rogue_fwif_check.h")?;
    let fwif_client_check = read("pvr_rogue_fwif_client_check.h")?;
    let fwif_shared_check = read("pvr_rogue_fwif_shared_check.h")?;
    let mips_check = read("pvr_rogue_mips_check.h")?;
    let fwif_sf = read("pvr_rogue_fwif_sf.h")?;

    let regs = parse_cr_from_source(&[&cr_defs, &cr_client])?;
    for entry in REGISTER_ALLOW_LIST {
        if !regs.contains_key(entry.name) {
            return Err(ParseError(format!(
                "allow-list register `{}` not parsed from headers — {}",
                entry.name, entry.why
            )));
        }
    }

    // Seed defs.h evaluation with CR EN / named values from allow-listed regs
    // so composites like ROGUE_CR_SOFT_RESET_RASCALDUSTS_EN resolve.
    let mut cr_seed = std::collections::BTreeMap::new();
    for entry in REGISTER_ALLOW_LIST {
        let reg = &regs[entry.name];
        let sep = if reg.name.contains("__") { "__" } else { "_" };
        for field in &reg.fields {
            if let Some(en) = field.en {
                cr_seed.insert(format!("{}{}{}_EN", reg.name, sep, field.name), en);
            }
            for (vname, v) in &field.values {
                cr_seed.insert(format!("{}{}{}_{}", reg.name, sep, field.name, vname), *v);
            }
        }
    }

    let meta = GenMeta { commit };

    // Firmware structures and layout assertions.
    let fwif_pp = [
        preprocess_fwif(&fwif_common),
        preprocess_fwif(&fwif_shared),
        preprocess_fwif(&fwif_client),
        preprocess_fwif(&fwif_h),
        preprocess_fwif(&mips),
        preprocess_fwif(&fwif_reset),
        preprocess_fwif(&defs),
    ];
    let env = collect_fwif_env(&fwif_pp.iter().map(String::as_str).collect::<Vec<_>>())?;
    let mut structs = std::collections::BTreeMap::new();
    for pp in &fwif_pp {
        let parsed = parse_structs(pp, &env)?;
        for (k, v) in parsed {
            structs.entry(k).or_insert(v);
        }
    }
    let asserts = parse_layout_asserts(&[
        ("fwif_check".into(), fwif_check),
        ("client_check".into(), fwif_client_check),
        ("shared_check".into(), fwif_shared_check),
        ("mips_check".into(), mips_check),
    ])?;
    verify_asserts(&asserts, &structs)?;
    let mut fwif = emit_fwif(&structs, &asserts, &meta)?;
    fwif.push_str(&emit_dev_info_bits(&fwif_dev_info)?);
    fwif.push_str(&emit_fwif_defines(&fwif_h)?);
    let fwif_assert_counts = assert_coverage(&asserts);

    let sf_entries = parse_stid_fmts(&fwif_sf)?;
    let sf_entry_count = sf_entries.len();
    let sf = emit_sf(&sf_entries, &meta)?;

    Ok(Generated {
        cr: emit_cr(&regs, &meta)?,
        defs: emit_defs(&defs, &meta, &cr_seed)?,
        mmu: emit_mmu(&mmu, &meta)?,
        mips: emit_mips(&mips, &meta)?,
        heap: emit_heap(&heap, &meta)?,
        fwif,
        sf,
        fwif_assert_counts,
        sf_entry_count,
    })
}

/// Write or check generated files under `rogue_dir`.
pub fn apply(generated: &Generated, rogue_dir: &Path, check: bool) -> Result<(), String> {
    for (name, raw_content) in generated.files() {
        let content = canonical_rust(raw_content)?;
        let path = rogue_dir.join(name);
        if check {
            let existing =
                fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
            if existing != content {
                return Err(format!(
                    "{} is out of date with headers (re-run pvr-regen without --check)",
                    path.display()
                ));
            }
        } else {
            fs::write(&path, content).map_err(|e| format!("write {}: {e}", path.display()))?;
        }
    }
    Ok(())
}

/// Apply the repository's formatting before both writing and comparing outputs.
///
/// The kernel build does not run this host-only tool. Regeneration requires the
/// repository's pinned rustfmt so the checked-in ABI has one canonical form.
fn canonical_rust(source: &str) -> Result<String, String> {
    let mut child = Command::new("rustfmt")
        .args([
            "--edition",
            "2024",
            "--emit",
            "stdout",
            "--config",
            "imports_granularity=Crate,group_imports=StdExternalCrate,reorder_imports=true",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("run pinned rustfmt: {e}"))?;
    {
        let mut input = child.stdin.take().ok_or("rustfmt stdin unavailable")?;
        input
            .write_all(source.as_bytes())
            .map_err(|e| format!("write rustfmt input: {e}"))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|e| format!("wait rustfmt: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "rustfmt failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    String::from_utf8(output.stdout).map_err(|e| format!("rustfmt output: {e}"))
}

/// Hand-written seed module text from the original `cr.rs` (for tests / docs).
pub const SEED_CORE_ID_PBVNC: &str = r#"/// `RGX_CR_CORE_ID` in its packed BVNC form.
///
/// This is the first register the driver reads, before it knows anything else
/// about the GPU, and it is what makes the driver GPU-agnostic: branch, version,
/// scalable-unit count and config all come from here rather than from a
/// compiled-in table.
pub mod core_id_pbvnc {
    use super::Field;

    /// Register offset.
    pub const OFFSET: u32 = 0x0020;
    /// Branch ID. 36 for BXE-2-32.
    pub const BRANCH_ID: Field = Field {
        shift: 48,
        mask: 0xffff_0000_0000_0000,
    };
    /// Version ID. 29 for BXE-2-32.
    pub const VERSION_ID: Field = Field {
        shift: 32,
        mask: 0x0000_ffff_0000_0000,
    };
    /// Number of scalable units. 52 for BXE-2-32.
    pub const NUMBER_OF_SCALABLE_UNITS: Field = Field {
        shift: 16,
        mask: 0x0000_0000_ffff_0000,
    };
    /// Config ID. 182 for BXE-2-32.
    pub const CONFIG_ID: Field = Field {
        shift: 0,
        mask: 0x0000_0000_0000_ffff,
    };
}"#;

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::emit::extract_core_id_pbvnc_module;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            static ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let id = ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("pvr-regen-{}-{id}", std::process::id()));
            fs::create_dir(&path).expect("create test directory");
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).expect("remove test directory");
        }
    }

    #[test]
    fn upstream_requires_one_exact_commit_field() {
        let scratch = Scratch::new();
        let path = scratch.0.join("UPSTREAM");
        let hash = "0123456789abcdef0123456789abcdef01234567";
        fs::write(&path, format!("# provenance\ncommit {hash}\n")).unwrap();
        assert_eq!(read_upstream_commit(&path).unwrap(), hash);
        for text in [
            "commitment ignored\n",
            "commit short\n",
            "commit 0123456789abcdef0123456789abcdef0123456z\n",
            "commit 0123456789abcdef0123456789abcdef01234567 extra\n",
            "commit 0123456789abcdef0123456789abcdef01234567\ncommit 0123456789abcdef0123456789abcdef01234567\n",
        ] {
            fs::write(&path, text).unwrap();
            assert!(read_upstream_commit(&path).is_err(), "accepted {text:?}");
        }
    }

    fn header_dir() -> PathBuf {
        std::env::var_os("PVR_REGEN_HEADERS")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".cache/headers"))
    }

    fn commit() -> String {
        read_upstream_commit(&PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../UPSTREAM"))
            .expect("UPSTREAM")
    }

    #[test]
    #[ignore = "requires headers at UPSTREAM; set PVR_REGEN_HEADERS and use --include-ignored"]
    fn idempotent_generate_twice() {
        let dir = header_dir();
        let c = commit();
        let a = generate(&dir, &c).expect("generate a");
        let b = generate(&dir, &c).expect("generate b");
        for ((na, ca), (nb, cb)) in a.files().iter().zip(b.files().iter()) {
            assert_eq!(na, nb);
            assert_eq!(
                ca, cb,
                "{na} differed across two generate() calls (idempotence)"
            );
        }
    }

    #[test]
    #[ignore = "requires headers at UPSTREAM; set PVR_REGEN_HEADERS and use --include-ignored"]
    fn core_id_pbvnc_matches_seed_byte_for_byte() {
        let dir = header_dir();
        let c = commit();
        let g = generate(&dir, &c).expect("generate");
        let got = extract_core_id_pbvnc_module(&g.cr).expect("extract module");
        assert_eq!(
            got, SEED_CORE_ID_PBVNC,
            "generated core_id_pbvnc must match the hand-written seed"
        );
    }

    /// Every FWIF define on the allow-list must evaluate, and the MMU cache
    /// flags must come out with the values the firmware acts on. Getting one
    /// wrong invalidates the wrong cache level, which is a silent failure.
    #[test]
    #[ignore = "requires headers at UPSTREAM; set PVR_REGEN_HEADERS and use --include-ignored"]
    fn allow_list_fwif_defines_all_parse() {
        let dir = header_dir();
        let path = dir.join("pvr_rogue_fwif.h");
        let emitted =
            emit_fwif::emit_fwif_defines(&fs::read_to_string(&path).unwrap()).expect("defines");
        for (name, value) in [
            ("ROGUE_FWIF_MMUCACHEDATA_FLAGS_PT", "0x0000_0001"),
            ("ROGUE_FWIF_MMUCACHEDATA_FLAGS_PD", "0x0000_0002"),
            ("ROGUE_FWIF_MMUCACHEDATA_FLAGS_PC", "0x0000_0004"),
            ("ROGUE_FWIF_MMUCACHEDATA_FLAGS_PMTLB", "0x0000_0010"),
            // TLB is PMTLB | 0x8, so it also proves the evaluator resolves a
            // define in terms of an earlier one.
            ("ROGUE_FWIF_MMUCACHEDATA_FLAGS_TLB", "0x0000_0018"),
            ("ROGUE_FWIF_MMUCACHEDATA_FLAGS_INTERRUPT", "0x0400_0000"),
        ] {
            let line = format!("pub const {name}: u32 = {value};");
            assert!(emitted.contains(&line), "missing or wrong: {line}");
        }
    }

    #[test]
    #[ignore = "requires headers at UPSTREAM; set PVR_REGEN_HEADERS and use --include-ignored"]
    fn allow_list_registers_all_parse() {
        let dir = header_dir();
        let c = commit();
        let _ = generate(&dir, &c).expect("every allow-list entry must parse");
    }

    /// Every upstream `stid_fmts[]` row must parse. The fixed header has 775 entries;
    /// a count mismatch requires checking parser coverage and regenerated `sf.rs`.
    #[test]
    #[ignore = "requires headers at UPSTREAM; set PVR_REGEN_HEADERS and use --include-ignored"]
    fn stid_fmts_table_has_every_row() {
        let dir = header_dir();
        let c = commit();
        let g = generate(&dir, &c).expect("sf generate");
        assert_eq!(
            g.sf_entry_count, 775,
            "stid_fmts[] row count at the UPSTREAM commit"
        );
        assert!(
            g.sf.contains("pub static STID_FMTS"),
            "generated sf.rs must contain STID_FMTS"
        );
        assert!(
            g.sf.contains("pub fn lookup"),
            "generated sf.rs must contain lookup()"
        );
        // Sorted: the first emitted id is the NULL placeholder (SF_FIRST).
        assert!(
            g.sf.contains(
                "StidFmt { id: 0x7000_0000, group: 0, params: 0, fmt: \"You should not use this string\" }"
            ),
            "SF_FIRST must be present with its format string"
        );
        assert!(
            g.sf.contains("cCCB Woff update = %d, DM = %d, FWCtx = 0x%08.8x"),
            "MAIN:14 format string must be emitted"
        );
    }

    #[test]
    #[ignore = "requires headers at UPSTREAM; set PVR_REGEN_HEADERS and use --include-ignored"]
    fn fwif_layout_asserts_all_match() {
        let dir = header_dir();
        let c = commit();
        let g = generate(&dir, &c).expect("fwif generate");
        let (offsets, sizes) = g.fwif_assert_counts;
        // Upstream check headers total 792 lines; the executable asserts are the
        // OFFSET_CHECK / SIZE_CHECK / static_assert(offsetof|sizeof) lines.
        assert_eq!(
            offsets + sizes,
            607,
            "transcribe every executable upstream layout assert"
        );
        assert!(
            g.fwif.contains("layout_asserts"),
            "generated fwif.rs must contain layout_asserts module"
        );
        assert!(
            g.fwif.contains("pub struct FwAddr"),
            "FwAddr newtype must be present"
        );
    }

    /// Checks generated device-info bit indices against the pinned protocol header.
    #[test]
    #[ignore = "requires headers at UPSTREAM; set PVR_REGEN_HEADERS and use --include-ignored"]
    fn device_info_indices_match_upstream_protocol() {
        let dir = header_dir();
        let path = dir.join("pvr_rogue_fwif_dev_info.h");
        let bits = emit_fwif::emit_dev_info_bits(&fs::read_to_string(&path).unwrap())
            .expect("dev_info bits");
        assert_eq!(const_u32(&bits, "FEATURE_COMPUTE"), 4);
        assert_eq!(const_u32(&bits, "FEATURE_MIPS"), 27);
        assert_eq!(const_u32(&bits, "BRN_63142"), 8);
        assert_eq!(const_u32(&bits, "ERN_35421"), 0);
    }

    fn const_u32(src: &str, name: &str) -> u32 {
        let pat = format!("pub const {name}: u32 = ");
        let i = src.find(&pat).unwrap_or_else(|| panic!("missing {name}"));
        let rest = &src[i + pat.len()..];
        let end = rest.find(';').unwrap();
        rest[..end].trim().parse().unwrap()
    }
}
