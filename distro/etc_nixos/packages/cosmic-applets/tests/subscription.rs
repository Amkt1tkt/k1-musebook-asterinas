// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: observable initialization and battery events on a private D-Bus.
#[cfg(test)]
mod tests {
    use std::{
        sync::{
            atomic::{AtomicBool, AtomicU64, Ordering},
            Arc,
        },
        time::{Duration, Instant},
    };

    use cosmic_settings_subscriptions::upower::device::{device_subscription, DeviceDbusEvent};
    use futures::{stream, StreamExt};
    use zbus::zvariant::OwnedObjectPath;

    const ROOT: &str = "/org/freedesktop/UPower";
    const DISPLAY: &str = "/org/freedesktop/UPower/devices/DisplayDevice";
    const BATTERY: &str = "/org/freedesktop/UPower/devices/battery_BAT0";

    struct Power {
        has_battery: bool,
    }

    #[zbus::interface(name = "org.freedesktop.UPower")]
    impl Power {
        fn get_display_device(&self) -> OwnedObjectPath {
            DISPLAY.try_into().unwrap()
        }

        fn enumerate_devices(&self) -> Vec<OwnedObjectPath> {
            if self.has_battery {
                vec![BATTERY.try_into().unwrap()]
            } else {
                vec![]
            }
        }

        #[zbus(property)]
        fn on_battery(&self) -> bool {
            true
        }
    }

    struct Display {
        percent: Arc<AtomicU64>,
    }

    #[zbus::interface(name = "org.freedesktop.UPower.Device")]
    impl Display {
        #[zbus(property)]
        fn percentage(&self) -> f64 {
            self.percent.load(Ordering::SeqCst) as f64
        }

        #[zbus(property)]
        fn time_to_empty(&self) -> i64 {
            3600
        }
    }

    struct Battery {
        unreadable: Arc<AtomicBool>,
    }

    #[zbus::interface(name = "org.freedesktop.UPower.Device")]
    impl Battery {
        #[zbus(property, name = "Type")]
        fn device_type(&self) -> zbus::fdo::Result<u32> {
            if self.unreadable.load(Ordering::SeqCst) {
                Err(zbus::fdo::Error::Failed(
                    "temporary battery query failure".into(),
                ))
            } else {
                Ok(2)
            }
        }

        #[zbus(property)]
        fn power_supply(&self) -> bool {
            true
        }
    }

    async fn serve(
        has_battery: bool,
        unreadable: Arc<AtomicBool>,
        percent: Arc<AtomicU64>,
    ) -> zbus::Connection {
        zbus::connection::Builder::system()
            .unwrap()
            .name("org.freedesktop.UPower")
            .unwrap()
            .serve_at(ROOT, Power { has_battery })
            .unwrap()
            .serve_at(DISPLAY, Display { percent })
            .unwrap()
            .serve_at(BATTERY, Battery { unreadable })
            .unwrap()
            .build()
            .await
            .unwrap()
    }

    fn subscribe() -> iced_futures::BoxStream<DeviceDbusEvent> {
        iced_futures::subscription::into_recipes(device_subscription("battery-regression"))
            .pop()
            .unwrap()
            .stream(Box::pin(stream::pending()))
    }

    fn assert_snapshot(event: DeviceDbusEvent, expected: f64) {
        match event {
            DeviceDbusEvent::Update {
                on_battery,
                percent,
                time_to_empty,
            } => {
                assert!(on_battery);
                assert_eq!(percent, expected);
                assert_eq!(time_to_empty, 3600);
            }
            DeviceDbusEvent::NoBattery => panic!("readable battery was reported absent"),
        }
    }

    #[tokio::test]
    async fn late_service_recovers_without_busy_waiting() {
        let started = Instant::now();
        let mut events = subscribe();
        assert!(
            tokio::time::timeout(Duration::from_millis(500), events.next())
                .await
                .is_err()
        );
        let service = serve(
            true,
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicU64::new(63)),
        )
        .await;
        let event = tokio::time::timeout(Duration::from_secs(4), events.next())
            .await
            .unwrap()
            .unwrap();
        assert_snapshot(event, 63.0);
        assert!(
            started.elapsed() >= Duration::from_secs(2),
            "retry did not respect its delay"
        );
        drop(events);
        service.close().await.unwrap();
    }

    #[tokio::test]
    async fn static_battery_has_initial_snapshot_then_receives_changes() {
        let percent = Arc::new(AtomicU64::new(63));
        let service = serve(true, Arc::new(AtomicBool::new(false)), percent.clone()).await;
        let mut events = subscribe();
        let event = tokio::time::timeout(Duration::from_secs(1), events.next())
            .await
            .unwrap()
            .unwrap();
        assert_snapshot(event, 63.0);
        percent.store(62, Ordering::SeqCst);
        let display = service
            .object_server()
            .interface::<_, Display>(DISPLAY)
            .await
            .unwrap();
        display
            .get()
            .await
            .percentage_changed(display.signal_context())
            .await
            .unwrap();
        // Cache population can also wake a listener; wait for the changed value.
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let event = events.next().await.unwrap();
                if matches!(event, DeviceDbusEvent::Update { percent: 62.0, .. }) {
                    assert_snapshot(event, 62.0);
                    break;
                }
            }
        })
        .await
        .unwrap();
        drop(events);
        drop(display);
        service.close().await.unwrap();
    }

    #[tokio::test]
    async fn failed_device_query_retries_instead_of_reporting_no_battery() {
        let unreadable = Arc::new(AtomicBool::new(true));
        let service = serve(true, unreadable.clone(), Arc::new(AtomicU64::new(63))).await;
        let mut events = subscribe();
        assert!(
            tokio::time::timeout(Duration::from_millis(500), events.next())
                .await
                .is_err()
        );
        unreadable.store(false, Ordering::SeqCst);
        let event = tokio::time::timeout(Duration::from_secs(4), events.next())
            .await
            .unwrap()
            .unwrap();
        assert_snapshot(event, 63.0);
        drop(events);
        service.close().await.unwrap();
    }

    #[tokio::test]
    async fn successful_empty_enumeration_still_reports_no_battery() {
        let service = serve(
            false,
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicU64::new(63)),
        )
        .await;
        let mut events = subscribe();
        let event = tokio::time::timeout(Duration::from_secs(1), events.next())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(event, DeviceDbusEvent::NoBattery));
        drop(events);
        service.close().await.unwrap();
    }
}
