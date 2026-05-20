//! Sensor poll task — periodically polls every configured sensor and
//! publishes the result on `EventBus.raw`.
//!
//! On the default (host) build the sensors are [`MockDigital`] / [`MockAdc`]
//! / [`MockClimate`]. With `--features pi-hw` on `acorn-sensors`, real
//! drivers are used (see `acorn_sensors::pi`).

use std::{sync::Arc, time::Duration};

use acorn_api::{events::RawReadingEvent, EventBus};
use acorn_sensors::{MockAdc, MockClimate, MockDigital, Sensor, SensorKind};
use tokio::sync::Mutex;

use crate::Args;

struct Bound {
    source: String,
    sensor: Mutex<Box<dyn Sensor>>,
}

pub async fn run(args: Args, bus: Arc<EventBus>) -> anyhow::Result<()> {
    let bound = build_sensors(&args);
    if bound.is_empty() {
        tracing::info!("no sensors bound; sensor task idle");
        return Ok(());
    }
    let names: Vec<&str> = bound.iter().map(|b| b.source.as_str()).collect();
    tracing::info!(?names, interval_ms = args.sensor_poll_ms, "sensor poll starting");

    let interval = Duration::from_millis(args.sensor_poll_ms.max(50));
    let mut ticker = tokio::time::interval(interval);
    loop {
        ticker.tick().await;
        for b in &bound {
            let mut guard = b.sensor.lock().await;
            match guard.poll().await {
                Ok(reading) => {
                    bus.publish_raw(RawReadingEvent {
                        source: b.source.clone(),
                        ts_us: now_us(),
                        reading: reading.into(),
                    });
                }
                Err(e) => {
                    tracing::warn!(source = %b.source, ?e, "sensor poll error");
                }
            }
        }
    }
}

fn build_sensors(args: &Args) -> Vec<Bound> {
    let mut v: Vec<Bound> = Vec::new();

    // Digital inputs — under `pi-hw` use real GPIO; otherwise mock toggles.
    let digitals = [
        (SensorKind::Reed, args.reed_pin, "reed"),
        (SensorKind::Pir, args.pir_pin, "pir"),
        (SensorKind::Vibration, args.vibration_pin, "vibration"),
    ];
    for (kind, pin, label) in digitals {
        let sensor: Box<dyn Sensor> = if cfg!(feature = "pi-hw") {
            match digital_constructor(kind)(pin) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(%label, %pin, ?e, "falling back to mock");
                    Box::new(MockDigital::new(kind, 10))
                }
            }
        } else {
            Box::new(MockDigital::new(kind, 10))
        };
        v.push(Bound {
            source: format!("{label}@gpio{pin}"),
            sensor: Mutex::new(sensor),
        });
    }

    // ADS1115 (4-ch ADC).
    let adc: Box<dyn Sensor> = if cfg!(feature = "pi-hw") {
        match acorn_sensors::pi::adc_on_i2c(args.ads1115_addr) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(addr = ?args.ads1115_addr, ?e, "falling back to mock ADC");
                Box::new(MockAdc::new())
            }
        }
    } else {
        Box::new(MockAdc::new())
    };
    v.push(Bound {
        source: format!("ads1115@0x{:02x}", args.ads1115_addr),
        sensor: Mutex::new(adc),
    });

    // BME280 (climate).
    let climate: Box<dyn Sensor> = if cfg!(feature = "pi-hw") {
        match acorn_sensors::pi::climate_on_i2c(args.bme280_addr) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(addr = ?args.bme280_addr, ?e, "falling back to mock climate");
                Box::new(MockClimate)
            }
        }
    } else {
        Box::new(MockClimate)
    };
    v.push(Bound {
        source: format!("bme280@0x{:02x}", args.bme280_addr),
        sensor: Mutex::new(climate),
    });

    v
}

type DigCtor = fn(u8) -> Result<Box<dyn Sensor>, acorn_sensors::SensorError>;

fn digital_constructor(kind: SensorKind) -> DigCtor {
    match kind {
        SensorKind::Reed => acorn_sensors::pi::reed_on_gpio,
        SensorKind::Pir => acorn_sensors::pi::pir_on_gpio,
        SensorKind::Vibration => acorn_sensors::pi::vibration_on_gpio,
        _ => unreachable!(),
    }
}

fn now_us() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}
