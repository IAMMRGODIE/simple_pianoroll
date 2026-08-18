//! Project save/load: a serde-serializable snapshot of everything that makes up
//! a "project" -- the pattern, transport, tuning, timbre, effects and editor
//! display settings -- so it can be written to / read back from a JSON file.

use serde::{Deserialize, Serialize};

use crate::audio::{Timbre, Waveform};
use crate::pattern::Pattern;
use crate::pianoroll::Scheme;
use crate::tuning::TuningKind;

/// One effect slot's enabled/mix state as stored on disk.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct EffectState {
    pub on: bool,
    pub mix: f32,
}

/// The full project state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Project {
    pub pattern: Pattern,
    pub tempo: f32,
    pub tuning: TuningKind,
    pub waveform: Waveform,
    pub timbre: Timbre,
    pub effects: Vec<EffectState>,
    pub note_names: String,
    pub tonic: i32,
    pub scheme: Scheme,
    pub snap: usize,
    pub clips: Vec<Pattern>,
    pub clip_names: Vec<String>,
    pub active_clip: usize,
}

/// Pretty-print the project as a JSON string.
pub fn to_json(p: &Project) -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(p)
}

/// Parse a project from a JSON string.
pub fn from_json(s: &str) -> Result<Project, serde_json::Error> {
    serde_json::from_str(s)
}