// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: host-side PowerVR register and firmware ABI generation.

//! Parser for `pvr_rogue_fwif*.h` shared structures and `*_check.h` layout asserts.
//!
//! Completeness: every type named by an upstream `OFFSET_CHECK` / `SIZE_CHECK` /
//! `static_assert(offsetof|sizeof)` must parse and lay out identically. A mismatch
//! against the check file is a generator error — never "fix" the expected number.

use std::collections::{BTreeMap, BTreeSet};

use crate::parse::{ParseError, parse_int};

/// One field inside a struct or union.
#[derive(Clone, Debug)]
pub struct CField {
    pub name: String,
    pub ty: CType,
    /// Element count; `None` means scalar. Flexible array → `Some(0)`.
    pub count: Option<u64>,
    pub align: Option<u32>,
    pub is_bitfield: bool,
    pub bit_width: u32,
}

/// A C type as needed for FWIF layout.
#[derive(Clone, Debug)]
pub enum CType {
    U8,
    U16,
    U32,
    U64,
    Bool,
    /// Host pointer (`u32 *`, …) — LP64 width.
    HostPtr,
    Named(String),
    /// Anonymous nested struct/union lifted to a synthetic name.
    Nested {
        is_union: bool,
        fields: Vec<CField>,
    },
}

impl CType {
    pub fn prim_size_align(&self) -> Option<(u64, u32)> {
        Some(match self {
            CType::U8 | CType::Bool => (1, 1),
            CType::U16 => (2, 2),
            CType::U32 => (4, 4),
            CType::U64 | CType::HostPtr => (8, 8),
            _ => return None,
        })
    }
}

/// A parsed struct (or bitfield-packed word).
#[derive(Clone, Debug)]
pub struct CStruct {
    pub name: String,
    pub fields: Vec<CField>,
    pub align: Option<u32>,
    pub bitfield_word: bool,
}

/// One upstream layout assertion.
#[derive(Clone, Debug)]
pub enum LayoutAssert {
    Offset {
        ty: String,
        member: String,
        offset: u64,
    },
    Size {
        ty: String,
        size: u64,
    },
}

/// Computed layout for one struct.
#[derive(Clone, Debug)]
pub struct Layout {
    pub size: u64,
    pub align: u32,
    pub offsets: BTreeMap<String, u64>,
}

fn align_up(x: u64, a: u32) -> u64 {
    let a = a as u64;
    (x + a - 1) / a * a
}

/// Strip comments, join `\` continuations, drop inactive `#if defined(...)` arms.
pub fn preprocess_fwif(input: &str) -> String {
    let no_comments = strip_c_comments(input);
    let joined = join_continuations(&no_comments);
    strip_conditionals(&joined)
}

