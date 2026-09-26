use std::{collections::HashMap, time::Duration};

use btleplug::api::{PeripheralProperties, ScanFilter};

use super::{find_by_discriminator, MATTER_SERVICE_UUID};
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
