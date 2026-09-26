use std::{collections::HashMap, time::Duration};

use btleplug::api::{PeripheralProperties, ScanFilter};

use super::{find_by_discriminator, scan_commissionable, MATTER_SERVICE_UUID};
use backend::{Call, CentralEvent, Device, State};

pub(super) mod backend;

const DATA: [u8; 8] = [0x00, 0xbc, 0x0a, 0x2f, 0x13, 0x0d, 0x02, 0x00];
const TIMEOUT: Duration = Duration::from_secs(30);

fn advertisement(id: u64, data: &[u8]) -> CentralEvent {
    CentralEvent::ServiceDataAdvertisement {
        id,
        service_data: HashMap::from([(MATTER_SERVICE_UUID, data.to_vec())]),
    }
}

fn device(data: &[u8]) -> Device {
    Device {
        properties: Some(PeripheralProperties {
            service_data: HashMap::from([(MATTER_SERVICE_UUID, data.to_vec())]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn finish(result: anyhow::Result<crate::btp::BlePeripheral>) {
    // No BTP handshake is run; stop the notification task created by connect_peripheral.
    let peripheral = result.unwrap();
    peripheral.c2_abort.abort();
    (peripheral.disconnect)();
}

#[tokio::test(start_paused = true)]
async fn find_skips_disappeared_peripheral_and_unreadable_properties() {
    let mut state = State {
        events: vec![
            CentralEvent::DeviceDiscovered(1),
            CentralEvent::DeviceUpdated(2),
            CentralEvent::DeviceDiscovered(3),
        ],
        ..Default::default()
    };
    state.devices.insert(
        2,
        Device {
            properties_failures: 1,
            ..device(&DATA)
        },
    );
    state.devices.insert(3, device(&DATA));

    let (result, state) = backend::run(state, find_by_discriminator(0x0abc, false, TIMEOUT)).await;
    finish(result);
    assert_eq!(
        state.lock().unwrap().calls,
        vec![
            Call::Events,
            Call::Start(ScanFilter::default()),
            Call::Lookup(1),
            Call::Lookup(2),
            Call::Properties(2),
            Call::Lookup(3),
            Call::Properties(3),
            Call::Stop,
            Call::Connect(3),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn find_retries_later_advertisement_after_failed_lookup() {
    let mut state = State {
        events: vec![advertisement(1, &DATA), advertisement(1, &DATA)],
        ..Default::default()
    };
    state.devices.insert(
        1,
        Device {
            lookup_failures: 1,
            ..Default::default()
        },
    );

    let (result, state) = backend::run(state, find_by_discriminator(0x0abc, false, TIMEOUT)).await;
    finish(result);
    assert_eq!(
        state.lock().unwrap().calls,
        vec![
            Call::Events,
            Call::Start(ScanFilter::default()),
            Call::Lookup(1),
            Call::Lookup(1),
            Call::Stop,
            Call::Connect(1),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn find_filters_service_data_before_looking_up_peripheral() {
    for (data, discriminator, short_match) in [
        (DATA, 0x0abc, false),
        (DATA, 0x0a00, true),
        (
            [0x00, 0x60, 0x05, 0x2f, 0x13, 0x0d, 0x02, 0x00],
            0x0500,
            true,
        ),
    ] {
        let mut wrong = data;
        wrong[1] ^= 1;
        if short_match {
            wrong[2] = 0x0b;
        }
        let mut state = State {
            events: vec![
                CentralEvent::Other,
                CentralEvent::ServiceDataAdvertisement {
                    id: 1,
                    service_data: HashMap::new(),
                },
                advertisement(1, &data[..7]),
                advertisement(1, &wrong),
                advertisement(2, &data),
            ],
            ..Default::default()
        };
        // Service-data-only device: there are no cached properties or discovery events.
        state.devices.insert(2, Device::default());
        let (result, state) = backend::run(
            state,
            find_by_discriminator(discriminator, short_match, TIMEOUT),
        )
        .await;
        finish(result);
        assert_eq!(
            state.lock().unwrap().calls,
            vec![
                Call::Events,
                Call::Start(ScanFilter::default()),
                Call::Lookup(2),
                Call::Stop,
                Call::Connect(2),
            ]
        );
    }
}

#[tokio::test(start_paused = true)]
async fn find_stops_after_timeout_even_when_candidates_fail() {
    let state = State {
        events: vec![CentralEvent::DeviceDiscovered(1)],
        ..Default::default()
    };
    let before = tokio::time::Instant::now();
    let (result, state) = backend::run(state, find_by_discriminator(0x0abc, false, TIMEOUT)).await;
    assert!(result
        .err()
        .unwrap()
        .to_string()
        .contains("BLE scan timeout"));
    assert_eq!(before.elapsed(), TIMEOUT);
    assert_eq!(state.lock().unwrap().calls.last(), Some(&Call::Stop));
}

#[tokio::test(start_paused = true)]
async fn find_preserves_scan_and_stream_errors() {
    for (state, message, stops) in [
        (
            State {
                events_error: true,
                ..Default::default()
            },
            "BLE event stream",
            false,
        ),
        (
            State {
                start_error: true,
                ..Default::default()
            },
            "start BLE scan",
            false,
        ),
        (
            State {
                end_events: true,
                ..Default::default()
            },
            "BLE event stream ended",
            true,
        ),
    ] {
        let (result, state) =
            backend::run(state, find_by_discriminator(0x0abc, false, TIMEOUT)).await;
        assert_eq!(result.err().unwrap().to_string(), message);
        assert_eq!(state.lock().unwrap().calls.contains(&Call::Stop), stops);
    }
}

#[tokio::test(start_paused = true)]
async fn find_preserves_connection_errors() {
    let mut state = State {
        events: vec![advertisement(1, &DATA)],
        ..Default::default()
    };
    state.devices.insert(
        1,
        Device {
            connect_error: true,
            ..Default::default()
        },
    );
    let (result, state) = backend::run(state, find_by_discriminator(0x0abc, false, TIMEOUT)).await;
    assert_eq!(result.err().unwrap().to_string(), "BLE connect");
    assert!(state
        .lock()
        .unwrap()
        .calls
        .ends_with(&[Call::Stop, Call::Connect(1)]));
}

#[tokio::test(start_paused = true)]
async fn scan_includes_service_data_only_devices_and_skips_bad_candidates() {
    let mut state = State::default();
    state.devices.insert(
        1,
        Device {
            properties_failures: 1,
            ..Default::default()
        },
    );
    let mut data = DATA;
    data[2] |= 0x10;
    let mut expected = device(&data);
    let props = expected.properties.as_mut().unwrap();
    props.local_name = Some("Matter test device".to_owned());
    props.rssi = Some(-60);
    props.tx_power_level = Some(-4);
    assert!(props.services.is_empty());
    state.devices.insert(2, expected);
    state.devices.insert(
        3,
        Device {
            properties: Some(PeripheralProperties::default()),
            ..Default::default()
        },
    );
    state.devices.insert(4, device(&DATA[..7]));
    state.devices.insert(5, Device::default());

    let before = tokio::time::Instant::now();
    let (result, state) = backend::run(state, scan_commissionable(TIMEOUT)).await;
    let found = result.unwrap();
    assert_eq!(before.elapsed(), TIMEOUT);
    assert_eq!(found.len(), 1);
    let found = &found[0];
    assert_eq!(found.discriminator, 0x0abc);
    assert_eq!(found.vendor_id, 0x132f);
    assert_eq!(found.product_id, 0x020d);
    assert!(found.cm_flag);
    assert_eq!(found.name.as_deref(), Some("Matter test device"));
    assert_eq!(found.rssi, Some(-60));
    assert_eq!(found.tx_power, Some(-4));
    assert_eq!(found.address, "2");
    assert_eq!(found.peripheral.id(), 2);
    assert_eq!(
        state.lock().unwrap().calls,
        vec![
            Call::Start(ScanFilter::default()),
            Call::Stop,
            Call::List,
            Call::Properties(1),
            Call::Properties(2),
            Call::Properties(3),
            Call::Properties(4),
            Call::Properties(5),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn scan_with_no_devices_returns_an_empty_list() {
    let (result, state) = backend::run(State::default(), scan_commissionable(TIMEOUT)).await;
    assert!(result.unwrap().is_empty());
    assert_eq!(
        state.lock().unwrap().calls,
        vec![Call::Start(ScanFilter::default()), Call::Stop, Call::List,]
    );
}

#[tokio::test(start_paused = true)]
async fn scan_preserves_start_and_enumeration_errors() {
    let (result, state) = backend::run(
        State {
            start_error: true,
            ..Default::default()
        },
        scan_commissionable(TIMEOUT),
    )
    .await;
    assert_eq!(result.err().unwrap().to_string(), "start BLE scan");
    assert_eq!(
        state.lock().unwrap().calls,
        vec![Call::Start(ScanFilter::default())]
    );

    let (result, state) = backend::run(
        State {
            list_error: true,
            ..Default::default()
        },
        scan_commissionable(TIMEOUT),
    )
    .await;
    assert_eq!(result.err().unwrap().to_string(), "Permission denied");
    assert_eq!(
        state.lock().unwrap().calls,
        vec![Call::Start(ScanFilter::default()), Call::Stop, Call::List,]
    );
}

#[tokio::test(start_paused = true)]
async fn find_warns_when_stop_fails_without_blocking_connection() {
    let mut state = State {
        events: vec![advertisement(1, &DATA)],
        stop_error: true,
        ..Default::default()
    };
    state.devices.insert(1, Device::default());
    let (result, state) = backend::run(state, find_by_discriminator(0x0abc, false, TIMEOUT)).await;
    finish(result);
    let state = state.lock().unwrap();
    assert!(state.calls.ends_with(&[Call::Stop, Call::Connect(1)]));
    assert_eq!(
        state.warnings,
        vec!["BLE stop_scan failed: PermissionDenied"]
    );
}

#[tokio::test(start_paused = true)]
async fn find_warns_when_stop_fails_without_masking_search_or_connection_errors() {
    for end_events in [false, true] {
        let state = State {
            stop_error: true,
            end_events,
            ..Default::default()
        };
        let (result, state) =
            backend::run(state, find_by_discriminator(0x0abc, false, TIMEOUT)).await;
        let expected = if end_events {
            "BLE event stream ended"
        } else {
            "BLE scan timeout"
        };
        assert_eq!(result.err().unwrap().to_string(), expected);
        assert_eq!(
            state.lock().unwrap().warnings,
            vec!["BLE stop_scan failed: PermissionDenied"]
        );
    }

    let mut state = State {
        events: vec![advertisement(1, &DATA)],
        stop_error: true,
        ..Default::default()
    };
    state.devices.insert(
        1,
        Device {
            connect_error: true,
            ..Default::default()
        },
    );
    let (result, state) = backend::run(state, find_by_discriminator(0x0abc, false, TIMEOUT)).await;
    assert_eq!(result.err().unwrap().to_string(), "BLE connect");
    assert_eq!(
        state.lock().unwrap().warnings,
        vec!["BLE stop_scan failed: PermissionDenied"]
    );
}

#[tokio::test(start_paused = true)]
async fn scan_warns_when_stop_fails_without_masking_results_or_enumeration_errors() {
    for list_error in [false, true] {
        let mut state = State {
            stop_error: true,
            list_error,
            ..Default::default()
        };
        state.devices.insert(1, device(&DATA));
        let (result, state) = backend::run(state, scan_commissionable(TIMEOUT)).await;
        if list_error {
            assert_eq!(result.err().unwrap().to_string(), "Permission denied");
        } else {
            let found = result.unwrap();
            assert_eq!(found.len(), 1);
            assert_eq!(found[0].discriminator, 0x0abc);
        }
        assert_eq!(
            state.lock().unwrap().warnings,
            vec!["BLE stop_scan failed: PermissionDenied"]
        );
    }
}
