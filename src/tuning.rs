//! Runtime-switchable tuning.
//!
//! `Adsr` from `i_am_dsp` only requires a `Tuning` impl, so we wrap a boxed
//! `dyn Tuning` in a small `TuningWrapper` and swap the concrete tuning at
//! runtime — no changes to upstream needed.

use i_am_dsp::prelude::*;

/// Reference note number sent to `get_frequency` when `pitch_index == 0`.
/// In `i_am_dsp`'s numbering 48 == C4 (for `EqualTemperament`).
pub const REF_NOTE: i32 = 48;

/// The set of tunings the UI offers. Selecting one also sets the number of
/// rows per octave on the piano-roll grid in later milestones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TuningKind {
    Equal12,
    Edo7,
    Edo12,
    Edo19,
    Edo24,
    Edo31,
    Edo53,
    Just,
    Pythagorean,
    Custom,
}

impl TuningKind {
    pub fn all() -> &'static [TuningKind] {
        &[
            TuningKind::Equal12,
            TuningKind::Edo7,
            TuningKind::Edo12,
            TuningKind::Edo19,
            TuningKind::Edo24,
            TuningKind::Edo31,
            TuningKind::Edo53,
            TuningKind::Just,
            TuningKind::Pythagorean,
            TuningKind::Custom,
        ]
    }

    pub fn label(self) -> &'static str {
        match self {
            TuningKind::Equal12 => "12-EDO (equal)",
            TuningKind::Edo7 => "7-EDO",
            TuningKind::Edo12 => "12-EDO",
            TuningKind::Edo19 => "19-EDO",
            TuningKind::Edo24 => "24-EDO",
            TuningKind::Edo31 => "31-EDO",
            TuningKind::Edo53 => "53-EDO",
            TuningKind::Just => "Just intonation",
            TuningKind::Pythagorean => "Pythagorean",
            TuningKind::Custom => "Custom scale",
        }
    }

    /// Number of rows per octave on the piano-roll grid.
    pub fn steps_per_octave(self) -> usize {
        match self {
            TuningKind::Equal12 | TuningKind::Edo12 => 12,
            TuningKind::Edo7 => 7,
            TuningKind::Edo19 => 19,
            TuningKind::Edo24 => 24,
            TuningKind::Edo31 => 31,
            TuningKind::Edo53 => 53,
            TuningKind::Just | TuningKind::Pythagorean => 12,
            TuningKind::Custom => 0,
        }
    }

    pub fn make(self) -> TuningWrapper {
        match self {
            TuningKind::Equal12 | TuningKind::Edo12 => TuningWrapper(Box::new(EqualTemperament)),
            TuningKind::Edo7 => TuningWrapper(Box::new(NEdoTuning::<7>)),
            TuningKind::Edo19 => TuningWrapper(Box::new(NEdoTuning::<19>)),
            TuningKind::Edo24 => TuningWrapper(Box::new(NEdoTuning::<24>)),
            TuningKind::Edo31 => TuningWrapper(Box::new(NEdoTuning::<31>)),
            TuningKind::Edo53 => TuningWrapper(Box::new(NEdoTuning::<53>)),
            TuningKind::Just => TuningWrapper(Box::new(JustIntonation)),
            TuningKind::Pythagorean => TuningWrapper(Box::new(PythagoreanTuning)),
            TuningKind::Custom => TuningWrapper(Box::new(CustomScale { ratios: Vec::new() })),
        }
    }
}

/// A dynamically chosen tuning, usable anywhere a `Tuning` is required.
pub struct TuningWrapper(Box<dyn Tuning + Send + Sync>);

impl Tuning for TuningWrapper {
    fn get_frequency(&self, note: f32) -> f32 {
        self.0.get_frequency(note)
    }
}

/// A user-defined scale: a list of frequency ratios relative to the root
/// (ratios[0] == 1.0). `pitch_index 0` is the root (C4 reference).
#[derive(Debug, Clone)]
pub struct CustomScale {
    pub ratios: Vec<f32>,
}

const C4_FREQ: f32 = 261.62558;

impl Tuning for CustomScale {
    fn get_frequency(&self, note: f32) -> f32 {
        let n = self.ratios.len() as i32;
        if n == 0 {
            return 0.0;
        }
        let steps = (note - REF_NOTE as f32).round() as i32; // = pitch_index
        let octave = (steps as f64 / n as f64).floor() as i32;
        let degree = steps.rem_euclid(n) as usize;
        C4_FREQ * self.ratios[degree] * 2.0f32.powf(octave as f32)
    }
}

/// Resolve a `TuningKind` to a concrete tuning, using the custom ratios for
/// `Custom`.
pub fn resolve(kind: TuningKind, custom_ratios: &[f32]) -> TuningWrapper {
    match kind {
        TuningKind::Custom => TuningWrapper(Box::new(CustomScale {
            ratios: custom_ratios.to_vec(),
        })),
        _ => kind.make(),
    }
}