fn strip_c_comments(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = i.saturating_add(2);
            continue;
        }
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn join_continuations(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for line in input.lines() {
        let trimmed = line.trim_end();
        if trimmed.ends_with('\\') {
            out.push_str(trimmed.trim_end_matches('\\').trim_end());
            out.push(' ');
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

fn strip_conditionals(input: &str) -> String {
    let mut out = String::new();
    // stack entries: (currently_keeping, else_branch_keep)
    let mut stack: Vec<(bool, bool)> = Vec::new();
    for line in input.lines() {
        let s = line.trim();
        if s.starts_with("#if") || s.starts_with("#ifdef") || s.starts_with("#ifndef") {
            let parent = stack.last().map(|(k, _)| *k).unwrap_or(true);
            let cond = if s.starts_with("#ifndef") {
                true
            } else if s.starts_with("#ifdef") {
                false
            } else if s.starts_with("#if 1") {
                true
            } else {
                // `#if defined(...)`, `#if 0`, unknown → false (open-driver config)
                false
            };
            stack.push((parent && cond, parent && !cond));
            continue;
        }
        if s.starts_with("#elif") {
            if let Some((_, else_keep)) = stack.last_mut() {
                *stack.last_mut().unwrap() = (false, *else_keep);
            }
            continue;
        }
        if s.starts_with("#else") {
            if let Some((_, else_keep)) = stack.last().copied() {
                *stack.last_mut().unwrap() = (else_keep, false);
            }
            continue;
        }
        if s.starts_with("#endif") {
            stack.pop();
            continue;
        }
        if stack.last().map(|(k, _)| !*k).unwrap_or(false) {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Evaluate a restricted constant expression used in array sizes / ALIGN.
pub fn eval_const_expr(expr: &str, env: &BTreeMap<String, u64>) -> Result<u64, ParseError> {
    let mut e = expr.trim().to_string();
    // sizeof(prim) before stripping casts — otherwise `sizeof(u32)` becomes `sizeof`.
    e = replace_sizeof_prim(&e)?;
    // Drop C casts like `(u32)`.
    while let Some(caps) = regex_cast(&e) {
        e = caps;
    }
    // ALIGN(x, a)
    while let Some((before, x, a, after)) = split_align(&e) {
        let xv = eval_const_expr(x.trim(), env)?;
        let av = eval_const_expr(a.trim(), env)?;
        e = format!("{before}{}{after}", align_up(xv, av as u32));
    }
    // BIT(n)
    e = replace_bit(&e)?;
    // Replace identifiers (longest first).
    let mut names: Vec<&String> = env.keys().collect();
    names.sort_by_key(|n| std::cmp::Reverse(n.len()));
    for name in names {
        let mut next = String::new();
        let mut rest = e.as_str();
        while let Some(pos) = rest.find(name.as_str()) {
            let before = &rest[..pos];
            let after = &rest[pos + name.len()..];
            let ok_before = before
                .chars()
                .last()
                .map(|c| !c.is_ascii_alphanumeric() && c != '_')
                .unwrap_or(true);
            let ok_after = after
                .chars()
                .next()
                .map(|c| !c.is_ascii_alphanumeric() && c != '_')
                .unwrap_or(true);
            if ok_before && ok_after {
                next.push_str(before);
                next.push_str(&env[name].to_string());
                rest = after;
            } else {
                next.push_str(before);
                next.push_str(name);
                rest = after;
            }
        }
        next.push_str(rest);
        e = next;
    }
    // Strip integer suffixes.
    e = strip_int_suffixes(&e);
    if !e
        .chars()
        .all(|c| c.is_ascii_hexdigit() || "+-*/%()<>|&~xX ".contains(c))
    {
        return Err(ParseError(format!(
            "unevaluable const expr `{expr}` → `{e}`"
        )));
    }
    // Restrict to a tiny arithmetic subset via a recursive descent.
    let mut p = ExprParser {
        s: e.as_bytes(),
        i: 0,
    };
    let v = p.parse_expr()?;
    p.skip_ws();
    if p.i != p.s.len() {
        return Err(ParseError(format!("trailing junk in `{expr}`")));
    }
    Ok(v)
}

fn regex_cast(e: &str) -> Option<String> {
    let re = [
        "(u8)",
        "(u16)",
        "(u32)",
        "(u64)",
        "(__u32)",
        "(__u64)",
        "(int)",
        "(unsigned)",
    ];
    for c in re {
        if let Some(pos) = e.find(c) {
            let mut out = String::new();
            out.push_str(&e[..pos]);
            out.push_str(&e[pos + c.len()..]);
            return Some(out);
        }
    }
    None
}

fn replace_sizeof_prim(e: &str) -> Result<String, ParseError> {
    let mut out = String::new();
    let mut rest = e;
    while let Some(pos) = rest.find("sizeof") {
        out.push_str(&rest[..pos]);
        let after = rest[pos + 6..].trim_start();
        if !after.starts_with('(') {
            return Err(ParseError("sizeof without (".into()));
        }
        let end = after
            .find(')')
            .ok_or_else(|| ParseError("sizeof(".into()))?;
        let inner = after[1..end].trim();
        let sz = match inner {
            "u8" | "char" | "bool" => 1u64,
            "u16" => 2,
            "u32" | "__u32" | "int" | "unsigned int" | "s32" => 4,
            "u64" | "__u64" | "aligned_u64" => 8,
            other => {
                return Err(ParseError(format!("sizeof({other}) unsupported")));
            }
        };
        out.push_str(&sz.to_string());
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn split_align(e: &str) -> Option<(String, String, String, String)> {
    let pos = e.find("ALIGN")?;
    let open = e[pos..].find('(')? + pos;
    let mut depth = 0usize;
    let mut comma = None;
    for (i, c) in e[open..].char_indices() {
        let i = open + i;
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    let cpos = comma?;
                    let x = e[open + 1..cpos].to_string();
                    let a = e[cpos + 1..i].to_string();
                    return Some((e[..pos].to_string(), x, a, e[i + 1..].to_string()));
                }
            }
            ',' if depth == 1 && comma.is_none() => comma = Some(i),
            _ => {}
        }
    }
    None
}

fn replace_bit(e: &str) -> Result<String, ParseError> {
    let mut out = String::new();
    let mut rest = e;
    while let Some(pos) = rest.find("BIT") {
        // word boundary-ish
        let before_ok = pos == 0 || !rest[..pos].chars().last().unwrap().is_ascii_alphanumeric();
        if !before_ok {
            out.push_str(&rest[..=pos]);
            rest = &rest[pos + 1..];
            continue;
        }
        out.push_str(&rest[..pos]);
        let after = rest[pos + 3..].trim_start();
        if !after.starts_with('(') {
            out.push_str("BIT");
            rest = &rest[pos + 3..];
            continue;
        }
        let end = after.find(')').ok_or_else(|| ParseError("BIT(".into()))?;
        let n: u32 = after[1..end]
            .trim()
            .parse()
            .map_err(|err| ParseError(format!("BIT: {err}")))?;
        out.push_str(&(1u64 << n).to_string());
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn strip_int_suffixes(e: &str) -> String {
    let mut out = String::with_capacity(e.len());
    let b = e.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_digit()
            || (b[i] == b'0' && i + 1 < b.len() && (b[i + 1] == b'x' || b[i + 1] == b'X'))
        {
            // copy number
            if b[i] == b'0' && i + 1 < b.len() && (b[i + 1] == b'x' || b[i + 1] == b'X') {
                out.push(b[i] as char);
                out.push(b[i + 1] as char);
                i += 2;
                while i < b.len() && b[i].is_ascii_hexdigit() {
                    out.push(b[i] as char);
                    i += 1;
                }
            } else {
                while i < b.len() && b[i].is_ascii_digit() {
                    out.push(b[i] as char);
                    i += 1;
                }
            }
            // skip U/ULL/LL
            if i + 2 < b.len()
                && (b[i] == b'U' || b[i] == b'u')
                && (b[i + 1] == b'L' || b[i + 1] == b'l')
                && (b[i + 2] == b'L' || b[i + 2] == b'l')
            {
                i += 3;
            } else if i + 1 < b.len()
                && (b[i] == b'L' || b[i] == b'l')
                && (b[i + 1] == b'L' || b[i + 1] == b'l')
            {
                i += 2;
            } else if i < b.len() && (b[i] == b'U' || b[i] == b'u') {
                i += 1;
            }
            continue;
        }
        out.push(b[i] as char);
        i += 1;
    }
    out
}

struct ExprParser<'a> {
    s: &'a [u8],
    i: usize,
}

impl<'a> ExprParser<'a> {
    fn skip_ws(&mut self) {
        while self.i < self.s.len() && self.s[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }
    fn parse_expr(&mut self) -> Result<u64, ParseError> {
        self.parse_or()
    }
    fn parse_or(&mut self) -> Result<u64, ParseError> {
        let mut v = self.parse_and()?;
        loop {
            self.skip_ws();
            if self.i + 1 < self.s.len() && self.s[self.i] == b'|' && self.s[self.i + 1] != b'|' {
                self.i += 1;
                v |= self.parse_and()?;
            } else {
                break;
            }
        }
        Ok(v)
    }
    fn parse_and(&mut self) -> Result<u64, ParseError> {
        let mut v = self.parse_shift()?;
        loop {
            self.skip_ws();
            if self.i + 1 < self.s.len() && self.s[self.i] == b'&' && self.s[self.i + 1] != b'&' {
                self.i += 1;
                v &= self.parse_shift()?;
            } else {
                break;
            }
        }
        Ok(v)
    }
    fn parse_shift(&mut self) -> Result<u64, ParseError> {
        let mut v = self.parse_add()?;
        loop {
            self.skip_ws();
            if self.i + 1 < self.s.len() && self.s[self.i] == b'<' && self.s[self.i + 1] == b'<' {
                self.i += 2;
                v <<= self.parse_add()?;
            } else if self.i + 1 < self.s.len()
                && self.s[self.i] == b'>'
                && self.s[self.i + 1] == b'>'
            {
                self.i += 2;
                v >>= self.parse_add()?;
            } else {
                break;
            }
        }
        Ok(v)
    }
    fn parse_add(&mut self) -> Result<u64, ParseError> {
        let mut v = self.parse_mul()?;
        loop {
            self.skip_ws();
            if self.i < self.s.len() && self.s[self.i] == b'+' {
                self.i += 1;
                v = v.wrapping_add(self.parse_mul()?);
            } else if self.i < self.s.len() && self.s[self.i] == b'-' {
                self.i += 1;
                v = v.wrapping_sub(self.parse_mul()?);
            } else {
                break;
            }
        }
        Ok(v)
    }
    fn parse_mul(&mut self) -> Result<u64, ParseError> {
        let mut v = self.parse_unary()?;
        loop {
            self.skip_ws();
            if self.i < self.s.len() && self.s[self.i] == b'*' {
                self.i += 1;
                v = v.wrapping_mul(self.parse_unary()?);
            } else if self.i < self.s.len() && self.s[self.i] == b'/' {
                self.i += 1;
                let d = self.parse_unary()?;
                if d == 0 {
                    return Err(ParseError("div by zero".into()));
                }
                v /= d;
            } else if self.i < self.s.len() && self.s[self.i] == b'%' {
                self.i += 1;
                let d = self.parse_unary()?;
                if d == 0 {
                    return Err(ParseError("mod by zero".into()));
                }
                v %= d;
            } else {
                break;
            }
        }
        Ok(v)
    }
    fn parse_unary(&mut self) -> Result<u64, ParseError> {
        self.skip_ws();
        if self.i < self.s.len() && self.s[self.i] == b'~' {
            self.i += 1;
            return Ok(!self.parse_unary()?);
        }
        if self.i < self.s.len() && self.s[self.i] == b'-' {
            self.i += 1;
            return Ok(self.parse_unary()?.wrapping_neg());
        }
        self.parse_primary()
    }
    fn parse_primary(&mut self) -> Result<u64, ParseError> {
        self.skip_ws();
        if self.i < self.s.len() && self.s[self.i] == b'(' {
            self.i += 1;
            let v = self.parse_expr()?;
            self.skip_ws();
            if self.i >= self.s.len() || self.s[self.i] != b')' {
                return Err(ParseError("expected )".into()));
            }
            self.i += 1;
            return Ok(v);
        }
        let start = self.i;
        if self.i + 1 < self.s.len()
            && self.s[self.i] == b'0'
            && (self.s[self.i + 1] == b'x' || self.s[self.i + 1] == b'X')
        {
            self.i += 2;
            while self.i < self.s.len() && self.s[self.i].is_ascii_hexdigit() {
                self.i += 1;
            }
        } else {
            while self.i < self.s.len() && self.s[self.i].is_ascii_digit() {
                self.i += 1;
            }
        }
        if start == self.i {
            return Err(ParseError(format!(
                "expected number at {}",
                String::from_utf8_lossy(&self.s[self.i..])
            )));
        }
        parse_int(std::str::from_utf8(&self.s[start..self.i]).unwrap())
    }
}

/// Collect `#define NAME <expr>` integers and simple enum constants.
pub fn collect_fwif_env(texts: &[&str]) -> Result<BTreeMap<String, u64>, ParseError> {
    let mut env = BTreeMap::new();
    // Seed values that appear as trailing enum enumerators without `=`.
    // Verified against upstream enum order at the UPSTREAM commit.
    env.insert("ROGUE_FWIF_TPU_DM_LAST".into(), 4);
    env.insert("ROGUE_FWIF_REG_CFG_TYPE_ALL".into(), 7);

    for _ in 0..40 {
        let mut progressed = false;
        for text in texts {
            for line in text.lines() {
                let line = line.trim();
                if !line.starts_with("#define") {
                    continue;
                }
                let rest = line["#define".len()..].trim_start();
                let mut parts = rest.splitn(2, char::is_whitespace);
                let Some(name) = parts.next() else { continue };
                if name.contains('(') {
                    continue;
                }
                let Some(val) = parts.next() else { continue };
                let val = val.trim();
                if env.contains_key(name) || val.contains('"') {
                    continue;
                }
                if let Ok(v) = eval_const_expr(val, &env) {
                    env.insert(name.to_string(), v);
                    progressed = true;
                } else if let Ok(v) = parse_int(val.split_whitespace().next().unwrap_or("")) {
                    env.insert(name.to_string(), v);
                    progressed = true;
                }
            }
        }
        if !progressed {
            break;
        }
    }

    for text in texts {
        parse_simple_enums(text, &mut env);
    }
    Ok(env)
}

fn parse_simple_enums(text: &str, env: &mut BTreeMap<String, u64>) {
    let mut search = text;
    while let Some(pos) = search.find("enum ") {
        let after = &search[pos + 5..];
        let brace = match after.find('{') {
            Some(b) => b,
            None => break,
        };
        let body_start = &after[brace + 1..];
        let end = match body_start.find('}') {
            Some(e) => e,
            None => break,
        };
        let body = &body_start[..end];
        let mut next = 0u64;
        for part in body.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            if let Some((name, val)) = part.split_once('=') {
                let name = name.trim();
                if !is_ident(name) {
                    continue;
                }
                if let Ok(v) = eval_const_expr(val.trim(), env) {
                    env.insert(name.to_string(), v);
                    // Flag-style enums use `= ~0U` etc.; don't advance into overflow.
                    next = v.saturating_add(1);
                }
            } else {
                let name = part.split_whitespace().next().unwrap_or("");
                if is_ident(name) {
                    env.entry(name.to_string()).or_insert(next);
                    next = next.saturating_add(1);
                }
            }
        }
        search = &body_start[end + 1..];
    }
}

fn is_ident(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {
            chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        }
        _ => false,
    }
}

/// Parse all `struct name { ... }` definitions from preprocessed text.
pub fn parse_structs(
    text: &str,
    env: &BTreeMap<String, u64>,
) -> Result<BTreeMap<String, CStruct>, ParseError> {
    let mut out = BTreeMap::new();
    let mut search = text;
    while let Some(pos) = find_struct_kw(search) {
        let after = &search[pos..];
        let rest = after.trim_start_matches("struct").trim_start();
        let name_end = rest
            .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .unwrap_or(rest.len());
        let name = &rest[..name_end];
        let after_name = rest[name_end..].trim_start();
        if !after_name.starts_with('{') {
            search = &rest[name_end..];
            continue;
        }
        let (body, after_body) = extract_brace_body(after_name)?;
        let align = parse_trailing_aligned(after_body);
        let fields = parse_field_list(&body, env).map_err(|e| {
            ParseError(format!(
                "struct {name}: {e} (near `{}`)",
                body.chars()
                    .take(120)
                    .collect::<String>()
                    .replace('\n', " ")
            ))
        })?;
        let bitfield_word = !fields.is_empty() && fields.iter().all(|f| f.is_bitfield);
        out.insert(
            name.to_string(),
            CStruct {
                name: name.to_string(),
                fields,
                align,
                bitfield_word,
            },
        );
        // advance
        let consumed = (after.as_ptr() as usize + after.len()) - (after_body.as_ptr() as usize);
        let _ = consumed;
        search = after_body;
    }
    Ok(out)
}

fn find_struct_kw(s: &str) -> Option<usize> {
    let mut i = 0;
    while let Some(rel) = s[i..].find("struct") {
        let pos = i + rel;
        let before_ok = pos == 0 || !s[..pos].chars().last().unwrap().is_ascii_alphanumeric();
        let after = &s[pos + 6..];
        let after_ok = after
            .chars()
            .next()
            .map(|c| c.is_ascii_whitespace() || c == '{')
            .unwrap_or(false);
        if before_ok && after_ok {
            return Some(pos);
        }
        i = pos + 6;
    }
    None
}

fn extract_brace_body(s: &str) -> Result<(String, &str), ParseError> {
    let s = s.trim_start();
    if !s.starts_with('{') {
        return Err(ParseError("expected {".into()));
    }
    let bytes = s.as_bytes();
    let mut depth = 0usize;
    for (i, &c) in bytes.iter().enumerate() {
        match c {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Ok((s[1..i].to_string(), &s[i + 1..]));
                }
            }
            _ => {}
        }
    }
    Err(ParseError("unclosed {".into()))
}

fn parse_trailing_aligned(after: &str) -> Option<u32> {
    let t = after.trim_start();
    if let Some(rest) = t.strip_prefix("__aligned") {
        let rest = rest.trim_start();
        if let Some(rest) = rest.strip_prefix('(') {
            let end = rest.find(')')?;
            return rest[..end].trim().parse().ok();
        }
    }
    None
}

fn parse_field_list(body: &str, env: &BTreeMap<String, u64>) -> Result<Vec<CField>, ParseError> {
    // Drop #define lines left inside struct bodies (constants already in env).
    let mut cleaned = String::new();
    for line in body.lines() {
        if line.trim_start().starts_with("#define") {
            continue;
        }
        cleaned.push_str(line);
        cleaned.push('\n');
    }
    let mut fields = Vec::new();
    let mut rest = cleaned.as_str();
    let mut anon = 0usize;
    while let Some(next) = skip_ws_str(rest) {
        rest = next;
        if rest.is_empty() {
            break;
        }
        // nested struct/union {
        if rest.starts_with("struct") || rest.starts_with("union") {
            let is_union = rest.starts_with("union");
            let after_kw = if is_union {
                rest["union".len()..].trim_start()
            } else {
                rest["struct".len()..].trim_start()
            };
            if after_kw.starts_with('{') {
                let (inner, after) = extract_brace_body(after_kw)?;
                let nested = parse_field_list(&inner, env)?;
                let (name, count, align, after2) = parse_declarator_tail(after, env, &mut anon)?;
                fields.push(CField {
                    name,
                    ty: CType::Nested {
                        is_union,
                        fields: nested,
                    },
                    count,
                    align,
                    is_bitfield: false,
                    bit_width: 0,
                });
                rest = after2;
                continue;
            }
        }
        // bitfield
        if let Some((f, after)) = try_parse_bitfield(rest)? {
            fields.push(f);
            rest = after;
            continue;
        }
        // pointer
        if let Some((f, after)) = try_parse_pointer(rest)? {
            fields.push(f);
            rest = after;
            continue;
        }
        // enum Type name...
        if rest.starts_with("enum ") {
            let after = rest["enum ".len()..].trim_start();
            let (ty_name, after) = take_ident(after)?;
            let _ = ty_name;
            let (name, count, align, after) = parse_declarator_tail(after, env, &mut anon)?;
            fields.push(CField {
                name,
                ty: CType::U32,
                count,
                align,
                is_bitfield: false,
                bit_width: 0,
            });
            rest = after;
            continue;
        }
        // struct Named name...
        if rest.starts_with("struct ") {
            let after = rest["struct ".len()..].trim_start();
            let (ty_name, after) = take_ident(after)?;
            let (name, count, align, after) = parse_declarator_tail(after, env, &mut anon)?;
            fields.push(CField {
                name,
                ty: CType::Named(ty_name.to_string()),
                count,
                align,
                is_bitfield: false,
                bit_width: 0,
            });
            rest = after;
            continue;
        }
        // prim name...
        if let Some((ty, after)) = try_take_prim(rest) {
            let (name, count, align, after) = parse_declarator_tail(after, env, &mut anon)?;
            fields.push(CField {
                name,
                ty,
                count,
                align,
                is_bitfield: false,
                bit_width: 0,
            });
            rest = after;
            continue;
        }
        return Err(ParseError(format!(
            "unparsed field near `{}`",
            rest.chars().take(80).collect::<String>()
        )));
    }
    Ok(fields)
}

fn skip_ws_str(s: &str) -> Option<&str> {
    let t = s.trim_start();
    if t.is_empty() { None } else { Some(t) }
}

fn take_ident(s: &str) -> Result<(&str, &str), ParseError> {
    let s = s.trim_start();
    let end = s
        .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .unwrap_or(s.len());
    if end == 0 {
        return Err(ParseError("expected ident".into()));
    }
    Ok((&s[..end], &s[end..]))
}

fn try_take_prim(s: &str) -> Option<(CType, &str)> {
    let patterns: &[(&str, CType)] = &[
        ("aligned_u64", CType::U64),
        ("unsigned int", CType::U32),
        ("__u64", CType::U64),
        ("__u32", CType::U32),
        ("bool", CType::Bool),
        ("char", CType::U8),
        ("u64", CType::U64),
        ("u32", CType::U32),
        ("u16", CType::U16),
        ("u8", CType::U8),
        ("s32", CType::U32),
        ("int", CType::U32),
    ];
    for (p, ty) in patterns {
        if let Some(rest) = s.strip_prefix(p) {
            if rest
                .chars()
                .next()
                .map(|c| c.is_ascii_whitespace() || c == '*')
                .unwrap_or(false)
            {
                return Some((ty.clone(), rest));
            }
        }
    }
    None
}

fn try_parse_bitfield(s: &str) -> Result<Option<(CField, &str)>, ParseError> {
    let s0 = s.trim_start();
    let after_ty = if let Some(r) = s0.strip_prefix("unsigned int") {
        r
    } else if let Some(r) = s0.strip_prefix("u32") {
        r
    } else if let Some(r) = s0.strip_prefix("__u32") {
        r
    } else {
        return Ok(None);
    };
    if !after_ty
        .chars()
        .next()
        .map(|c| c.is_ascii_whitespace())
        .unwrap_or(false)
    {
        return Ok(None);
    }
    // `u32 *ptr` is a pointer, not a bitfield — bail before requiring an ident.
    let after_ws = after_ty.trim_start();
    if after_ws.starts_with('*') {
        return Ok(None);
    }
    let Ok((name, after)) = take_ident(after_ws) else {
        return Ok(None);
    };
    let after = after.trim_start();
    if !after.starts_with(':') {
        return Ok(None);
    }
    let after = after[1..].trim_start();
    let end = after
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(after.len());
    let width: u32 = after[..end]
        .parse()
        .map_err(|e| ParseError(format!("bit width: {e}")))?;
    let after = after[end..].trim_start();
    if !after.starts_with(';') {
        return Err(ParseError("bitfield missing ;".into()));
    }
    Ok(Some((
        CField {
            name: name.to_string(),
            ty: CType::U32,
            count: None,
            align: None,
            is_bitfield: true,
            bit_width: width,
        },
        &after[1..],
    )))
}

fn try_parse_pointer(s: &str) -> Result<Option<(CField, &str)>, ParseError> {
    let s0 = s.trim_start();
    // const? type *
    let after = s0.strip_prefix("const ").unwrap_or(s0);
    let after = if let Some(r) = after.strip_prefix("u32") {
        r
    } else if let Some(r) = after.strip_prefix("char") {
        r
    } else if let Some(r) = after.strip_prefix("void") {
        r
    } else if let Some(r) = after.strip_prefix("struct") {
        let r = r.trim_start();
        let (_, r) = take_ident(r)?;
        r
    } else {
        return Ok(None);
    };
    let after = after.trim_start();
    if !after.starts_with('*') {
        return Ok(None);
    }
    let after = after.trim_start_matches('*').trim_start();
    let (name, after) = take_ident(after)?;
    let after = after.trim_start();
    if !after.starts_with(';') {
        return Err(ParseError("pointer missing ;".into()));
    }
    Ok(Some((
        CField {
            name: name.to_string(),
            ty: CType::HostPtr,
            count: None,
            align: None,
            is_bitfield: false,
            bit_width: 0,
        },
        &after[1..],
    )))
}

fn parse_declarator_tail<'a>(
    s: &'a str,
    env: &BTreeMap<String, u64>,
    anon: &mut usize,
) -> Result<(String, Option<u64>, Option<u32>, &'a str), ParseError> {
    let s = s.trim_start();
    let (name, s) = if s.starts_with('[') || s.starts_with(';') || s.starts_with("__aligned") {
        let n = format!("__anon{anon}");
        *anon += 1;
        (n, s)
    } else {
        let (n, rest) = take_ident(s)?;
        (n.to_string(), rest)
    };
    let mut s = s.trim_start();
    let mut dims: Vec<u64> = Vec::new();
    let mut flex = false;
    while s.starts_with('[') {
        let end = s.find(']').ok_or_else(|| ParseError("array ]".into()))?;
        let inner = s[1..end].trim();
        if inner.is_empty() {
            flex = true;
            dims.clear();
            s = &s[end + 1..];
            break;
        }
        dims.push(eval_const_expr(inner, env)?);
        s = s[end + 1..].trim_start();
    }
    let mut align = None;
    if let Some(rest) = s.strip_prefix("__aligned") {
        let rest = rest.trim_start();
        let rest = rest
            .strip_prefix('(')
            .ok_or_else(|| ParseError("__aligned(".into()))?;
        let end = rest
            .find(')')
            .ok_or_else(|| ParseError("__aligned)".into()))?;
        align = Some(
            rest[..end]
                .trim()
                .parse()
                .map_err(|e| ParseError(format!("align: {e}")))?,
        );
        s = rest[end + 1..].trim_start();
    }
    if !s.starts_with(';') {
        return Err(ParseError(format!(
            "expected ; after declarator `{name}`, got `{}`",
            s.chars().take(40).collect::<String>()
        )));
    }
    let count = if flex {
        Some(0)
    } else if dims.is_empty() {
        None
    } else {
        Some(dims.iter().product())
    };
    Ok((name, count, align, &s[1..]))
}

/// Parse all layout asserts from check headers.
pub fn parse_layout_asserts(texts: &[(String, String)]) -> Result<Vec<LayoutAssert>, ParseError> {
    let mut out = Vec::new();
    for (_name, text) in texts {
        let text = preprocess_fwif(text);
        parse_offset_size_macros(&text, &mut out)?;
        parse_static_asserts(&text, &mut out)?;
    }
    Ok(out)
}

fn parse_offset_size_macros(text: &str, out: &mut Vec<LayoutAssert>) -> Result<(), ParseError> {
    // Collapse whitespace inside invocations by scanning.
    let mut i = 0;
    let b = text.as_bytes();
    while i < b.len() {
        if text[i..].starts_with("OFFSET_CHECK") {
            let open = text[i..]
                .find('(')
                .ok_or_else(|| ParseError("OFFSET_CHECK(".into()))?
                + i;
            let (args, end) = extract_paren_args(&text[open..])?;
            let parts = split_args(&args);
            if parts.len() >= 3 {
                let ty = parts[0].replace("struct ", "").trim().to_string();
                if ty != "type" {
                    let off = parse_int(parts.last().unwrap())?;
                    let member = parts[1..parts.len() - 1].join(",").trim().to_string();
                    out.push(LayoutAssert::Offset {
                        ty,
                        member,
                        offset: off,
                    });
                }
            }
            i = open + end;
            continue;
        }
        if text[i..].starts_with("SIZE_CHECK") {
            let open = text[i..]
                .find('(')
                .ok_or_else(|| ParseError("SIZE_CHECK(".into()))?
                + i;
            let (args, end) = extract_paren_args(&text[open..])?;
            let parts = split_args(&args);
            if parts.len() >= 2 {
                let ty = parts[0].replace("struct ", "").trim().to_string();
                if ty != "type" {
                    out.push(LayoutAssert::Size {
                        ty,
                        size: parse_int(parts[1].trim())?,
                    });
                }
            }
            i = open + end;
            continue;
        }
        i += 1;
    }
    Ok(())
}

fn parse_static_asserts(text: &str, out: &mut Vec<LayoutAssert>) -> Result<(), ParseError> {
    // After preprocess, mips_check uses one-line static_assert(...).
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("static_assert(offsetof(struct ") {
            let (ty, rest) = rest
                .split_once(',')
                .ok_or_else(|| ParseError("offsetof ty".into()))?;
            let rest = rest.trim();
            let (member, rest) = rest
                .split_once(')')
                .ok_or_else(|| ParseError("offsetof member".into()))?;
            let rest = rest.trim();
            let rest = rest
                .strip_prefix("==")
                .ok_or_else(|| ParseError("offsetof ==".into()))?
                .trim();
            let num_end = rest
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(rest.len());
            let offset = parse_int(&rest[..num_end])?;
            out.push(LayoutAssert::Offset {
                ty: ty.trim().to_string(),
                member: member.trim().to_string(),
                offset,
            });
        } else if let Some(rest) = line.strip_prefix("static_assert(sizeof(struct ") {
            let (ty, rest) = rest
                .split_once(')')
                .ok_or_else(|| ParseError("sizeof ty".into()))?;
            let rest = rest.trim();
            let rest = rest
                .strip_prefix("==")
                .ok_or_else(|| ParseError("sizeof ==".into()))?
                .trim();
            let num_end = rest
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(rest.len());
            let size = parse_int(&rest[..num_end])?;
            out.push(LayoutAssert::Size {
                ty: ty.trim().to_string(),
                size,
            });
        }
    }
    Ok(())
}

