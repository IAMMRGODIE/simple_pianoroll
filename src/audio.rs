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

/// Length of the metronome tick (seconds).
const CLICK_LEN: f32 = 0.03;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Waveform {
    Sine,
    Triangle,
    Saw,
    Square,
}

/// Simple single-voice timbre: waveform + ADSR + gain.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
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
    generator: Box<dyn Generator>,
    using_sample: bool,
    sample_path: Option<PathBuf>,
    /// One-shot (no loop) playback for the loaded sample.
    sample_one_shot: bool,
    sample_rate: usize,
    tempo: f32,
    pattern: Pattern,
    sample_counter: usize,
    loop_samples: usize,
    playing: bool,
    play_start_pos: usize, // ruler position when playback last started
    stop_pending: bool,
    preview: Vec<PreviewNote>,
    events_buf: Vec<NoteEvent>,
    timbre: Timbre,
    effects: Vec<EffectSlot>,
    metronome: bool,
    metronome_volume: f32,
    /// User ratios for the Custom scale (relative to root).
    custom_ratios: Vec<f32>,
    /// Sorted (sample, event) schedule for the current pattern/loop.
    events: Vec<(usize, NoteEvent)>,
    /// Index of the next un-fired event in 'events'.
    events_pos: usize,
    /// Last metronome beat index (for beat-crossing detection).
    last_beat: usize,
    click_t: f32,
    click_freq: f32,
    click_gain: f32,
}

/// Builder for a single generated waveform (used when no sample is loaded).
fn wave_adsr(
    sample_rate: usize,
    tuning: TuningWrapper,
    wave: Waveform,
) -> Adsr<WaveTableSmoother, TuningWrapper, 2> {
    let table: Box<dyn WaveTable + Send + Sync> = match wave {
        Waveform::Sine => Box::new(SineWave),
        Waveform::Triangle => Box::new(TriangleWave),
        Waveform::Saw => Box::new(SawWave),
        Waveform::Square => Box::new(SquareWave),
    };
    let smoother = WaveTableSmoother::new(vec![table], 0.0);
    Adsr::new(smoother, tuning, sample_rate)
}

/// Build the boxed generator. Like i_am_dsp's demo, the waveform oscillator and
/// the sampler are each wrapped in their own `Adsr` and boxed as `dyn Generator`,
/// so the track can switch between "generated wave" and "loaded sample" cleanly.
/// The sample is re-loaded from `sample_path` (Sampler isn't Clone); when the
/// file needs resampling we join the background thread and feed via `set_pcm_data`.
fn build_generator(
    sample_rate: usize,
    tuning: TuningWrapper,
    wave: Waveform,
    using_sample: bool,
    sample_path: Option<PathBuf>,
    sample_one_shot: bool,
) -> Box<dyn Generator> {
    if using_sample
        && let Some(path) = &sample_path {
            let mut sm = Sampler::<2>::new(sample_rate);
            sm.one_shot = sample_one_shot;
            let loaded = match sm.load_from_file(path) {
                Err(e) => {
                    eprintln!("WARNING: failed to load sample: {e}");
                    false
                }
                Ok(None) => true,
                Ok(Some(handle)) => match handle.thread_handle.join() {
                    Ok(Ok(pcm)) => {
                        sm.set_pcm_data(pcm);
                        true
                    }
                    Ok(Err(e)) => {
                        eprintln!("WARNING: sample resample failed: {e}");
                        false
                    }
                    Err(_) => {
                        eprintln!("WARNING: sample resample thread panicked");
                        false
                    }
                },
            };
            if loaded {
                return Box::new(Adsr::new(sm, tuning, sample_rate));
            }
            // fall through to the waveform if the sample failed to load
        }
    Box::new(wave_adsr(sample_rate, tuning, wave))
}

impl Engine {
    pub fn new(sample_rate: usize, kind: TuningKind) -> Self {
        let pattern = Pattern::demo();
        let tempo = pattern::DEFAULT_TEMPO;
        let loop_samples = pattern::loop_samples(&pattern, sample_rate, tempo);
        let timbre = Timbre::default();
        let generator = build_generator(sample_rate, kind.make(), timbre.waveform, false, None, false);
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
        let mut e = Self {
            tuning_kind: kind,
            generator,
            sample_rate,
            tempo,
            pattern,
            loop_samples,
            sample_counter: 0,
            playing: false, // do not auto-play on startup
            play_start_pos: 0,
            stop_pending: false,
            preview: Vec::new(),
            events_buf: Vec::new(),
            timbre,
            effects,
            using_sample: false,
            sample_path: None,
            sample_one_shot: false,
            metronome: false,
            metronome_volume: 0.5,
            custom_ratios: Vec::new(),
            events: Vec::new(),
            events_pos: 0,
            last_beat: usize::MAX, // ensure the first beat ticks too
            click_t: CLICK_LEN,
            click_freq: 1000.0,
            click_gain: 0.0,
        };
        e.rebuild_events();
        e
    }

