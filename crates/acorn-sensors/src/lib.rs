//! `acorn-sensors` — physical sensor pipeline (feature-gated; optional on
//! non-Pi hosts).
//!
//! Phase 2: GPIO 5/6/13 (reed/PIR/vibration), I2C 0x48 (ADS1115),
//!          0x76 (BME280); 13 drift detectors; entropy anti-spoofing.