fn extract_paren_args(s: &str) -> Result<(String, usize), ParseError> {
    // s starts with '('
    let bytes = s.as_bytes();
    if bytes.first() != Some(&b'(') {
        return Err(ParseError("expected (".into()));
    }
    let mut depth = 0usize;
    for (i, &c) in bytes.iter().enumerate() {
        match c {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Ok((s[1..i].to_string(), i + 1));
                }
            }
            _ => {}
        }
    }
    Err(ParseError("unclosed (".into()))
}

fn split_args(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut depth = 0i32;
    for c in s.chars() {
        match c {
            '(' | '[' => {
                depth += 1;
                cur.push(c);
            }
            ')' | ']' => {
                depth -= 1;
                cur.push(c);
            }
            ',' if depth == 0 => {
                parts.push(cur.trim().to_string());
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        parts.push(cur.trim().to_string());
    }
    parts
}

/// Types named by layout asserts (the allow-list).
pub fn asserted_types(asserts: &[LayoutAssert]) -> BTreeSet<String> {
    let mut s = BTreeSet::new();
    for a in asserts {
        match a {
            LayoutAssert::Offset { ty, .. } | LayoutAssert::Size { ty, .. } => {
                s.insert(ty.clone());
            }
        }
    }
    s
}

/// Lay out a struct under LP64 / Linux C ABI rules used by the upstream checks.
pub fn layout_struct(
    name: &str,
    structs: &BTreeMap<String, CStruct>,
    cache: &mut BTreeMap<String, Layout>,
) -> Result<Layout, ParseError> {
    if let Some(l) = cache.get(name) {
        return Ok(l.clone());
    }
    let st = structs
        .get(name)
        .ok_or_else(|| ParseError(format!("struct `{name}` not parsed")))?;
    let lay = if st.bitfield_word {
        let mut offsets = BTreeMap::new();
        for f in &st.fields {
            offsets.insert(f.name.clone(), 0);
        }
        Layout {
            size: 4,
            align: 4,
            offsets,
        }
    } else {
        layout_fields(&st.fields, false, st.align, structs, cache)?
    };
    cache.insert(name.to_string(), lay.clone());
    Ok(lay)
}

fn layout_fields(
    fields: &[CField],
    is_union: bool,
    struct_align: Option<u32>,
    structs: &BTreeMap<String, CStruct>,
    cache: &mut BTreeMap<String, Layout>,
) -> Result<Layout, ParseError> {
    let mut offsets = BTreeMap::new();
    let mut off = 0u64;
    let mut max_align = 1u32;
    let mut max_end = 0u64;
    let mut bf_unit: Option<u64> = None;
    let mut bf_bits = 0u32;

    for f in fields {
        if f.is_bitfield {
            if bf_unit.is_none() || bf_bits + f.bit_width > 32 {
                off = align_up(off, 4);
                bf_unit = Some(off);
                bf_bits = 0;
                off += 4;
            }
            offsets.insert(f.name.clone(), bf_unit.unwrap());
            bf_bits += f.bit_width;
            max_align = max_align.max(4);
            max_end = max_end.max(off);
            continue;
        }
        bf_unit = None;
        bf_bits = 0;

        let (elem_size, mut elem_align, nested) = type_size_align(&f.ty, structs, cache)?;
        if let Some(a) = f.align {
            elem_align = elem_align.max(a);
        }
        let count = f.count.unwrap_or(1);
        let total = elem_size.saturating_mul(count);

        if is_union {
            offsets.insert(f.name.clone(), 0);
            if let Some(n) = &nested {
                for (k, v) in &n.offsets {
                    offsets.insert(format!("{}.{k}", f.name), *v);
                }
                if f.name.starts_with("__anon") {
                    if let CType::Nested {
                        is_union: true,
                        fields,
                    } = &f.ty
                    {
                        for nf in fields {
                            offsets.insert(nf.name.clone(), 0);
                        }
                    }
                }
            }
            max_end = max_end.max(if total == 0 { elem_size } else { total });
            max_align = max_align.max(elem_align);
        } else {
            off = align_up(off, elem_align);
            offsets.insert(f.name.clone(), off);
            if let Some(n) = &nested {
                for (k, v) in &n.offsets {
                    offsets.insert(format!("{}.{k}", f.name), off + *v);
                }
                if count > 1 {
                    for idx in 0..count {
                        for (k, v) in &n.offsets {
                            offsets.insert(
                                format!("{}[{idx}].{k}", f.name),
                                off + idx * elem_size + *v,
                            );
                        }
                    }
                }
                if f.name.starts_with("__anon") {
                    if let CType::Nested {
                        is_union: true,
                        fields,
                    } = &f.ty
                    {
                        for nf in fields {
                            offsets.insert(nf.name.clone(), off);
                        }
                    }
                }
            }
            off += total;
            max_end = max_end.max(off);
            max_align = max_align.max(elem_align);
        }
    }
    if let Some(a) = struct_align {
        max_align = max_align.max(a);
    }
    Ok(Layout {
        size: align_up(max_end, max_align),
        align: max_align,
        offsets,
    })
}

fn type_size_align(
    ty: &CType,
    structs: &BTreeMap<String, CStruct>,
    cache: &mut BTreeMap<String, Layout>,
) -> Result<(u64, u32, Option<Layout>), ParseError> {
    if let Some((s, a)) = ty.prim_size_align() {
        return Ok((s, a, None));
    }
    match ty {
        CType::Named(n) => {
            let lay = layout_struct(n, structs, cache)?;
            Ok((lay.size, lay.align, None))
        }
        CType::Nested { is_union, fields } => {
            let lay = layout_fields(fields, *is_union, None, structs, cache)?;
            Ok((lay.size, lay.align, Some(lay)))
        }
        _ => Err(ParseError("bad type".into())),
    }
}

/// Verify computed layouts against upstream asserts. Returns (ok, failures).
pub fn verify_asserts(
    asserts: &[LayoutAssert],
    structs: &BTreeMap<String, CStruct>,
) -> Result<(), ParseError> {
    let mut cache = BTreeMap::new();
    let mut failures = Vec::new();
    for a in asserts {
        match a {
            LayoutAssert::Offset { ty, member, offset } => {
                let lay = match layout_struct(ty, structs, &mut cache) {
                    Ok(l) => l,
                    Err(e) => {
                        failures.push(format!("{ty}.{member}: {e}"));
                        continue;
                    }
                };
                match lay.offsets.get(member) {
                    Some(got) if got == offset => {}
                    Some(got) => failures.push(format!(
                        "{ty}.{member}: layout {got} != upstream assert {offset}"
                    )),
                    None => failures.push(format!(
                        "{ty}.{member}: no such field in layout (assert offset {offset})"
                    )),
                }
            }
            LayoutAssert::Size { ty, size } => {
                let lay = match layout_struct(ty, structs, &mut cache) {
                    Ok(l) => l,
                    Err(e) => {
                        failures.push(format!("{ty} size: {e}"));
                        continue;
                    }
                };
                if lay.size != *size {
                    failures.push(format!(
                        "{ty}: layout size {} != upstream assert {size}",
                        lay.size
                    ));
                }
            }
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        let n = failures.len();
        let head: Vec<_> = failures.into_iter().take(12).collect();
        Err(ParseError(format!(
            "{n} layout assert failure(s); first: {}",
            head.join(" | ")
        )))
    }
}

/// Closure of structs needed to emit `roots` (asserted types).
pub fn dependency_closure(
    roots: &BTreeSet<String>,
    structs: &BTreeMap<String, CStruct>,
) -> BTreeSet<String> {
    let mut need = roots.clone();
    let mut changed = true;
    while changed {
        changed = false;
        for name in need.clone() {
            if let Some(st) = structs.get(&name) {
                for f in &st.fields {
                    collect_type_names(&f.ty, &mut need, &mut changed);
                }
            }
        }
    }
    need
}

fn collect_type_names(ty: &CType, need: &mut BTreeSet<String>, changed: &mut bool) {
    match ty {
        CType::Named(n) => {
            if need.insert(n.clone()) {
                *changed = true;
            }
        }
        CType::Nested { fields, .. } => {
            for f in fields {
                collect_type_names(&f.ty, need, changed);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod sizeof_tests {
    use super::*;
    #[test]
    fn sizeof_u32_in_array_dim() {
        let env = BTreeMap::new();
        assert_eq!(eval_const_expr("128 - sizeof(u32)", &env).unwrap(), 124);
    }
}
