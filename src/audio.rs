//! The real-time audio engine: drives `i_am_dsp`'s polyphonic `Adsr` generator
//! from a looping song, sample by sample, inside a `cpal` output callback.
//!
//! UI and audio threads share the engine through `Arc<Mutex<Engine>>` (the same
//! pattern `i_am_dsp`'s `DspDemo` uses). The audio callback locks once per
//! buffer and pulls one stereo sample per call.

use std::path::PathBuf;
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

/// Selectable oscillator waveform for the track's voice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Waveform {
    Sine,
    Triangle,
    Saw,
    Square,
}

/// Simple single-voice timbre: waveform + ADSR + gain.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Timbre {
    pub waveform: Waveform,
    pub attack: f32,
    pub hold: f32,
    pub decay: f32,
    pub sustain: f32,
    pub release: f32,
    pub gain: f32,
}

impl Default for Timbre {
    fn default() -> Self {
        Self {
            waveform: Waveform::Sine,
            attack: 10.0,
            hold: 100.0,
            decay: 100.0,
            sustain: 1.0,
            release: 100.0,
            gain: 0.8,
        }
    }
}

/// One slot in the track's effect chain (enabled + wet amount).
struct EffectSlot {
    name: &'static str,
    on: bool,
    mix: f32,
    effect: Box<dyn Effect>,
}

/// Shared engine state, read/written by the audio callback and the UI thread.
pub struct Engine {
    tuning_kind: TuningKind,
    generator: Adsr<OscillatorSmoother<2>, TuningWrapper, 2>,
    using_sample: bool,
    sample_path: Option<PathBuf>,
    sample_rate: usize,
    tempo: f32,
    pattern: Pattern,
    sample_counter: usize,
    loop_samples: usize,
    playing: bool,
    stop_pending: bool,
    preview: Vec<PreviewNote>,
    events_buf: Vec<NoteEvent>,
    timbre: Timbre,
    effects: Vec<EffectSlot>,
}

/// Build the oscillator for a generated waveform (boxed so it can be swapped
/// for a loaded sample).
fn wave_box(wave: Waveform) -> Box<dyn Oscillator<2> + Send + Sync> {
    let table: Box<dyn WaveTable + Send + Sync> = match wave {
        Waveform::Sine => Box::new(SineWave),
        Waveform::Triangle => Box::new(TriangleWave),
        Waveform::Saw => Box::new(SawWave),
        Waveform::Square => Box::new(SquareWave),
    };
    Box::new(WaveTableSmoother::new(vec![table], 0.0))
}

/// Build the generator: its oscillator is either the selected waveform or a
/// loaded sample (re-loaded from `sample_path`, since `Sampler` isn't Clone).
fn build_generator(
    sample_rate: usize,
    kind: TuningKind,
    wave: Waveform,
    using_sample: bool,
    sample_path: Option<PathBuf>,
) -> Adsr<OscillatorSmoother<2>, TuningWrapper, 2> {
    let osc: Box<dyn Oscillator<2> + Send + Sync> = if using_sample {
        match &sample_path {
            Some(path) => {
                let mut sm = Sampler::<2>::new(sample_rate);
                if let Err(e) = sm.load_from_file(path) {
                    eprintln!("WARNING: failed to load sample: {e}");
                    wave_box(wave)
                } else {
                    Box::new(sm)
                }
            }
            None => wave_box(wave),
        }
    } else {
        wave_box(wave)
    };
    let smoother = OscillatorSmoother::new(vec![osc], 0.0);
    Adsr::new(smoother, kind.make(), sample_rate)
}

