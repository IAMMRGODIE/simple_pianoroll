//! MIDI import via the `midly` crate (robust SMF parsing).
//!
//! Converts: ticks -> our step grid (4 steps per quarter), MIDI note numbers ->
//! pitch_index (MIDI 48 == C4 == pitch_index 0). Tracks are kept separate so
//! the UI can let the user pick which tracks to import and whether each track
//! becomes its own clip or everything merges into one clip.

use std::collections::HashMap;

use anyhow::{anyhow, bail, Result};
use midly::{MetaMessage, MidiMessage, Smf, Timing, TrackEventKind};

use crate::pattern::Pattern;

/// One parsed MIDI track (name + finished notes).
pub struct ImportedTrack {
    pub name: String,
    /// (start_tick, midi_note, velocity 0..1, length_ticks)
    pub notes: Vec<(u64, u8, f32, u64)>,
    /// UI checkbox state: whether this track should be imported.
    pub selected: bool,
}

/// Parsed file, kept in memory so the import window can pick tracks.
pub struct ImportData {
    pub tracks: Vec<ImportedTrack>,
    pub tempo: f32,
    pub beats_per_bar: u32,
    pub beat_unit: u32,
    pub ppq: u32,
}

/// Parse an SMF file into per-track note lists.
pub fn parse(bytes: &[u8]) -> Result<ImportData> {
    let smf = Smf::parse(bytes).map_err(|e| anyhow!("invalid MIDI file: {e}"))?;
    let ppq = match smf.header.timing {
        Timing::Metrical(ppq) => ppq.as_int() as u32,
        Timing::Timecode(..) => bail!("SMPTE time division is not supported"),
    };
    if ppq == 0 {
        bail!("time division is zero");
    }

    // First tempo / time-signature meta found across the file (in track order)
    // wins; format-1 files put them on the conductor track (track 0).
    let mut tempo: Option<f32> = None;
    let mut tsig: Option<(u8, u8)> = None;

    let mut tracks = Vec::new();
    for (ti, track) in smf.tracks.iter().enumerate() {
        let mut name = format!("Track {}", ti + 1);
        let mut tick: u64 = 0;
        let mut active: HashMap<u8, (u64, f32)> = HashMap::new();
        let mut notes: Vec<(u64, u8, f32, u64)> = Vec::new();
        let mut max_tick: u64 = 0;

        for ev in track {
            tick += ev.delta.as_int() as u64;
            max_tick = max_tick.max(tick);
            match ev.kind {
                TrackEventKind::Midi { message, .. } => match message {
                    MidiMessage::NoteOn { key, vel } => {
                        let key = key.as_int();
                        let vel = vel.as_int();
                        if vel == 0 {
                            if let Some((st, v)) = active.remove(&key) {
                                notes.push((st, key, v, tick.saturating_sub(st)));
                            }
                        } else {
                            active.insert(key, (tick, vel as f32 / 127.0));
                        }
                    }
                    MidiMessage::NoteOff { key, .. } => {
                        let key = key.as_int();
                        if let Some((st, v)) = active.remove(&key) {
                            notes.push((st, key, v, tick.saturating_sub(st)));
                        }
                    }
                    _ => {}
                },
                TrackEventKind::Meta(MetaMessage::TrackName(bytes)) => {
                    name = String::from_utf8_lossy(bytes).trim().to_string();
                    if name.is_empty() {
                        name = format!("Track {}", ti + 1);
                    }
                }
                TrackEventKind::Meta(MetaMessage::Tempo(uspq)) => {
                    if tempo.is_none() && uspq.as_int() > 0 {
                        tempo = Some(60_000_000.0 / uspq.as_int() as f32);
                    }
                }
                TrackEventKind::Meta(MetaMessage::TimeSignature(nn, dd, _, _)) if tsig.is_none() => {
                    tsig = Some((nn, dd));
                }
                _ => {}
            }
        }
        // close notes still ringing at the end of the track
        for (n, (st, v)) in active.drain() {
            notes.push((st, n, v, max_tick.saturating_sub(st)));
        }
        notes.sort_by_key(|(st, _, _, _)| *st);
        tracks.push(ImportedTrack {
            name,
            notes,
            selected: true,
        });
    }
    if tracks.is_empty() {
        bail!("no tracks found in the file");
    }

    let tempo = tempo.unwrap_or(120.0).clamp(40.0, 300.0);
    let (nn, dd) = tsig.unwrap_or((4, 2)); // 4/4: denominator exponent 2 -> unit 4
    Ok(ImportData {
        tracks,
        tempo,
        beats_per_bar: nn.max(1) as u32,
        beat_unit: (1u32 << dd).clamp(2, 16),
        ppq,
    })
}

