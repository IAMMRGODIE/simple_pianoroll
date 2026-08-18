//! The real-time audio engine: drives `i_am_dsp`'s polyphonic `Adsr` generator
//! from a looping song, sample by sample, inside a `cpal` output callback.
//!
//! UI and audio threads share the engine through `Arc<Mutex<Engine>>` (the same
//! pattern `i_am_dsp`'s `DspDemo` uses). The audio callback locks once per
//! buffer and pulls one stereo sample per call.

use std::sync::{Arc, Mutex};

use anyhow::anyhow;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use i_am_dsp::prelude::*;

use crate::pattern::{self, Pattern};
use crate::tuning::{TuningKind, TuningWrapper, REF_NOTE};

/// A per-sample `ProcessContext` that hands the scheduled events to `Adsr`.
struct SeqContext {
    info: ProcessInfos,
    events: Vec<NoteEvent>,
}

impl ProcessContext for SeqContext {
    fn infos(&self) -> &ProcessInfos {
        &self.info
    }
    fn next_event(&mut self) -> Option<NoteEvent> {
        self.events.pop()
    }
    fn send_event(&mut self, ev: NoteEvent) {
        self.events.push(ev)
    }
    fn events(&self) -> &[NoteEvent] {
        &self.events
    }
}

/// A short tone played to preview a pitch while editing.
/// Kept as a small list so rapidly re-previewed pitches each get released.
struct PreviewNote {
    note: usize,
    started: bool,
    remaining: usize,
}

/// Shared engine state, read/written by the audio callback and the UI thread.
pub struct Engine {
    tuning_kind: TuningKind,
    generator: Adsr<WaveTableSmoother, TuningWrapper, 2>,
    sample_rate: usize,
    tempo: f32,
    pattern: Pattern,
    sample_counter: usize,
    loop_samples: usize,
    playing: bool,
    stop_pending: bool,
    preview: Vec<PreviewNote>,
    events_buf: Vec<NoteEvent>,
}

fn build_generator(
    sample_rate: usize,
    kind: TuningKind,
) -> Adsr<WaveTableSmoother, TuningWrapper, 2> {
    let tables: Vec<Box<dyn WaveTable + Send + Sync>> = vec![
        Box::new(SineWave),
        Box::new(TriangleWave),
        Box::new(SawWave),
        Box::new(SquareWave),
    ];
    let smoother = WaveTableSmoother::new(tables, 0.0);
    Adsr::new(smoother, kind.make(), sample_rate)
}

impl Engine {
    pub fn new(sample_rate: usize, kind: TuningKind) -> Self {
        let pattern = Pattern::demo();
        let tempo = pattern::DEFAULT_TEMPO;
        let loop_samples = pattern::loop_samples(&pattern, sample_rate, tempo);
        Self {
            tuning_kind: kind,
            generator: build_generator(sample_rate, kind),
            sample_rate,
            tempo,
            pattern,
            loop_samples,
            sample_counter: 0,
            playing: true,
            stop_pending: false,
            preview: Vec::new(),
            events_buf: Vec::new(),
        }
    }

    /// Advance one sample and return the stereo output.
    fn next_sample(&mut self) -> [f32; 2] {
        self.events_buf.clear();

        let stop = std::mem::replace(&mut self.stop_pending, false);
        if self.playing {
            pattern::sample_events_into(
                &self.pattern,
                self.sample_rate,
                self.tempo,
                self.sample_counter,
                &mut self.events_buf,
            );

            // A note ending exactly at total_steps never gets its NoteOff
            // because step == total_steps never occurs inside the loop; stop
            // such notes on the last sample so they don't sustain past the wrap.
            if self.sample_counter + 1 == self.loop_samples {
                for n in &self.pattern.notes {
                    if n.start_step + n.length_steps == self.pattern.total_steps {
                        let note = (REF_NOTE + n.pitch_index).max(0) as usize;
                        self.events_buf.push(NoteEvent::NoteOff {
                            time: self.sample_counter,
                            channel: 0,
                            note,
                            velocity: n.velocity,
                        });
                    }
                }
            }
        }
        if stop {
            self.events_buf.push(NoteEvent::ImmediateStop);
        }

        // Sound preview from the editor: each entry rings ~120 ms then releases.
        // Keeping a list means rapid pitch changes each get their own release.
        if !self.preview.is_empty() {
            for p in self.preview.iter_mut() {
                if !p.started {
                    self.events_buf.push(NoteEvent::NoteOn {
                        time: self.sample_counter,
                        channel: 0,
                        note: p.note,
                        velocity: 0.9,
                    });
                    p.started = true;
                }
            }
            let mut expiring: Vec<usize> = Vec::new();
            for p in self.preview.iter_mut() {
                if p.started {
                    p.remaining = p.remaining.saturating_sub(1);
                    if p.remaining == 0 {
                        expiring.push(p.note);
                    }
                }
            }
            self.preview.retain(|p| !p.started || p.remaining > 0);
            for note in expiring {
                self.events_buf.push(NoteEvent::NoteOff {
                    time: self.sample_counter,
                    channel: 0,
                    note,
                    velocity: 0.0,
                });
            }
        }

        let mut info = ProcessInfos::new();
        info.sample_rate = self.sample_rate;
        let mut ctx: Box<dyn ProcessContext> = Box::new(SeqContext {
            info,
            events: std::mem::take(&mut self.events_buf),
        });
        let out = self.generator.generate(&mut ctx);

        if self.playing {
            self.sample_counter = (self.sample_counter + 1) % self.loop_samples;
        }
        out
    }

