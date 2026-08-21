//! Editable pattern data + step-based scheduling.
//!
//! The piano-roll editor mutates a `Pattern` (notes with grid-aligned times in
//! *steps*); the transport converts those steps to per-sample `NoteEvent`s.
//! Each `Note` carries a stable unique `id` so the editor can hold a selection
//! set that stays valid across add/remove/move operations.

use i_am_dsp::prelude::NoteEvent;

use crate::tuning::REF_NOTE;

/// Grid resolution: how many steps per beat (4 = 16th notes).
pub const STEPS_PER_BEAT: usize = 4;
/// Steps in a full bar (4/4): 4 beats.
pub const BAR_STEPS: usize = 4 * STEPS_PER_BEAT;
/// Default tempo when nothing is loaded.
pub const DEFAULT_TEMPO: f32 = 120.0;

/// A note in the pattern. `pitch_index` is our uniform EDO degree
/// (0 == reference C4); times are in grid *steps*.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Note {
    /// Stable identity, used by the editor for selections.
    pub id: u64,
    pub pitch_index: i32,
    pub start_step: usize,
    pub length_steps: usize,
    pub velocity: f32,
    /// Optional user label; empty means "show the auto note name".
    pub label: String,
}

/// The editable loop: a list of notes plus its total length in steps.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Pattern {
    pub notes: Vec<Note>,
    pub total_steps: usize,
    next_id: u64,
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
                start_step: i * STEPS_PER_BEAT,
                length_steps: STEPS_PER_BEAT,
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
            next_id: 0,
        }
    }

    /// Allocate a fresh id.
    fn take_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Build an editor-created note and push it.
    pub fn add_note(
        &mut self,
        pitch_index: i32,
        start_step: usize,
        length_steps: usize,
        velocity: f32,
    ) -> u64 {
        let id = self.take_id();
        self.notes.push(Note {
            id,
            pitch_index,
            start_step,
            length_steps: length_steps.max(1),
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
    pub fn duplicate(&mut self, src: &Note, start_step: usize, length_steps: usize) -> u64 {
        let id = self.take_id();
        let mut clone = src.clone();
        clone.id = id;
        clone.start_step = start_step;
        clone.length_steps = length_steps.max(1);
        self.notes.push(clone);
        id
    }

    /// Change the clip length (in steps), trimming notes that would overflow.
    pub fn set_len(&mut self, steps: usize) {
        let st = steps.max(1);
        for n in self.notes.iter_mut() {
            if n.start_step >= st {
                n.start_step = st - 1;
                n.length_steps = 1;
            } else {
                let end = (n.start_step + n.length_steps).min(st);
                n.length_steps = end.max(n.start_step + 1) - n.start_step;
            }
        }
        self.total_steps = st;
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
    (pattern.total_steps as f64 * samples_per_step(sample_rate, tempo)).round().max(1.0) as usize
}

/// The grid step the given absolute loop sample is on.
pub fn step_at(sample: usize, sample_rate: usize, tempo: f32) -> usize {
    (sample as f64 / samples_per_step(sample_rate, tempo)).floor() as usize
}

/// The absolute loop sample at the start of a given step.
pub fn sample_of_step(step: usize, sample_rate: usize, tempo: f32) -> usize {
    (step as f64 * samples_per_step(sample_rate, tempo)).round() as usize
}

/// Fill `out` with the NoteEvents that fire at absolute loop `sample`.
pub fn sample_events_into(
    pattern: &Pattern,
    sample_rate: usize,
    tempo: f32,
    sample: usize,
    out: &mut Vec<NoteEvent>,
) {
    let step = step_at(sample, sample_rate, tempo);
    for n in &pattern.notes {
        let note = (REF_NOTE + n.pitch_index).max(0) as usize;
        if n.start_step == step {
            out.push(NoteEvent::NoteOn {
                time: sample,
                channel: 0,
                note,
                velocity: n.velocity,
            });
        }
        if n.start_step + n.length_steps == step {
            out.push(NoteEvent::NoteOff {
                time: sample,
                channel: 0,
                note,
                velocity: n.velocity,
            });
        }
    }
}