fn clip_len(p: &Pattern) -> usize {
    let max_end = p
        .notes
        .iter()
        .map(|n| n.start_step + n.length_steps)
        .fold(0.0f64, f64::max);
    (((max_end / 4.0).ceil() as usize) * 4).max(8)
}

/// Build clip patterns from the currently-selected tracks.
/// `separate`: one clip per selected track; otherwise all merge into one clip.
/// Returns (patterns, clip names).
pub fn build_clips(data: &ImportData, separate: bool) -> (Vec<Pattern>, Vec<String>) {
    let to_step = |ticks: u64| ticks as f64 * 4.0 / data.ppq as f64;
    let add_notes = |p: &mut Pattern, t: &ImportedTrack| {
        for (st, n, vel, len) in &t.notes {
            let pitch = *n as i32 - 48; // MIDI 48 == C4 == pitch_index 0
            p.add_note(pitch, to_step(*st), to_step(*len).max(0.5), vel.clamp(0.0, 1.0));
        }
    };
    let selected: Vec<&ImportedTrack> = data.tracks.iter().filter(|t| t.selected).collect();

    if separate {
        let mut pats = Vec::new();
        let mut names = Vec::new();
        for t in selected {
            let mut p = Pattern::empty(8);
            p.beats_per_bar = data.beats_per_bar;
            p.beat_unit = data.beat_unit;
            add_notes(&mut p, t);
            p.total_steps = clip_len(&p);
            pats.push(p);
            names.push(t.name.clone());
        }
        (pats, names)
    } else {
        let mut p = Pattern::empty(8);
        p.beats_per_bar = data.beats_per_bar;
        p.beat_unit = data.beat_unit;
        for t in selected {
            add_notes(&mut p, t);
        }
        p.total_steps = clip_len(&p);
        (vec![p], vec!["Imported".to_string()])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_simple_smf() {
        // format-0, 1 track, 480 ticks per quarter; one C4 (MIDI 60) note of one
        // quarter, then end-of-track.
        let mut file = Vec::new();
        file.extend_from_slice(b"MThd");
        file.extend_from_slice(&[0, 0, 0, 6, 0, 0, 0, 1, 0x01, 0xE0]);
        let track: [u8; 13] = [
            0x00, 0x90, 0x3C, 0x7F, // delta 0, note on C4 vel 127
            0x83, 0x60, 0x80, 0x3C, 0x7F, // delta 480, note off
            0x00, 0xFF, 0x2F, 0x00, // end of track
        ];
        file.extend_from_slice(b"MTrk");
        file.extend_from_slice(&(track.len() as u32).to_be_bytes());
        file.extend_from_slice(&track);

        let data = parse(&file).unwrap();
        assert_eq!(data.ppq, 480);
        assert_eq!(data.tracks.len(), 1);
        assert_eq!(data.tracks[0].notes, vec![(0, 60, 1.0, 480)]);
        assert_eq!(data.tempo, 120.0);
        assert_eq!(data.beats_per_bar, 4);
        assert_eq!(data.beat_unit, 4);

        let (clips, _) = build_clips(&data, false);
        assert_eq!(clips.len(), 1);
        assert_eq!(clips[0].notes.len(), 1);
        let n = &clips[0].notes[0];
        assert_eq!(n.pitch_index, 12); // MIDI 60 - 48
        assert_eq!(n.start_step, 0.0);
        assert_eq!(n.length_steps, 4.0); // one quarter = 4 steps
    }

    #[test]
    fn separate_tracks_become_separate_clips() {
        let mut file = Vec::new();
        file.extend_from_slice(b"MThd");
        file.extend_from_slice(&[0, 0, 0, 6, 0, 1, 0, 2, 0x01, 0xE0]); // format 1, 2 tracks
        let t1: [u8; 8] = [
            0x00, 0x90, 0x3C, 0x7F, // C4 on
            0x00, 0x80, 0x3C, 0x00, // off (delta 0)
        ];
        let t2: [u8; 8] = [
            0x00, 0x90, 0x40, 0x7F, // C5 on
            0x00, 0x80, 0x40, 0x00,
        ];
        for t in [&t1, &t2] {
            file.extend_from_slice(b"MTrk");
            file.extend_from_slice(&(t.len() as u32).to_be_bytes());
            file.extend_from_slice(t);
        }
        let data = parse(&file).unwrap();
        assert_eq!(data.tracks.len(), 2);
        let (clips, names) = build_clips(&data, true);
        assert_eq!(clips.len(), 2);
        assert_eq!(names, vec!["Track 1", "Track 2"]);
        assert_eq!(clips[0].notes[0].pitch_index, 12);
        assert_eq!(clips[1].notes[0].pitch_index, 16);
        // merged mode: both notes in one clip
        let (merged, _) = build_clips(&data, false);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].notes.len(), 2);
    }
}
