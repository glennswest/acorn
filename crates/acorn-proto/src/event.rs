//! Sensing-event vocabulary — the Z Man integration boundary.
//!
//! These types are deliberately decoupled from the RuView wire format. The
//! appliance translates raw feature vectors / cognitive output into
//! [`SensingEvent`]s; Z Man's automation engine consumes them as triggers
//! without ever knowing about RVF, witness chains, or the UDP protocol.

use serde::{Deserialize, Serialize};

/// A logical sensing zone — maps to a Z Man area / room.
pub type Zone = String;

/// The 8-dim normalized feature vector (ADR-069 dimension table).
///
/// On the wire every component is clamped to `0.0..=1.0`. The accessor
/// methods de-normalize back to physical units.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FeatureVector(pub [f32; 8]);

impl FeatureVector {
    /// Dim 0 — presence score, normalized.
    pub fn presence(&self) -> f32 {
        self.0[0]
    }
    /// Dim 1 — motion energy, normalized.
    pub fn motion_energy(&self) -> f32 {
        self.0[1]
    }
    /// Dim 2 de-normalized to breathing rate in BPM (`× 30`).
    pub fn breathing_bpm(&self) -> f32 {
        self.0[2] * 30.0
    }
    /// Dim 3 de-normalized to heart rate in BPM (`× 120`).
    pub fn heart_rate_bpm(&self) -> f32 {
        self.0[3] * 120.0
    }
    /// Dim 4 — mean phase variance of the top-K subcarriers.
    pub fn phase_variance(&self) -> f32 {
        self.0[4]
    }
    /// Dim 5 de-normalized to an estimated person count (`× 4`, rounded).
    pub fn person_count(&self) -> u32 {
        (self.0[5] * 4.0).round() as u32
    }
    /// Dim 6 — fall flag (binary).
    pub fn fall_detected(&self) -> bool {
        self.0[6] >= 0.5
    }
    /// Dim 7 de-normalized to RSSI in dBm (`× 100 − 100`).
    pub fn rssi_dbm(&self) -> f32 {
        self.0[7] * 100.0 - 100.0
    }
}

/// A semantic sensing event emitted to downstream consumers (e.g. Z Man).
///
/// Serializes with an external `kind` tag, e.g. `{"kind":"fall","zone":"office"}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SensingEvent {
    /// Occupancy state of a zone changed.
    Occupancy {
        zone: Zone,
        occupied: bool,
        confidence: f32,
    },
    /// Motion detected in a zone.
    Motion { zone: Zone, energy: f32 },
    /// A fall was detected in a zone.
    Fall { zone: Zone },
    /// Vital signs estimate for a zone.
    Vitals {
        zone: Zone,
        heart_rate_bpm: f32,
        breathing_bpm: f32,
    },
    /// Vector-space regime change from the cognitive boundary analysis.
    RegimeChange { zone: Zone, fragility: f32 },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn denormalization_matches_adr069() {
        // breathing dim = 0.66 -> ~19.8 BPM; heart dim = 0.65 -> 78 BPM.
        let fv = FeatureVector([0.0, 0.0, 0.66, 0.65, 0.0, 0.5, 1.0, 0.55]);
        assert!((fv.breathing_bpm() - 19.8).abs() < 0.01);
        assert!((fv.heart_rate_bpm() - 78.0).abs() < 0.01);
        assert_eq!(fv.person_count(), 2);
        assert!(fv.fall_detected());
        assert!((fv.rssi_dbm() - (-45.0)).abs() < 0.01);
    }
}