impl Engine {
    pub fn new(sample_rate: usize, kind: TuningKind) -> Self {
        let pattern = Pattern::demo();
        let tempo = pattern::DEFAULT_TEMPO;
        let loop_samples = pattern::loop_samples(&pattern, sample_rate, tempo);
        let timbre = Timbre::default();
        let generator = build_generator(sample_rate, kind, timbre.waveform, false, None);
        let effects: Vec<EffectSlot> = vec![
            EffectSlot {
                name: "Lowpass",
                on: false,
                mix: 1.0,
                effect: Box::new(Lowpass::<2>::new(sample_rate, 2000.0, Biquad::<2>::Q1)),
            },
            EffectSlot {
                name: "Delay",
                on: false,
                mix: 0.5,
                effect: Box::new(Delay::new((), 65536, 80.0, sample_rate)),
            },
        ];
        Self {
            tuning_kind: kind,
            generator,
            sample_rate,
            tempo,
            pattern,
            loop_samples,
            sample_counter: 0,
            playing: true,
            stop_pending: false,
            preview: Vec::new(),
            events_buf: Vec::new(),
            timbre,
            effects,
            using_sample: false,
            sample_path: None,
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
        let mut out = self.generator.generate(&mut ctx);

        // single-track effect chain (dry/wet mix per slot)
        for slot in self.effects.iter_mut() {
            if slot.on {
                let dry = out;
                slot.effect.process(&mut out, &[], &mut ctx);
                let m = slot.mix;
                out[0] = dry[0] * (1.0 - m) + out[0] * m;
                out[1] = dry[1] * (1.0 - m) + out[1] * m;
            }
        }

        if self.playing {
            self.sample_counter = (self.sample_counter + 1) % self.loop_samples;
        }
        out
    }

    // ---- timbre ----
    pub fn timbre(&self) -> Timbre {
        self.timbre
    }

    pub fn set_timbre(&mut self, t: Timbre) {
        if t.waveform != self.timbre.waveform && !self.using_sample {
            self.generator = build_generator(
                self.sample_rate,
                self.tuning_kind,
                t.waveform,
                self.using_sample,
                self.sample_path.clone(),
            );
        }
        self.timbre = t;
        self.generator.attack_time = t.attack;
        self.generator.hold_time = t.hold;
        self.generator.decay_time = t.decay;
        self.generator.sustain_level = t.sustain;
        self.generator.release_time = t.release;
        self.generator.gain = t.gain;
    }

    // ---- sample source ----
    pub fn using_sample(&self) -> bool {
        self.using_sample
    }
    pub fn sample_path(&self) -> Option<PathBuf> {
        self.sample_path.clone()
    }

    /// Load an audio file as the track's sound source (a resampler/sampler).
    pub fn load_sample(&mut self, path: impl AsRef<std::path::Path>) -> bool {
        let path = path.as_ref().to_path_buf();
        self.sample_path = Some(path);
        self.using_sample = true;
        self.generator = build_generator(
            self.sample_rate,
            self.tuning_kind,
            self.timbre.waveform,
            true,
            self.sample_path.clone(),
        );
        true
    }

    /// Switch back to the selected generated waveform.
    pub fn use_wave(&mut self) {
        self.using_sample = false;
        self.generator = build_generator(
            self.sample_rate,
            self.tuning_kind,
            self.timbre.waveform,
            false,
            None,
        );
    }

    // ---- effects ----
    pub fn effect_count(&self) -> usize {
        self.effects.len()
    }
    pub fn effect_name(&self, i: usize) -> &'static str {
        self.effects.get(i).map(|s| s.name).unwrap_or("")
    }
    pub fn effect_on(&self, i: usize) -> bool {
        self.effects.get(i).map(|s| s.on).unwrap_or(false)
    }
    pub fn effect_mix(&self, i: usize) -> f32 {
        self.effects.get(i).map(|s| s.mix).unwrap_or(0.0)
    }
    pub fn set_effect_on(&mut self, i: usize, on: bool) {
        if let Some(s) = self.effects.get_mut(i) {
            s.on = on;
        }
    }
    pub fn set_effect_mix(&mut self, i: usize, mix: f32) {
        if let Some(s) = self.effects.get_mut(i) {
            s.mix = mix.clamp(0.0, 1.0);
        }
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
        self.generator = build_generator(self.sample_rate, kind, self.timbre.waveform, self.using_sample, self.sample_path.clone());
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