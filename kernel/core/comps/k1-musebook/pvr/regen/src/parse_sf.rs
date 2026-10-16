// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: host-side PowerVR register and firmware ABI generation.

//! Parse `stid_fmts[]` and `enum rogue_fw_log_sfgroups` from `pvr_rogue_fwif_sf.h`.

use std::collections::BTreeMap;

use crate::parse::ParseError;

/// One row of upstream `stid_fmts[]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StidEntry {
    /// Upstream `ROGUE_FW_LOG_CREATESFID(message, group, params)` value.
    pub id: u32,
    /// Ordinal of `ROGUE_FW_GROUP_*` in `enum rogue_fw_log_sfgroups`.
    pub group: u8,
    /// Argument count, the `e` of `ROGUE_FW_LOG_CREATESFID(a, b, e)`.
    pub params: u8,
    /// Message number within the group, the `a` of `CREATESFID`.
    pub message: u16,
    /// Upstream `rogue_km_stid_fmt::name` — the C printf template.
    pub fmt: String,
}

/// Upstream `ROGUE_FW_LOG_IDMARKER`.
const LOG_IDMARKER: u32 = 0x7000_0000;

/// Parse the group enum and every `stid_fmts[]` entry from the sf header.
///
/// Anything the parser does not understand inside the table is an error: a
/// silently dropped row would make a real firmware id look like corruption.
pub fn parse_stid_fmts(src: &str) -> Result<Vec<StidEntry>, ParseError> {
    let groups = parse_sf_groups(src)?;
    let body = stid_fmts_body(src)?;

    let mut entries = Vec::new();
    let mut rest = body;
    while let Some(pos) = rest.find("ROGUE_FW_LOG_CREATESFID") {
        rest = &rest[pos + "ROGUE_FW_LOG_CREATESFID".len()..];
        let rest_trim = rest.trim_start();
        if !rest_trim.starts_with('(') {
            return Err(ParseError(
                "pvr_rogue_fwif_sf.h: CREATESFID not followed by '('".into(),
            ));
        }
        let (args, after) = split_paren_args(rest_trim)?;
        let parts: Vec<&str> = args.split(',').map(str::trim).collect();
        if parts.len() != 3 {
            return Err(ParseError(format!(
                "pvr_rogue_fwif_sf.h: CREATESFID expects 3 args, got {args:?}"
            )));
        }
        let message: u32 = parts[0].parse().map_err(|e| {
            ParseError(format!(
                "pvr_rogue_fwif_sf.h: bad message number `{}`: {e}",
                parts[0]
            ))
        })?;
        let group_name = parts[1];
        let params: u32 = parts[2].parse().map_err(|e| {
            ParseError(format!(
                "pvr_rogue_fwif_sf.h: bad params count `{}`: {e}",
                parts[2]
            ))
        })?;
        let group = *groups.get(group_name).ok_or_else(|| {
            ParseError(format!(
                "pvr_rogue_fwif_sf.h: unknown group `{group_name}` in stid_fmts"
            ))
        })?;
        if message > 0xffff {
            return Err(ParseError(format!(
                "pvr_rogue_fwif_sf.h: message {message} does not fit 16 bits"
            )));
        }
        if group > 0xf || params > 0xf {
            return Err(ParseError(format!(
                "pvr_rogue_fwif_sf.h: group {group} or params {params} exceeds 4-bit field"
            )));
        }
        let id = message | (u32::from(group) << 12) | (params << 16) | LOG_IDMARKER;
        let (fmt, after_fmt) = parse_c_string_field(after)?;
        entries.push(StidEntry {
            id,
            group,
            params: params as u8,
            message: message as u16,
            fmt,
        });
        rest = after_fmt;
    }

    if entries.is_empty() {
        return Err(ParseError(
            "pvr_rogue_fwif_sf.h: stid_fmts[] contained no CREATESFID entries".into(),
        ));
    }

    // Duplicate ids would make binary search ambiguous and mean the header is
    // no longer a function from id to format — refuse rather than pick one.
    let mut seen = BTreeMap::new();
    for (i, e) in entries.iter().enumerate() {
        if let Some(prev) = seen.insert(e.id, i) {
            return Err(ParseError(format!(
                "pvr_rogue_fwif_sf.h: duplicate stid id {:#010x} at rows {prev} and {i}",
                e.id
            )));
        }
    }

    Ok(entries)
}

/// Parse `enum rogue_fw_log_sfgroups` into name → ordinal.
fn parse_sf_groups(src: &str) -> Result<BTreeMap<String, u8>, ParseError> {
    const MARKER: &str = "enum rogue_fw_log_sfgroups";
    let start = src.find(MARKER).ok_or_else(|| {
        ParseError("pvr_rogue_fwif_sf.h: missing enum rogue_fw_log_sfgroups".into())
    })?;
    let after = &src[start + MARKER.len()..];
    let brace = after
        .find('{')
        .ok_or_else(|| ParseError("pvr_rogue_fwif_sf.h: sfgroups enum has no '{'".into()))?;
    let body_start = &after[brace + 1..];
    let end = body_start
        .find('}')
        .ok_or_else(|| ParseError("pvr_rogue_fwif_sf.h: sfgroups enum has no '}'".into()))?;
    let body = &body_start[..end];

    let mut map = BTreeMap::new();
    let mut next = 0u8;
    for part in body.split(',') {
        let name = part
            .split("//")
            .next()
            .unwrap()
            .split("/*")
            .next()
            .unwrap()
            .trim();
        if name.is_empty() {
            continue;
        }
        let (name, value) = if let Some((n, v)) = name.split_once('=') {
            let n = n.trim();
            let v: u8 = v.trim().parse().map_err(|e| {
                ParseError(format!("pvr_rogue_fwif_sf.h: bad group value for {n}: {e}"))
            })?;
            (n, v)
        } else {
            (name, next)
        };
        if map.insert(name.to_string(), value).is_some() {
            return Err(ParseError(format!(
                "pvr_rogue_fwif_sf.h: duplicate group `{name}`"
            )));
        }
        next = value.saturating_add(1);
    }
    if map.is_empty() {
        return Err(ParseError(
            "pvr_rogue_fwif_sf.h: enum rogue_fw_log_sfgroups is empty".into(),
        ));
    }
    Ok(map)
}

