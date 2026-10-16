// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: parse `k1.diag=` and gate periodic console breadcrumbs.

//! `k1.diag=` — which periodic console breadcrumbs are on.
//!
//! Periodic console diagnostics are off by default to preserve serial-console
//! bandwidth and interactive use. Enable them by tag:
//!
//! ```text
//! k1.diag=all                 every periodic breadcrumb
//! k1.diag=therm,batt          just those two
//! k1.diag=off                 none (same as leaving it out)
//! ```
//!
//! Tags are interpreted by their consumers. Retained diagnostics include
//! thermal/battery/PMIC/RTC status and `proc` (successful/failed exec,
//! abnormal exit, and ENOSYS) plus `stderr` (fd 2 writes, 512 KiB budget).
//!
//! Hardware cursor selection remains the separate `k1.cursorplane=`
//! parameter and is independent of this diagnostic list.
//!
//! This gates *periodic* status and the two syscall tees above. One-shot
//! bring-up lines, warnings, and errors are not behind it.
//!
//! A separate knob, `k1.splash=off|quiet|log` (default `log`), controls the
//! kernel boot splash on the K1 eDP independently: `off` disables it,
//! `quiet` shows only progress, and `log` also mirrors kernel messages.
//!
//! Read straight off the boot command line rather than only through the
//! parameter registry: some emitters live in the kernel crate and can run
//! before the cmdline component has dispatched.

use alloc::{string::String, vec::Vec};

use spin::Once;

use crate::define_kv_param;

static SPEC: Once<String> = Once::new();
define_kv_param!("k1.diag", SPEC);

enum Spec {
    Off,
    All,
    Tags(Vec<String>),
}

static PARSED: Once<Spec> = Once::new();

/// Whether the periodic breadcrumb `tag` (e.g. `"therm"`) should print.
pub fn diag_enabled(tag: &str) -> bool {
    let spec = PARSED.call_once(|| parse(raw_spec().as_deref()));
    match spec {
        Spec::Off => false,
        Spec::All => true,
        Spec::Tags(tags) => tags.iter().any(|t| t == tag),
    }
}

fn raw_spec() -> Option<String> {
    if let Some(s) = SPEC.get() {
        return Some(s.clone());
    }
    raw_spec_from_cmdline(ostd::boot::boot_info().kernel_cmdline.as_str()).map(String::from)
}

fn raw_spec_from_cmdline(cmdline: &str) -> Option<&str> {
    crate::dispatch::split_arg(cmdline)
        .take_while(|arg| *arg != "--")
        .filter_map(|arg| arg.strip_prefix("k1.diag="))
        .last()
}

fn parse(raw: Option<&str>) -> Spec {
    let Some(raw) = raw else {
        return Spec::Off;
    };
    let raw = raw
        .strip_prefix('"')
        .and_then(|raw| raw.strip_suffix('"'))
        .unwrap_or(raw);
    let mut tags = Vec::new();
    for tag in raw.split(',').map(str::trim).filter(|t| !t.is_empty()) {
        match tag {
            "all" | "on" | "1" | "yes" | "true" => return Spec::All,
            "off" | "none" | "0" | "no" | "false" => return Spec::Off,
            other => tags.push(String::from(other)),
        }
    }
    if tags.is_empty() {
        Spec::Off
    } else {
        Spec::Tags(tags)
    }
}

#[cfg(ktest)]
mod test {
    use ostd::prelude::ktest;

    use super::{Spec, parse, raw_spec_from_cmdline};

    fn matches(spec: &Spec, tag: &str) -> bool {
        match spec {
            Spec::Off => false,
            Spec::All => true,
            Spec::Tags(t) => t.iter().any(|x| x == tag),
        }
    }

    #[ktest]
    fn early_lookup_matches_kernel_argument_boundaries() {
        assert_eq!(
            raw_spec_from_cmdline("k1.diag=off k1.diag=therm"),
            Some("therm")
        );
        assert_eq!(
            raw_spec_from_cmdline("k1.diag=off -- k1.diag=all"),
            Some("off")
        );
        assert_eq!(raw_spec_from_cmdline("-- k1.diag=all"), None);
        assert_eq!(raw_spec_from_cmdline(r#"init="k1.diag=all""#), None);
        let value = raw_spec_from_cmdline(r#"k1.diag="therm, batt" quiet"#).unwrap();
        assert!(matches(&parse(Some(value)), "therm"));
        assert!(matches(&parse(Some(value)), "batt"));
    }

    #[ktest]
    fn absent_means_quiet() {
        assert!(!matches(&parse(None), "therm"));
        assert!(!matches(&parse(Some("")), "therm"));
        assert!(!matches(&parse(Some("off")), "therm"));
    }

    #[ktest]
    fn all_and_tag_lists() {
        let all = parse(Some("all"));
        assert!(matches(&all, "therm") && matches(&all, "futex"));
        let some = parse(Some("therm, perf"));
        assert!(matches(&some, "therm"));
        assert!(matches(&some, "perf"));
        assert!(!matches(&some, "batt"));
        // A keyword anywhere in the list wins over the tags around it.
        assert!(matches(&parse(Some("batt,all")), "flip"));
    }
}
