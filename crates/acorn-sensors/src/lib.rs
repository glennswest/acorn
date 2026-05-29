//! `acorn-sensors` — Reflex rules over the distributed sensing event stream.
//!
//! Acorn's deployment model is one aggregator daemon (`acornd`, e.g. on a Pi
//! CM5) that receives feature packets over UDP from a fleet of ESP32 sensor
//! nodes. The Pi has no local sensors of its own. This crate therefore
//! contains no hardware drivers; it provides only the [`Reflex`] state
//! machine that turns the
//! [`acorn_proto::event::FeatureVector`] from each incoming packet into
//! semantic [`acorn_proto::event::SensingEvent`]s.

#![forbid(unsafe_code)]

use acorn_proto::event::{FeatureVector, SensingEvent, Zone};
use parking_lot::Mutex;

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

#[cfg(test)]
mod tests {
    use super::*;

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