/// Body of `static const struct rogue_km_stid_fmt stid_fmts[] = { ... };`.
fn stid_fmts_body(src: &str) -> Result<&str, ParseError> {
    const MARKER: &str = "stid_fmts[]";
    let start = src
        .find(MARKER)
        .ok_or_else(|| ParseError("pvr_rogue_fwif_sf.h: missing stid_fmts[]".into()))?;
    let after = &src[start + MARKER.len()..];
    let brace = after
        .find('{')
        .ok_or_else(|| ParseError("pvr_rogue_fwif_sf.h: stid_fmts[] has no '{'".into()))?;
    // Match the closing `};` of the array at brace depth zero. Format strings
    // can contain `{` / `}` in theory; the pinned inputs do not, but depth tracking
    // keeps a stray brace in a comment from truncating the table.
    let bytes = after[brace + 1..].as_bytes();
    let mut depth = 1i32;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Ok(&after[brace + 1..brace + 1 + i]);
                }
            }
            b'"' => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\\' {
                        i += 2;
                        continue;
                    }
                    if bytes[i] == b'"' {
                        break;
                    }
                    i += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    Err(ParseError(
        "pvr_rogue_fwif_sf.h: stid_fmts[] has no closing '}'".into(),
    ))
}

/// The `name` field of a `stid_fmts[]` row: `, "printf template"`.
///
/// A row without a string is an error. Silently dropping the template would
/// ship a table that can validate ids but cannot print what the firmware said.
fn parse_c_string_field(s: &str) -> Result<(String, &str), ParseError> {
    let s = s.trim_start();
    let s = s.strip_prefix(',').ok_or_else(|| {
        ParseError("pvr_rogue_fwif_sf.h: CREATESFID not followed by ',' and a string".into())
    })?;
    let s = s.trim_start();
    if !s.starts_with('"') {
        return Err(ParseError(
            "pvr_rogue_fwif_sf.h: stid_fmts row is missing its format string".into(),
        ));
    }
    let bytes = s.as_bytes();
    let mut i = 1usize;
    let mut out = String::new();
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                i += 1;
                let Some(&esc) = bytes.get(i) else {
                    return Err(ParseError(
                        "pvr_rogue_fwif_sf.h: unterminated escape in format string".into(),
                    ));
                };
                match esc {
                    b'n' => out.push('\n'),
                    b't' => out.push('\t'),
                    b'r' => out.push('\r'),
                    b'\\' => out.push('\\'),
                    b'"' => out.push('"'),
                    other => out.push(other as char),
                }
                i += 1;
            }
            b'"' => return Ok((out, &s[i + 1..])),
            c => {
                out.push(c as char);
                i += 1;
            }
        }
    }
    Err(ParseError(
        "pvr_rogue_fwif_sf.h: unterminated format string".into(),
    ))
}

/// Split `(a, b, e)` into the inside and the remainder after the closing `)`.
fn split_paren_args(s: &str) -> Result<(&str, &str), ParseError> {
    debug_assert!(s.starts_with('('));
    let bytes = s.as_bytes();
    let mut depth = 0i32;
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Ok((&s[1..i], &s[i + 1..]));
                }
            }
            _ => {}
        }
    }
    Err(ParseError(
        "pvr_rogue_fwif_sf.h: unclosed CREATESFID argument list".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_minimal_table() {
        let src = r#"
enum rogue_fw_log_sfgroups {
	ROGUE_FW_GROUP_NULL,
	ROGUE_FW_GROUP_MAIN,
};
static const struct rogue_km_stid_fmt stid_fmts[] = {
	{ ROGUE_FW_LOG_CREATESFID(0, ROGUE_FW_GROUP_NULL, 0),
	  "placeholder" },
	{ ROGUE_FW_LOG_CREATESFID(1, ROGUE_FW_GROUP_MAIN, 6),
	  "Kick 3D: %x" },
};
"#;
        let entries = parse_stid_fmts(src).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, 0x7000_0000);
        assert_eq!(entries[0].group, 0);
        assert_eq!(entries[0].params, 0);
        assert_eq!(entries[0].fmt, "placeholder");
        assert_eq!(entries[1].id, 0x7000_1001 | (6 << 16));
        assert_eq!(entries[1].group, 1);
        assert_eq!(entries[1].params, 6);
        assert_eq!(entries[1].message, 1);
        assert_eq!(entries[1].fmt, "Kick 3D: %x");
    }

    #[test]
    fn a_row_without_a_format_string_is_an_error() {
        let src = r#"
enum rogue_fw_log_sfgroups {
	ROGUE_FW_GROUP_NULL,
};
static const struct rogue_km_stid_fmt stid_fmts[] = {
	{ ROGUE_FW_LOG_CREATESFID(0, ROGUE_FW_GROUP_NULL, 0) },
};
"#;
        assert!(parse_stid_fmts(src).is_err());
    }
}
