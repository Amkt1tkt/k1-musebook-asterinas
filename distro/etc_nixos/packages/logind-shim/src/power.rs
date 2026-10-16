// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: hand login1 power requests to PID 1's orderly shutdown targets.

use std::{
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const SYSTEMCTL: &str = "/run/current-system/systemd/bin/systemctl";

#[derive(Clone, Copy, Debug)]
pub enum Action {
    PowerOff,
    Reboot,
}

impl Action {
    fn target(self) -> &'static str {
        match self {
            Self::PowerOff => "poweroff.target",
            Self::Reboot => "reboot.target",
        }
    }

    fn args(self) -> [&'static str; 5] {
        [
            "--no-block",
            "--job-mode=replace-irreversibly",
            "--no-ask-password",
            "start",
            self.target(),
        ]
    }
}

pub fn authorize_uid(uid: u32) -> Result<(), String> {
    if uid == 0 {
        Ok(())
    } else {
        Err("This board's power-control shim requires a root bus caller".into())
    }
}

pub fn available() -> bool {
    Path::new(SYSTEMCTL).is_file()
}

pub async fn queue(action: Action) -> Result<(), String> {
    queue_with(Path::new(SYSTEMCTL), action).await
}

async fn queue_with(systemctl: &Path, action: Action) -> Result<(), String> {
    // Do not run "systemctl poweroff": it calls login1 again. Starting the
    // target directly lets PID 1 stop services, save the RTC, kill remaining
    // processes and execute systemd-shutdown. No --force or direct reboot(2).
    let mut child = Command::new(systemctl)
        .args(action.args())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("cannot start {}: {e}", systemctl.display()))?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => {
                return Err(format!(
                    "systemctl start {} failed: {status}",
                    action.target()
                ));
            }
            Ok(None) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            result => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "systemctl request timed out or wait failed: {result:?}"
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_root_bus_callers_can_request_power_transitions() {
        assert!(authorize_uid(0).is_ok());
        assert!(authorize_uid(1).is_err());
        assert!(authorize_uid(1000).is_err());
        assert!(authorize_uid(u32::MAX).is_err());
    }

    #[test]
    fn requests_use_orderly_pid1_targets_without_login1_recursion() {
        assert_eq!(
            Action::PowerOff.args(),
            [
                "--no-block",
                "--job-mode=replace-irreversibly",
                "--no-ask-password",
                "start",
                "poweroff.target"
            ]
        );
        assert_eq!(Action::Reboot.args()[4], "reboot.target");
        assert!(!Action::PowerOff.args().contains(&"--force"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn failed_or_missing_programs_are_reported_to_the_caller() {
        assert!(queue_with(Path::new("/bin/false"), Action::PowerOff)
            .await
            .is_err());
        assert!(
            queue_with(Path::new("/no-such-k1-systemctl"), Action::Reboot)
                .await
                .is_err()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn successful_request_program_is_reaped() {
        // Only /bin/true executes; these tests never contact a running PID 1.
        assert!(queue_with(Path::new("/bin/true"), Action::PowerOff)
            .await
            .is_ok());
    }
}