    /// Play a short preview tone for `pitch_index` (re-triggers on pitch change).
    pub fn preview_note(&mut self, pitch_index: i32) {
        let note = (REF_NOTE + pitch_index).max(0) as usize;
        if self.preview.len() >= 24 {
            return; // don't let a busy drag pile up previews
        }
        self.preview.push(PreviewNote {
            note,
            started: false,
            remaining: (self.sample_rate as f32 * 0.12) as usize,
        });
    }

    /// Jump the transport to the given grid step.
    pub fn seek_to_step(&mut self, step: usize) {
        self.sample_counter = pattern::sample_of_step(step, self.sample_rate, self.tempo)
            % self.loop_samples;
        self.stop_pending = true; // don't let notes ring across a transport jump
    }

    pub fn set_tuning(&mut self, kind: TuningKind) {
        if self.tuning_kind == kind {
            return;
        }
        self.tuning_kind = kind;
        self.generator = build_generator(self.sample_rate, kind);
    }

    pub fn set_tempo(&mut self, bpm: f32) {
        self.tempo = bpm;
        self.loop_samples = pattern::loop_samples(&self.pattern, self.sample_rate, bpm);
    }

    pub fn set_playing(&mut self, playing: bool) {
        if !playing && self.playing {
            self.stop_pending = true;
        }
        self.playing = playing;
    }

    pub fn tuning_kind(&self) -> TuningKind {
        self.tuning_kind
    }
    pub fn tempo(&self) -> f32 {
        self.tempo
    }
    pub fn playing(&self) -> bool {
        self.playing
    }
    /// Read-only access to the editable pattern.
    pub fn pattern(&self) -> &Pattern {
        &self.pattern
    }
    /// Replace the whole pattern (e.g. load a demo) and resync the loop.
    pub fn set_pattern(&mut self, p: Pattern) {
        self.loop_samples = pattern::loop_samples(&p, self.sample_rate, self.tempo);
        self.pattern = p;
    }
    /// The grid step the playhead is currently on (for the transport line).
    pub fn playhead_step(&self) -> usize {
        pattern::step_at(self.sample_counter, self.sample_rate, self.tempo)
            % self.pattern.total_steps.max(1)
    }
}

/// Open the default output device and start streaming. Returns the shared
/// engine handle plus the live stream (which must be kept alive).
pub fn start(kind: TuningKind) -> (Arc<Mutex<Engine>>, Option<cpal::Stream>) {
    let built = (|| -> anyhow::Result<(Arc<Mutex<Engine>>, cpal::Stream)> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| anyhow!("no output device available"))?;
    let config: cpal::StreamConfig = device.default_output_config()?.into();
    let sample_rate = config.sample_rate as usize;

    let engine = Arc::new(Mutex::new(Engine::new(sample_rate, kind)));
    let stream_engine = Arc::clone(&engine);

    let stream = device.build_output_stream(
        config,
        move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
            let mut e = match stream_engine.lock() {
                Ok(e) => e,
                Err(_) => return,
            };
            for chunk in data.chunks_mut(2) {
                if chunk.len() != 2 {
                    break;
                }
                let out = e.next_sample();
                chunk[0] = out[0];
                chunk[1] = out[1];
            }
        },
        move |err| eprintln!("audio stream error: {err}"),
        None,
    )?;
    stream.play()?;
    Ok((engine, stream))
    })();
    match built {
        Ok((engine, stream)) => (engine, Some(stream)),
        Err(e) => {
            eprintln!("WARNING: audio unavailable, running silent: {e:#}");
            (Arc::new(Mutex::new(Engine::new(48_000, kind))), None)
        }
    }
}