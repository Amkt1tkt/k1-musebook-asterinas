// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: real-bus regression tests for delayed battery service readiness.

use std::{
    env, fs,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use zbus::{zvariant::OwnedObjectPath, Connection, Proxy};

const SERVICE: &str = "org.freedesktop.UPower";
const UP_PATH: &str = "/org/freedesktop/UPower";
const DISPLAY_PATH: &str = "/org/freedesktop/UPower/devices/DisplayDevice";
const BAT0_PATH: &str = "/org/freedesktop/UPower/devices/battery_BAT0";
const DEVICE_INTERFACE: &str = "org.freedesktop.UPower.Device";
const DEADLINE: Duration = Duration::from_secs(8);
const POLL: Duration = Duration::from_millis(25);
const SAMPLE: Duration = Duration::from_millis(1250);

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
    daemon: Option<Child>,
    shim: Option<Child>,
}

impl Fixture {
    fn new() -> Self {
        let sequence = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!(
            "upower-shim-test-{}-{epoch}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&root).unwrap();
        Self {
            root,
            daemon: None,
            shim: None,
        }
    }

    fn address(&self) -> String {
        format!("unix:path={}", self.root.join("bus").display())
    }

    fn battery_file(&self, name: &str) -> PathBuf {
        self.root.join("BAT0").join(name)
    }

    fn write_battery(&self, percentage: u8, status: &str) {
        fs::create_dir_all(self.root.join("BAT0")).unwrap();
        self.write("capacity", &format!("{percentage}\n"));
        self.write("present", "1\n");
        self.write("status", status);
        self.write("current_now", "-1000000\n");
        self.write("charge_now", "2000000\n");
    }

    fn write(&self, name: &str, content: &str) {
        // Rename makes each fixture update atomic with respect to the sampler.
        let temporary = self.battery_file(&format!(".{name}.new"));
        fs::write(&temporary, content).unwrap();
        fs::rename(temporary, self.battery_file(name)).unwrap();
    }

    async fn start_bus(&mut self) -> Connection {
        let executable = env::var_os("UPOWER_SHIM_TEST_DBUS_DAEMON")
            .expect("set UPOWER_SHIM_TEST_DBUS_DAEMON to a native dbus-daemon");
        let configuration = self.root.join("dbus.conf");
        // This fixture owns an isolated bus; no host policy or activation files
        // should affect the shim's startup and failure behavior.
        fs::write(
            &configuration,
            format!(
                "<busconfig><type>session</type><listen>{}</listen>\n\
                 <policy context=\"default\">\n\
                 <allow own=\"*\"/><allow send_destination=\"*\"/>\n\
                 <allow receive_sender=\"*\"/></policy></busconfig>\n",
                self.address()
            ),
        )
        .unwrap();
        let log = fs::File::create(self.root.join("dbus.stderr")).unwrap();
        self.daemon = Some(
            Command::new(executable)
                .arg("--config-file")
                .arg(configuration)
                .arg("--nofork")
                .arg("--nopidfile")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(log)
                .spawn()
                .expect("start private D-Bus daemon"),
        );
        tokio::time::timeout(DEADLINE, async {
            while !self.root.join("bus").exists() {
                assert!(
                    self.daemon.as_mut().unwrap().try_wait().unwrap().is_none(),
                    "private D-Bus daemon exited: {}",
                    fs::read_to_string(self.root.join("dbus.stderr")).unwrap()
                );
                tokio::time::sleep(POLL).await;
            }
            zbus::connection::Builder::address(self.address().as_str())
                .unwrap()
                .build()
                .await
                .unwrap()
        })
        .await
        .expect("private D-Bus daemon became ready")
    }

    fn start_shim(&mut self) {
        let log = fs::File::create(self.root.join("shim.stderr")).unwrap();
        self.shim = Some(
            Command::new(env!("CARGO_BIN_EXE_upower-shim"))
                .arg("--sysfs-root")
                .arg(self.root.join("BAT0"))
                .args(["--interval-secs", "1"])
                .env("DBUS_SYSTEM_BUS_ADDRESS", self.address())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(log)
                .spawn()
                .expect("start UPower shim"),
        );
    }

    fn assert_running(&mut self) {
        assert!(
            self.shim.as_mut().unwrap().try_wait().unwrap().is_none(),
            "UPower shim exited before recovery: {}",
            self.log()
        );
    }

    fn log(&self) -> String {
        fs::read_to_string(self.root.join("shim.stderr")).unwrap()
    }

    async fn wait_for_owner(&mut self, connection: &Connection) {
        tokio::time::timeout(DEADLINE, async {
            loop {
                self.assert_running();
                if has_owner(connection).await {
                    return;
                }
                tokio::time::sleep(POLL).await;
            }
        })
        .await
        .expect("UPower shim acquired its bus name after recovery");
    }

    async fn assert_unpublished(&mut self, connection: &Connection) {
        // More than one interval prevents an assertion before the next retry.
        tokio::time::sleep(SAMPLE).await;
        self.assert_running();
        assert!(!has_owner(connection).await, "unusable BAT0 was published");
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for child in [&mut self.shim, &mut self.daemon] {
            if let Some(mut process) = child.take() {
                let _ = process.kill();
                let _ = process.wait();
            }
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

async fn has_owner(connection: &Connection) -> bool {
    let bus = Proxy::new(
        connection,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    )
    .await
    .unwrap();
    bus.call("NameHasOwner", &(SERVICE,)).await.unwrap()
}

async fn percentage(connection: &Connection) -> f64 {
    let properties = Proxy::new(
        connection,
        SERVICE,
        DISPLAY_PATH,
        "org.freedesktop.DBus.Properties",
    )
    .await
    .unwrap();
    // A direct Get avoids a client property cache masking service failures.
    let value: zbus::zvariant::OwnedValue = properties
        .call("Get", &(DEVICE_INTERFACE, "Percentage"))
        .await
        .unwrap();
    f64::try_from(value).unwrap()
}

async fn wait_for_percentage(connection: &Connection, expected: f64) {
    tokio::time::timeout(DEADLINE, async {
        while percentage(connection).await != expected {
            tokio::time::sleep(POLL).await;
        }
    })
    .await
    .expect("published battery percentage recovered");
}

async fn assert_battery_device(connection: &Connection) {
    let upower = Proxy::new(connection, SERVICE, UP_PATH, SERVICE)
        .await
        .unwrap();
    let devices: Vec<OwnedObjectPath> = upower.call("EnumerateDevices", &()).await.unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].as_str(), BAT0_PATH);
    let device = Proxy::new(connection, SERVICE, BAT0_PATH, DEVICE_INTERFACE)
        .await
        .unwrap();
    assert_eq!(device.get_property::<u32>("Type").await.unwrap(), 2);
    assert!(device.get_property::<bool>("PowerSupply").await.unwrap());
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires a native dbus-daemon; see tests/README.md"]
async fn delayed_battery_and_invalid_capacity_do_not_publish_an_empty_device() {
    let mut fixture = Fixture::new();
    let connection = fixture.start_bus().await;
    fixture.start_shim();
    fixture.assert_unpublished(&connection).await;

    fs::create_dir(fixture.root.join("BAT0")).unwrap();
    fixture.assert_unpublished(&connection).await;
    fixture.write("capacity", "invalid\n");
    fixture.assert_unpublished(&connection).await;

    fixture.write("present", "0\n");
    fixture.write("capacity", "63\n");
    fixture.assert_unpublished(&connection).await;
    fixture.write_battery(63, "Discharging\n");
    fixture.wait_for_owner(&connection).await;
    assert_eq!(percentage(&connection).await, 63.0);
    assert_battery_device(&connection).await;
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires a native dbus-daemon; see tests/README.md"]
async fn delayed_bus_publishes_the_latest_battery_snapshot() {
    let mut fixture = Fixture::new();
    fixture.write_battery(42, "Discharging\n");
    fixture.start_shim();
    tokio::time::sleep(SAMPLE).await;
    fixture.assert_running();

    fixture.write_battery(81, "Charging\n");
    let connection = fixture.start_bus().await;
    fixture.wait_for_owner(&connection).await;
    assert_eq!(percentage(&connection).await, 81.0);
    assert_battery_device(&connection).await;
    let upower = Proxy::new(&connection, SERVICE, UP_PATH, SERVICE)
        .await
        .unwrap();
    assert!(!upower.get_property::<bool>("OnBattery").await.unwrap());
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires a native dbus-daemon; see tests/README.md"]
async fn sampling_failures_preserve_the_last_snapshot_and_recover() {
    let mut fixture = Fixture::new();
    fixture.write_battery(54, "Discharging\n");
    let connection = fixture.start_bus().await;
    fixture.start_shim();
    fixture.wait_for_owner(&connection).await;

    fs::remove_file(fixture.battery_file("capacity")).unwrap();
    tokio::time::sleep(SAMPLE).await;
    fixture.assert_running();
    assert!(has_owner(&connection).await);
    assert_eq!(percentage(&connection).await, 54.0);
    fixture.write("capacity", "invalid\n");
    tokio::time::sleep(SAMPLE).await;
    assert_eq!(percentage(&connection).await, 54.0);
    assert_battery_device(&connection).await;

    fixture.write("capacity", "72\n");
    wait_for_percentage(&connection, 72.0).await;
    fixture.assert_running();
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires a native dbus-daemon; see tests/README.md"]
async fn repeated_unavailability_does_not_repeat_error_logs() {
    let mut fixture = Fixture::new();
    let connection = fixture.start_bus().await;
    fixture.start_shim();
    fixture.assert_unpublished(&connection).await;
    let first_failure = fixture.log();
    assert!(
        !first_failure.is_empty(),
        "initial unavailability was not logged"
    );
    tokio::time::sleep(Duration::from_secs(2)).await;
    fixture.assert_running();
    assert_eq!(
        fixture.log(),
        first_failure,
        "each startup retry emitted a log"
    );

    fixture.write_battery(39, "Discharging\n");
    fixture.wait_for_owner(&connection).await;
    // Allow the initial publication log to settle before the next failure.
    tokio::time::sleep(POLL).await;
    let before_failure = fixture.log();
    fixture.write("capacity", "invalid\n");
    tokio::time::sleep(SAMPLE).await;
    let sampling_failure = fixture.log();
    assert!(
        sampling_failure.len() > before_failure.len(),
        "sampling unavailability was not logged"
    );
    tokio::time::sleep(Duration::from_secs(2)).await;
    fixture.assert_running();
    assert_eq!(
        fixture.log(),
        sampling_failure,
        "each sampling retry emitted a log"
    );
    fixture.write("capacity", "45\n");
    wait_for_percentage(&connection, 45.0).await;
}
