//! Editable pattern data + step-based scheduling.
//!
//! The piano-roll editor mutates a `Pattern` (notes with grid-aligned times in
//! *steps*); the transport converts those steps to per-sample `NoteEvent`s.
//! Note times are `f64` so tuplets (triplets, quintuplets, ...) can land on
//! non-integer step positions. Each `Note` carries a stable unique `id` so
//! the editor can hold a selection set that stays valid across edits.

use i_am_dsp::prelude::NoteEvent;

use crate::tuning::REF_NOTE;

/// Grid resolution: how many steps per quarter note (4 = 16th notes).
pub const STEPS_PER_BEAT: usize = 4;
/// Default tempo when nothing is loaded.
pub const DEFAULT_TEMPO: f32 = 120.0;

/// A note in the pattern. `pitch_index` is our uniform EDO degree
/// (0 == reference C4); times are in grid *steps* (fractional for tuplets).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Note {
    /// Stable identity, used by the editor for selections.
    pub id: u64,
    pub pitch_index: i32,
    pub start_step: f64,
    pub length_steps: f64,
    pub velocity: f32,
    /// Optional user label; empty means "show the auto note name".
    pub label: String,
}

fn default_beats() -> u32 {
    4
}
fn default_unit() -> u32 {
    4
}

/// The editable loop: a list of notes plus its total length in steps and the
/// time signature (beats per bar / beat unit denominator).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Pattern {
    pub notes: Vec<Note>,
    pub total_steps: usize,
    /// Time signature numerator: beats per bar (default 4).
    #[serde(default = "default_beats")]
    pub beats_per_bar: u32,
    /// Time signature denominator: the beat unit (2 = half, 4 = quarter,
    /// 8 = eighth, 16 = sixteenth; default 4).
    #[serde(default = "default_unit")]
    pub beat_unit: u32,
    next_id: u64,
}

/// Grid steps in one beat for the given beat unit (denominator).
/// The base grid is 16th notes (STEPS_PER_BEAT = 4 steps per quarter), so:
/// unit 2 -> 8 steps, 4 -> 4 steps, 8 -> 2 steps, 16 -> 1 step.
pub fn steps_per_beat(unit: u32) -> f64 {
    STEPS_PER_BEAT as f64 * 4.0 / (unit.max(1) as f64)
}

/// Grid steps in one full bar for a pattern (always an integer for valid units).
pub fn bar_steps(pattern: &Pattern) -> f64 {
    pattern.beats_per_bar.max(1) as f64 * steps_per_beat(pattern.beat_unit)
}

impl Pattern {
    /// A starter melody: a looping C-major-ish arpeggio, one note per beat,
    /// 8 beats = 32 steps.
    pub fn demo() -> Self {
        let pitches = [9, 12, 16, 12, 9, 7, 4, 0];
        let mut p = Self::empty(16 * STEPS_PER_BEAT);
        for (i, &pitch) in pitches.iter().enumerate() {
            let id = p.take_id();
            p.notes.push(Note {
                id,
                pitch_index: pitch,
                start_step: (i * STEPS_PER_BEAT) as f64,
                length_steps: STEPS_PER_BEAT as f64,
                velocity: 0.8,
                label: String::new(),
            });
        }
        p
    }

    /// A fresh, empty pattern of the given length (in steps).
    pub fn empty(total_steps: usize) -> Self {
        Self {
            notes: Vec::new(),
            total_steps: total_steps.max(1),
            beats_per_bar: 4,
            beat_unit: 4,
            next_id: 0,
        }
    }

    /// Allocate a fresh id.
    fn take_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Build an editor-created note and push it. Notes are NOT clamped to the
    /// clip length: they may extend past `total_steps` (they just won't play).
    pub fn add_note(
        &mut self,
        pitch_index: i32,
        start_step: f64,
        length_steps: f64,
        velocity: f32,
    ) -> u64 {
        let id = self.take_id();
        self.notes.push(Note {
            id,
            pitch_index,
            start_step,
            length_steps: length_steps.max(1.0),
            velocity,
            label: String::new(),
        });
        id
    }

    /// Set the custom label of the note with the given id.
    pub fn set_label(&mut self, id: u64, label: String) {
        if let Some(n) = self.notes.iter_mut().find(|n| n.id == id) {
            n.label = label;
        }
    }

    /// Add a copy of `src` (keeping its label) at a new position, returning the new id.
    pub fn duplicate(&mut self, src: &Note, start_step: f64, length_steps: f64) -> u64 {
        let id = self.take_id();
        let mut clone = src.clone();
        clone.id = id;
        clone.start_step = start_step;
        clone.length_steps = length_steps.max(1.0);
        self.notes.push(clone);
        id
    }

    /// Change the clip length (in steps). Notes are left untouched: notes that
    /// fall outside the loop simply don't play, so editing the length (even by
    /// typing intermediate values) never destroys notes.
    pub fn set_len(&mut self, steps: usize) {
        self.total_steps = steps.max(1);
    }
}

/// Samples per grid step at the given sample rate and tempo.
pub fn samples_per_step(sample_rate: usize, tempo: f32) -> f64 {
    // Clamp so a bad (<= 0) tempo from an imported project can never produce a
    // zero/negative step length (which would panic on integer % / in the
    // audio callback and poison the engine mutex).
    let tempo = tempo.max(1.0);
    sample_rate as f64 * 60.0 / tempo as f64 / STEPS_PER_BEAT as f64
}

/// Length of one full loop in samples.
pub fn loop_samples(pattern: &Pattern, sample_rate: usize, tempo: f32) -> usize {
    (pattern.total_steps as f64 * samples_per_step(sample_rate, tempo))
        .round()
        .max(1.0) as usize
}

/// The absolute loop sample at the start of a given (possibly fractional) step.
pub fn sample_of_step(step: f64, sample_rate: usize, tempo: f32) -> usize {
    (step * samples_per_step(sample_rate, tempo)).round() as usize
}

/// Build the sorted list of (sample, event) for one full loop, given its length
/// in samples. NoteOn/NoteOff land on exact sample positions, so tuplets with
/// fractional step times are played correctly. Events at or past `loop_samples`
/// are dropped (notes beyond the clip don't play); a note ringing past the wrap
/// is cut off on the last loop sample.
pub fn build_events(
    pattern: &Pattern,
    sample_rate: usize,
    tempo: f32,
    loop_samples: usize,
) -> Vec<(usize, NoteEvent)> {
    let sps = samples_per_step(sample_rate, tempo);
    let last = loop_samples.saturating_sub(1);
    let mut ev = Vec::new();
    for n in &pattern.notes {
        let note = (REF_NOTE + n.pitch_index).max(0) as usize;
        let start = (n.start_step * sps).round() as usize;
        if start < loop_samples {
            ev.push((
                start,
                NoteEvent::NoteOn {
                    time: start,
                    channel: 0,
                    note,
                    velocity: n.velocity,
                },
            ));
            let end = ((n.start_step + n.length_steps) * sps).round() as usize;
            let off = end.min(last);
            ev.push((
                off,
                NoteEvent::NoteOff {
                    time: off,
                    channel: 0,
                    note,
                    velocity: n.velocity,
                },
            ));
        }
    }
    ev.sort_by_key(|(s, _)| *s);
    ev
}
