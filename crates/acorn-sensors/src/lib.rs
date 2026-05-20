//! `acorn-sensors` — physical sensor pipeline + reflex rules.
//!
//! Two layers:
//!
//! * The [`Sensor`] trait + concrete implementations
//!   ([`MockSensor`] always available; the `pi-hw` feature enables
//!   `rppal`/`ads1x1x`/`bme280` backed types stubbed in the `pi` module).
//! * The [`Reflex`] state machine, which turns a stream of
//!   [`acorn_proto::event::FeatureVector`]s into semantic
//!   [`acorn_proto::event::SensingEvent`]s.

#![forbid(unsafe_code)]

use acorn_proto::event::{FeatureVector, SensingEvent, Zone};
use async_trait::async_trait;
use parking_lot::Mutex;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SensorError {
    #[error("io: {0}")]
    Io(String),
    #[error("hardware unavailable (pi-hw feature off)")]
    HardwareUnavailable,
    #[error("driver: {0}")]
    Driver(String),
}

// ---------------------------------------------------------------------------
// Sensor trait
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SensorKind {
    Reed,
    Pir,
    Vibration,
    Adc,
    Climate,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SensorReading {
    Digital { high: bool },
    Adc { channels: [i16; 4] },
    Climate { temp_c: f32, humidity_pct: f32, pressure_hpa: f32 },
}

#[async_trait]
pub trait Sensor: Send + Sync {
    fn kind(&self) -> SensorKind;
    async fn poll(&mut self) -> Result<SensorReading, SensorError>;
}

// ---------------------------------------------------------------------------
// Mock sensors — deterministic, no hardware
// ---------------------------------------------------------------------------

/// A deterministic GPIO mock that flips state every `period` polls.
pub struct MockDigital {
    kind: SensorKind,
    counter: Mutex<u64>,
    period: u64,
}

impl MockDigital {
    pub fn new(kind: SensorKind, period: u64) -> Self {
        debug_assert!(matches!(
            kind,
            SensorKind::Reed | SensorKind::Pir | SensorKind::Vibration
        ));
        Self {
            kind,
            counter: Mutex::new(0),
            period: period.max(1),
        }
    }
}

#[async_trait]
impl Sensor for MockDigital {
    fn kind(&self) -> SensorKind {
        self.kind
    }
    async fn poll(&mut self) -> Result<SensorReading, SensorError> {
        let mut c = self.counter.lock();
        *c += 1;
        Ok(SensorReading::Digital {
            high: (*c / self.period) % 2 == 1,
        })
    }
}

/// A mock ADS1115 that produces a triangle wave on each channel.
pub struct MockAdc {
    counter: Mutex<u64>,
}

impl MockAdc {
    pub fn new() -> Self {
        Self {
            counter: Mutex::new(0),
        }
    }
}

impl Default for MockAdc {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Sensor for MockAdc {
    fn kind(&self) -> SensorKind {
        SensorKind::Adc
    }
    async fn poll(&mut self) -> Result<SensorReading, SensorError> {
        let mut c = self.counter.lock();
        *c += 1;
        let t = (*c % 200) as i16;
        let v = if t < 100 { t * 100 } else { (200 - t) * 100 };
        Ok(SensorReading::Adc {
            channels: [v, -v, v / 2, -v / 2],
        })
    }
}

/// A mock BME280 that produces stable indoor-ish climate values.
pub struct MockClimate;

#[async_trait]
impl Sensor for MockClimate {
    fn kind(&self) -> SensorKind {
        SensorKind::Climate
    }
    async fn poll(&mut self) -> Result<SensorReading, SensorError> {
        Ok(SensorReading::Climate {
            temp_c: 21.5,
            humidity_pct: 42.0,
            pressure_hpa: 1013.0,
        })
    }
}

// ---------------------------------------------------------------------------
// Pi hardware impls (gated)
// ---------------------------------------------------------------------------

/// Pi-hardware implementations. The constructors compile on any host. On
/// linux with the `pi-hw` feature on they instantiate real drivers; on any
/// other target / without the feature they return
/// [`SensorError::HardwareUnavailable`].
pub mod pi {
    use super::*;

    pub fn reed_on_gpio(pin: u8) -> Result<Box<dyn Sensor>, SensorError> {
        gpio_input(pin, SensorKind::Reed)
    }
    pub fn pir_on_gpio(pin: u8) -> Result<Box<dyn Sensor>, SensorError> {
        gpio_input(pin, SensorKind::Pir)
    }
    pub fn vibration_on_gpio(pin: u8) -> Result<Box<dyn Sensor>, SensorError> {
        gpio_input(pin, SensorKind::Vibration)
    }
    pub fn adc_on_i2c(addr: u8) -> Result<Box<dyn Sensor>, SensorError> {
        ads1115(addr)
    }
    pub fn climate_on_i2c(addr: u8) -> Result<Box<dyn Sensor>, SensorError> {
        bme280(addr)
    }

    // --- Stubs (any host w/o pi-hw, or non-linux even with pi-hw) ----------

    #[cfg(not(all(feature = "pi-hw", target_os = "linux")))]
    fn gpio_input(_pin: u8, _kind: SensorKind) -> Result<Box<dyn Sensor>, SensorError> {
        Err(SensorError::HardwareUnavailable)
    }
    #[cfg(not(all(feature = "pi-hw", target_os = "linux")))]
    fn ads1115(_addr: u8) -> Result<Box<dyn Sensor>, SensorError> {
        Err(SensorError::HardwareUnavailable)
    }
    #[cfg(not(all(feature = "pi-hw", target_os = "linux")))]
    fn bme280(_addr: u8) -> Result<Box<dyn Sensor>, SensorError> {
        Err(SensorError::HardwareUnavailable)
    }

    // --- Real drivers (linux + pi-hw) --------------------------------------

    #[cfg(all(feature = "pi-hw", target_os = "linux"))]
    fn gpio_input(pin: u8, kind: SensorKind) -> Result<Box<dyn Sensor>, SensorError> {
        use rppal::gpio::Gpio;
        let pin = Gpio::new()
            .map_err(|e| SensorError::Driver(format!("gpio init: {e}")))?
            .get(pin)
            .map_err(|e| SensorError::Driver(format!("gpio pin: {e}")))?
            .into_input_pullup();
        Ok(Box::new(RppalDigital { kind, pin }))
    }

    #[cfg(all(feature = "pi-hw", target_os = "linux"))]
    pub(super) struct RppalDigital {
        pub kind: SensorKind,
        pub pin: rppal::gpio::InputPin,
    }

    #[cfg(all(feature = "pi-hw", target_os = "linux"))]
    #[async_trait::async_trait]
    impl Sensor for RppalDigital {
        fn kind(&self) -> SensorKind {
            self.kind
        }
        async fn poll(&mut self) -> Result<SensorReading, SensorError> {
            Ok(SensorReading::Digital {
                high: self.pin.is_high(),
            })
        }
    }

    #[cfg(all(feature = "pi-hw", target_os = "linux"))]
    fn ads1115(addr: u8) -> Result<Box<dyn Sensor>, SensorError> {
        use ads1x1x::{channel, Ads1x1x, FullScaleRange, TargetAddr};
        use linux_embedded_hal::I2cdev;
        let dev = I2cdev::new("/dev/i2c-1")
            .map_err(|e| SensorError::Driver(format!("i2c open: {e}")))?;
        let target = TargetAddr::new(addr)
            .map_err(|e| SensorError::Driver(format!("ads addr: {e:?}")))?;
        let mut adc = Ads1x1x::new_ads1115(dev, target);
        adc.set_full_scale_range(FullScaleRange::Within4_096V)
            .map_err(|e| SensorError::Driver(format!("ads fsr: {e:?}")))?;
        Ok(Box::new(Ads1115Sensor { adc }))
    }

    #[cfg(all(feature = "pi-hw", target_os = "linux"))]
    pub(super) struct Ads1115Sensor {
        pub adc: ads1x1x::Ads1x1x<
            linux_embedded_hal::I2cdev,
            ads1x1x::ic::Ads1115,
            ads1x1x::ic::Resolution16Bit,
            ads1x1x::mode::OneShot,
        >,
    }

    #[cfg(all(feature = "pi-hw", target_os = "linux"))]
    #[async_trait::async_trait]
    impl Sensor for Ads1115Sensor {
        fn kind(&self) -> SensorKind {
            SensorKind::Adc
        }
        async fn poll(&mut self) -> Result<SensorReading, SensorError> {
            use ads1x1x::channel::*;
            let a = nb::block!(self.adc.read(SingleA0))
                .map_err(|e| SensorError::Driver(format!("ads a0: {e:?}")))?;
            let b = nb::block!(self.adc.read(SingleA1))
                .map_err(|e| SensorError::Driver(format!("ads a1: {e:?}")))?;
            let c = nb::block!(self.adc.read(SingleA2))
                .map_err(|e| SensorError::Driver(format!("ads a2: {e:?}")))?;
            let d = nb::block!(self.adc.read(SingleA3))
                .map_err(|e| SensorError::Driver(format!("ads a3: {e:?}")))?;
            Ok(SensorReading::Adc {
                channels: [a, b, c, d],
            })
        }
    }

    #[cfg(all(feature = "pi-hw", target_os = "linux"))]
    fn bme280(addr: u8) -> Result<Box<dyn Sensor>, SensorError> {
        use bme280::i2c::BME280;
        use linux_embedded_hal::{Delay, I2cdev};
        let dev = I2cdev::new("/dev/i2c-1")
            .map_err(|e| SensorError::Driver(format!("i2c open: {e}")))?;
        let mut driver = BME280::new(dev, addr);
        driver
            .init(&mut Delay)
            .map_err(|e| SensorError::Driver(format!("bme280 init: {e:?}")))?;
        Ok(Box::new(Bme280Sensor { driver }))
    }

    #[cfg(all(feature = "pi-hw", target_os = "linux"))]
    pub(super) struct Bme280Sensor {
        pub driver: bme280::i2c::BME280<linux_embedded_hal::I2cdev>,
    }

    #[cfg(all(feature = "pi-hw", target_os = "linux"))]
    #[async_trait::async_trait]
    impl Sensor for Bme280Sensor {
        fn kind(&self) -> SensorKind {
            SensorKind::Climate
        }
        async fn poll(&mut self) -> Result<SensorReading, SensorError> {
            use linux_embedded_hal::Delay;
            let m = self
                .driver
                .measure(&mut Delay)
                .map_err(|e| SensorError::Driver(format!("bme280 measure: {e:?}")))?;
            Ok(SensorReading::Climate {
                temp_c: m.temperature,
                humidity_pct: m.humidity,
                pressure_hpa: m.pressure / 100.0,
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Reflex rules
// ---------------------------------------------------------------------------

/// Tunable thresholds for the reflex rules.
#[derive(Debug, Clone, Copy)]
pub struct ReflexConfig {
    pub presence_threshold: f32,
    pub motion_threshold: f32,
    pub min_hr_bpm: f32,
    pub max_hr_bpm: f32,
    pub min_rr_bpm: f32,
    pub max_rr_bpm: f32,
}

impl Default for ReflexConfig {
    fn default() -> Self {
        Self {
            presence_threshold: 0.5,
            motion_threshold: 0.7,
            min_hr_bpm: 40.0,
            max_hr_bpm: 200.0,
            min_rr_bpm: 5.0,
            max_rr_bpm: 40.0,
        }
    }
}

/// Per-zone reflex state. Emits [`SensingEvent`]s only when a meaningful
/// change happens (transitions, threshold crossings) — not every poll.
pub struct Reflex {
    cfg: ReflexConfig,
    state: Mutex<ReflexState>,
}

#[derive(Default)]
struct ReflexState {
    occupied: Option<bool>,
    last_motion_above: bool,
    last_fall: bool,
}

impl Reflex {
    pub fn new(cfg: ReflexConfig) -> Self {
        Self {
            cfg,
            state: Mutex::new(ReflexState::default()),
        }
    }

    /// Evaluate one feature vector for the given zone. Returns 0..N events.
    pub fn evaluate(&self, fv: &FeatureVector, zone: &Zone) -> Vec<SensingEvent> {
        let mut out = Vec::new();
        let mut st = self.state.lock();

        // Rule 1 — fall (transition from "not fallen" to "fallen").
        let now_fall = fv.fall_detected();
        if now_fall && !st.last_fall {
            out.push(SensingEvent::Fall { zone: zone.clone() });
        }
        st.last_fall = now_fall;

        // Rule 2 — occupancy state change.
        let occupied = fv.presence() >= self.cfg.presence_threshold;
        if st.occupied != Some(occupied) {
            out.push(SensingEvent::Occupancy {
                zone: zone.clone(),
                occupied,
                confidence: fv.presence().clamp(0.0, 1.0),
            });
            st.occupied = Some(occupied);
        }

        // Rule 3 — vitals (only when occupied and within plausible bounds).
        if occupied {
            let hr = fv.heart_rate_bpm();
            let rr = fv.breathing_bpm();
            if hr >= self.cfg.min_hr_bpm
                && hr <= self.cfg.max_hr_bpm
                && rr >= self.cfg.min_rr_bpm
                && rr <= self.cfg.max_rr_bpm
            {
                out.push(SensingEvent::Vitals {
                    zone: zone.clone(),
                    heart_rate_bpm: hr,
                    breathing_bpm: rr,
                });
            }
        }

        // Rule 4 — motion crossing the threshold (transition only).
        let now_motion = fv.motion_energy() >= self.cfg.motion_threshold;
        if now_motion && !st.last_motion_above {
            out.push(SensingEvent::Motion {
                zone: zone.clone(),
                energy: fv.motion_energy(),
            });
        }
        st.last_motion_above = now_motion;

        out
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_digital_toggles() {
        let mut s = MockDigital::new(SensorKind::Pir, 2);
        let r1 = s.poll().await.unwrap();
        let r2 = s.poll().await.unwrap();
        let r3 = s.poll().await.unwrap();
        let r4 = s.poll().await.unwrap();
        // period=2 → first two false, next two true (counter starts at 1).
        assert!(matches!(r1, SensorReading::Digital { high: false }));
        assert!(matches!(r2, SensorReading::Digital { high: true }));
        assert!(matches!(r3, SensorReading::Digital { high: true }));
        assert!(matches!(r4, SensorReading::Digital { high: false }));
    }

    #[tokio::test]
    async fn mock_climate_is_stable() {
        let mut s = MockClimate;
        let r = s.poll().await.unwrap();
        if let SensorReading::Climate {
            temp_c,
            humidity_pct,
            pressure_hpa,
        } = r
        {
            assert!((20.0..25.0).contains(&temp_c));
            assert!((30.0..60.0).contains(&humidity_pct));
            assert!((900.0..1100.0).contains(&pressure_hpa));
        } else {
            panic!("wrong reading type");
        }
    }

    #[test]
    fn pi_hardware_unavailable_without_feature() {
        let r = pi::reed_on_gpio(5);
        match r {
            Err(SensorError::HardwareUnavailable) => {}
            Err(SensorError::Driver(_)) => {} // when feature=pi-hw build path is taken
            other => panic!("unexpected: {:?}", other.map(|_| "ok")),
        }
    }

    #[test]
    fn reflex_emits_occupancy_change() {
        let r = Reflex::new(ReflexConfig::default());
        let zone = "kitchen".to_string();
        let absent = FeatureVector([0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
        let present = FeatureVector([0.9, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);

        let e1 = r.evaluate(&absent, &zone);
        assert!(matches!(
            e1.as_slice(),
            [SensingEvent::Occupancy { occupied: false, .. }]
        ));
        // Same state: no emit.
        let e2 = r.evaluate(&absent, &zone);
        assert!(e2.is_empty());
        // Transition: emit.
        let e3 = r.evaluate(&present, &zone);
        assert!(e3
            .iter()
            .any(|e| matches!(e, SensingEvent::Occupancy { occupied: true, .. })));
    }

    #[test]
    fn reflex_emits_fall_on_transition_only() {
        let r = Reflex::new(ReflexConfig::default());
        let zone = "bedroom".to_string();
        let standing = FeatureVector([0.9, 0.0, 0.66, 0.65, 0.0, 0.5, 0.0, 0.55]);
        let fallen = FeatureVector([0.9, 0.0, 0.66, 0.65, 0.0, 0.5, 1.0, 0.55]);

        let _ = r.evaluate(&standing, &zone);
        let e2 = r.evaluate(&fallen, &zone);
        assert!(e2.iter().any(|e| matches!(e, SensingEvent::Fall { .. })));
        // Still fallen — no new fall event.
        let e3 = r.evaluate(&fallen, &zone);
        assert!(!e3.iter().any(|e| matches!(e, SensingEvent::Fall { .. })));
    }

    #[test]
    fn reflex_emits_vitals_when_occupied_and_in_range() {
        let r = Reflex::new(ReflexConfig::default());
        let zone = "living".to_string();
        let fv = FeatureVector([0.9, 0.0, 0.66, 0.65, 0.0, 0.5, 0.0, 0.55]);
        let events = r.evaluate(&fv, &zone);
        assert!(events.iter().any(|e| matches!(
            e,
            SensingEvent::Vitals { heart_rate_bpm, breathing_bpm, .. }
                if (*heart_rate_bpm - 78.0).abs() < 0.1 && (*breathing_bpm - 19.8).abs() < 0.1
        )));
    }
}
