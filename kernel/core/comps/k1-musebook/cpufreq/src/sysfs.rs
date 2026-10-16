// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: Linux cpufreq sysfs attribute formatting.

//! Linux cpufreq sysfs text. The kernel crate hangs the nodes.

use alloc::{format, string::String};

use crate::{
    opp,
    policy::{Governor, Policy},
};

pub const AVAILABLE_GOVERNORS: &str = "performance powersave userspace";

pub fn format_khz(khz: u32) -> String {
    format!("{khz}\n")
}

pub fn format_available_frequencies() -> String {
    format!("{}\n", opp::format_available())
}

pub fn format_governor(gov: Governor) -> String {
    format!("{}\n", gov.as_str())
}

pub fn format_available_governors() -> String {
    format!("{AVAILABLE_GOVERNORS}\n")
}

/// Snapshot used by live attrs so a `cat` does not take the policy lock
/// twice for one file.
#[derive(Clone, Copy)]
pub struct AttrSnap {
    pub policy: Policy,
}

impl AttrSnap {
    pub fn attr(self, name: &str) -> Option<String> {
        match name {
            "scaling_cur_freq" => Some(format_khz(self.policy.cur_khz)),
            "scaling_min_freq" => Some(format_khz(self.policy.min_khz)),
            "scaling_max_freq" => Some(format_khz(self.policy.max_khz)),
            "scaling_setspeed" => Some(format_khz(self.policy.setspeed_khz)),
            "scaling_available_frequencies" => Some(format_available_frequencies()),
            "scaling_governor" => Some(format_governor(self.policy.gov)),
            "scaling_available_governors" => Some(format_available_governors()),
            _ => None,
        }
    }
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::*;

    use super::*;

    #[ktest]
    fn linux_spellings() {
        let snap = AttrSnap {
            policy: Policy::conservative_default(),
        };
        assert_eq!(snap.attr("scaling_cur_freq").unwrap(), "1228800\n");
        assert_eq!(
            snap.attr("scaling_available_frequencies").unwrap(),
            "614400 819000 1000000 1228800 1600000\n"
        );
        assert_eq!(snap.attr("scaling_governor").unwrap(), "userspace\n");
        assert_eq!(
            snap.attr("scaling_available_governors").unwrap(),
            "performance powersave userspace\n"
        );
    }
}