    /// Advance one sample and return the stereo output.
    fn next_sample(&mut self) -> [f32; 2] {
        self.events_buf.clear();

        let stop = std::mem::replace(&mut self.stop_pending, false);
        if self.playing {
            // Emit events whose sample has arrived. The schedule is a sorted
            // (sample, event) list rebuilt whenever the pattern/tempo changes,
            // so fractional tuplet positions play at their exact sample.
            while self.events_pos < self.events.len()
                && self.events[self.events_pos].0 <= self.sample_counter
            {
                let (_, ev) = self.events[self.events_pos].clone();
                self.events_buf.push(ev);
                self.events_pos += 1;
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

        // metronome: tick on each beat (stronger+higher on the bar downbeat),
        // honoring the pattern's time signature.
        if self.playing && self.metronome {
            let sps = pattern::samples_per_step(self.sample_rate, self.tempo);
            let spb = pattern::steps_per_beat(self.pattern.beat_unit);
            let beat = (self.sample_counter as f64 / sps / spb).floor() as usize;
            if beat != self.last_beat {
                self.last_beat = beat;
                let bar = beat % self.pattern.beats_per_bar.max(1) as usize == 0;
                self.click_t = 0.0;
                self.click_freq = if bar { 2000.0 } else { 1000.0 };
                self.click_gain = self.metronome_volume * if bar { 1.0 } else { 0.5 };
            }
            if self.click_t < CLICK_LEN {
                let n = self.click_t;
                let env = (1.0 - n / CLICK_LEN).powi(2);
                let tone =
                    (std::f32::consts::TAU * self.click_freq * n).sin() * env * self.click_gain;
                out[0] += tone;
                out[1] += tone;
                self.click_t += 1.0 / self.sample_rate as f32;
            }
        }

        if self.playing {
            self.sample_counter = (self.sample_counter + 1) % self.loop_samples;
            if self.sample_counter == 0 {
                self.events_pos = 0; // the schedule repeats each loop
            }
        }
        out
    }

    // ---- metronome ----
    pub fn metronome(&self) -> bool {
        self.metronome
    }
    pub fn set_metronome(&mut self, on: bool) {
        self.metronome = on;
    }
    pub fn metronome_volume(&self) -> f32 {
        self.metronome_volume
    }
    pub fn set_metronome_volume(&mut self, v: f32) {
        self.metronome_volume = v.clamp(0.0, 1.0);
    }

    // ---- timbre ----
    pub fn timbre(&self) -> Timbre {
        self.timbre
    }

    fn rebuild_generator(&mut self) {
        let tuning = self.resolved_tuning();
        self.generator = build_generator(
            self.sample_rate,
            tuning,
            self.timbre.waveform,
            self.using_sample,
            self.sample_path.clone(),
            self.sample_one_shot,
        );
        self.apply_timbre_params();
    }

    fn resolved_tuning(&self) -> TuningWrapper {
        crate::tuning::resolve(self.tuning_kind, &self.custom_ratios)
    }

    /// Steps per octave, honoring the custom scale's size.
    pub fn tuning_steps(&self) -> i32 {
        if self.tuning_kind == TuningKind::Custom {
            self.custom_ratios.len().max(1) as i32
        } else {
            self.tuning_kind.steps_per_octave() as i32
        }
    }

    pub fn set_custom_ratios(&mut self, ratios: Vec<f32>) {
        self.custom_ratios = ratios;
        if self.tuning_kind == TuningKind::Custom {
            self.rebuild_generator();
        }
    }

    /// Apply the ADSR/gain timbre to whatever the current boxed generator is,
    /// via its parameter interface (we can't reach into a `dyn Generator`).
    fn apply_timbre_params(&mut self) {
        let t = self.timbre;
        let g = &mut self.generator;
        // Start notes at the wave's peak (phase 0.25) so the onset is immediately
        // audible and lines up with the (instant) metronome tick.
        g.set_parameter("attack_time", SetValue::Float(t.attack));
        g.set_parameter("hold_time", SetValue::Float(t.hold));
        g.set_parameter("decay_time", SetValue::Float(t.decay));
        g.set_parameter("sustain_level", SetValue::Float(t.sustain));
        g.set_parameter("release_time", SetValue::Float(t.release));
        g.set_parameter("gain", SetValue::Float(t.gain));
    }

    pub fn set_timbre(&mut self, t: Timbre) {
        let wave_changed = t.waveform != self.timbre.waveform;
        self.timbre = t;
        if wave_changed && !self.using_sample {
            self.rebuild_generator();
        } else {
            self.apply_timbre_params();
        }
    }

    // ---- sample source ----
    pub fn using_sample(&self) -> bool {
        self.using_sample
    }
    pub fn sample_path(&self) -> Option<PathBuf> {
        self.sample_path.clone()
    }
    pub fn sample_one_shot(&self) -> bool {
        self.sample_one_shot
    }

    /// Toggle one-shot (no-loop) playback of the loaded sample.
    pub fn set_sample_one_shot(&mut self, on: bool) {
        if self.sample_one_shot == on {
            return;
        }
        self.sample_one_shot = on;
        if self.using_sample {
            self.rebuild_generator();
        }
    }

    /// Load an audio file as the track's sound source (a resampler/sampler).
    pub fn load_sample(&mut self, path: impl AsRef<std::path::Path>) {
        let path = path.as_ref().to_path_buf();
        self.sample_path = Some(path);
        self.using_sample = true;
        self.rebuild_generator();
    }

    /// Switch back to the selected generated waveform.
    pub fn use_wave(&mut self) {
        self.using_sample = false;
        self.rebuild_generator();
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
        if self.playing {
            return; // no preview sound while the transport is playing
        }
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

    /// Jump the transport to the given grid step (may be fractional).
    pub fn seek_to_step(&mut self, step: f64) {
        self.sample_counter = pattern::sample_of_step(step, self.sample_rate, self.tempo)
            % self.loop_samples;
        self.events_pos = self.events.partition_point(|(s, _)| *s < self.sample_counter);
        // While playing, a manual reposition (ruler click/drag) re-anchors the
        // stop position so Stop returns to where the playhead now sits.
        if self.playing {
            self.play_start_pos = self.sample_counter;
        }
        self.stop_pending = true; // don't let notes ring across a transport jump
    }

    pub fn set_tuning(&mut self, kind: TuningKind) {
        if self.tuning_kind == kind {
            return;
        }
        self.tuning_kind = kind;
        self.rebuild_generator();
    }

    pub fn set_tempo(&mut self, bpm: f32) {
        self.tempo = bpm.max(1.0); // keep loop_samples well-defined
        self.loop_samples = pattern::loop_samples(&self.pattern, self.sample_rate, bpm);
        self.rebuild_events();
    }

    pub fn set_playing(&mut self, playing: bool) {
        if playing && !self.playing {
            // remember where the ruler was when playback starts
            self.play_start_pos = self.sample_counter;
            self.events_pos = self.events.partition_point(|(s, _)| *s < self.sample_counter);
        } else if !playing && self.playing {
            self.stop_pending = true;
            // return the ruler to where it was when playback started
            self.sample_counter = self.play_start_pos;
        }
        self.playing = playing;
    }

    /// Jump the transport back to the start of the loop.
    pub fn rewind(&mut self) {
        self.sample_counter = 0;
        if self.playing {
            // Same re-anchoring as scrubbing: Home while playing makes Stop
            // return to the start as well.
            self.play_start_pos = 0;
        }
        self.stop_pending = true;
        self.events_pos = 0;
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
        self.rebuild_events();
    }
    /// The grid step (fractional, tuplet-aware) the playhead is currently on.
    pub fn playhead_step(&self) -> f64 {
        self.sample_counter as f64 / pattern::samples_per_step(self.sample_rate, self.tempo)
    }

    /// Rebuild the sorted event schedule for the current pattern/loop and
    /// re-point the cursor at the first event not yet passed.
    fn rebuild_events(&mut self) {
        self.events = pattern::build_events(
            &self.pattern,
            self.sample_rate,
            self.tempo,
            self.loop_samples,
        );
        self.events_pos = self.events.partition_point(|(s, _)| *s < self.sample_counter);
    }
    // ---- project save/load ----
    /// Snapshot the current project state.
    pub fn export_project(&self) -> crate::project::Project {
        crate::project::Project {
            pattern: self.pattern.clone(),
            tempo: self.tempo,
            tuning: self.tuning_kind,
            waveform: self.timbre.waveform,
            timbre: self.timbre,
            effects: self
                .effects
                .iter()
                .map(|e| crate::project::EffectState { on: e.on, mix: e.mix })
                .collect(),
            note_names: String::new(),
            tonic: 0,
            scheme: crate::pianoroll::Scheme::ByPitchClass,
            snap: 1.0,
            clips: Vec::new(),
            clip_names: Vec::new(),
            active_clip: 0,
            custom_ratios: self.custom_ratios.clone(),
            sample_one_shot: self.sample_one_shot,
        }
    }

    /// Apply a loaded project's engine state (pattern, tempo, tuning, timbre, effects).
    pub fn import_project(&mut self, p: &crate::project::Project) {
        self.set_pattern(p.pattern.clone());
        self.set_tempo(p.tempo);
        if p.tuning != self.tuning_kind {
            self.set_tuning(p.tuning);
        }
        if p.timbre != self.timbre {
            self.set_timbre(p.timbre);
        }
        for (i, es) in p.effects.iter().enumerate() {
            self.set_effect_on(i, es.on);
            self.set_effect_mix(i, es.mix);
        }
        self.set_sample_one_shot(p.sample_one_shot);
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
                // A poisoned lock (some earlier panic while holding it) must not
                // silence the stream: recover the data and keep generating.
                Err(poisoned) => poisoned.into_inner(),
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