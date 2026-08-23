//! simple_pianoroll — a real-time piano-roll tool built on i_am_dsp.
//!
//! UI architecture: the editor never holds the engine lock while rendering. We
//! briefly lock the engine to snapshot the `Pattern` (and read tuning / tempo /
//! playhead), run the whole UI against a local `Pattern`, then briefly lock
//! again to write changes back and request a repaint. That keeps the real-time
//! audio thread from being starved by the UI.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod midi;
mod pattern;
mod pianoroll;
mod project;
mod tuning;

use std::sync::{Arc, Mutex};

use eframe::egui::{self, Theme};

use audio::Engine;
use pattern::Pattern;
use pianoroll::EditorState;
use tuning::TuningKind;

struct PianoRollApp {
    engine: Arc<Mutex<Engine>>,
    editor: EditorState,
    custom_ratios_input: Vec<f32>,
    show_custom_window: bool,
    show_colors_window: bool,
    /// Pending MIDI import (parsed file waiting for the track-selection window).
    midi_import: Option<midi::ImportData>,
    /// Whether imported MIDI tracks go to separate clips.
    midi_separate: bool,
}

impl eframe::App for PianoRollApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // If a text field (label / name / ratio editor) is focused, don't let
        // the piano-roll shortcuts trigger while the user is typing.
        let typing = ui.ctx().egui_wants_keyboard_input();

        // Space toggles play/pause (brief lock).
        if !typing && ui.input(|i| i.key_pressed(egui::Key::Space)) {
            #[cfg(target_arch = "wasm32")]
            audio::resume_audio();
            let mut e = self.engine_guard();
            let p = !e.playing();
            e.set_playing(p);
        }
        // The web AudioContext starts suspended; any click is a user gesture
        // that may resume it (cheap no-op once running).
        #[cfg(target_arch = "wasm32")]
        if ui.input(|i| i.pointer.any_click()) {
            audio::resume_audio();
        }
        // Home / W: rewind the transport to the start.
        if !typing && ui.input(|i| i.key_pressed(egui::Key::Home)) {
            self.engine_guard().rewind();
        }

        // Snapshot engine state (brief locks), then the rest runs lock-free.
        let mut pat = self.engine_guard().pattern().clone();
        let spo = self.engine_guard().tuning_steps();
        let ph = self.engine_guard().playhead_step();

        // ---- keyboard shortcuts (work on the local pattern + editor) ----
        if !typing {
            let mods = ui.input(|i| i.modifiers);
            // egui turns Ctrl+C/V/X into Event::Copy/Paste/Cut before the reader sees them.
            let ev_copy = ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Copy)));
            let ev_cut = ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Cut)));
            let ev_paste = ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Paste(_))));
            let ev_del = ui.input(|i| i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace));
            let ev_d = mods.command && ui.input(|i| i.key_pressed(egui::Key::D) || i.key_pressed(egui::Key::B));
            let ev_z = mods.command && ui.input(|i| i.key_pressed(egui::Key::Z));
            let ev_y = mods.command && ui.input(|i| i.key_pressed(egui::Key::Y));
            // egui turns Ctrl+A into Event::SelectAll; keep Key::A as a fallback.
            let ev_selall = mods.command && ui.input(|i| i.key_pressed(egui::Key::A));
            let ev_save = mods.command && ui.input(|i| i.key_pressed(egui::Key::S));
            let ev_up = ui.input(|i| i.key_pressed(egui::Key::ArrowUp));
            let ev_down = ui.input(|i| i.key_pressed(egui::Key::ArrowDown));

            if ev_copy {
                self.editor.copy_selected(&pat);
            } else if ev_cut {
                self.editor.copy_selected(&pat);
                if !self.editor.selection.is_empty() {
                    self.editor.begin_edit(&mut pat);
                    let sel = self.editor.selection.clone();
                    pat.notes.retain(|n| !sel.contains(&n.id));
                    self.editor.selection.clear();
                }
            } else if ev_paste {
                self.editor.paste_at_playhead(&mut pat, ph);
            } else if ev_del {
                // Delete / Backspace: remove the selected notes
                if !self.editor.selection.is_empty() {
                    self.editor.begin_edit(&mut pat);
                    let sel = self.editor.selection.clone();
                    pat.notes.retain(|n| !sel.contains(&n.id));
                    self.editor.selection.clear();
                }
            } else if ev_selall {
                // Ctrl+A: select all notes
                self.editor.selection = pat.notes.iter().map(|n| n.id).collect();
            } else if ev_d {
                self.editor.duplicate_selected(&mut pat);
            } else if ev_z {
                if mods.shift {
                    self.editor.redo(&mut pat);
                } else {
                    self.editor.undo(&mut pat);
                }
            } else if ev_y {
                self.editor.redo(&mut pat);
            } else if ev_save {
                self.save_project(&pat);
            } else if mods.command && ev_up {
                self.editor.transpose(&mut pat, spo, false, spo);
            } else if mods.command && ev_down {
                self.editor.transpose(&mut pat, -spo, false, spo);
            } else if mods.shift && ev_up {
                self.editor.transpose(&mut pat, 1, false, spo);
            } else if mods.shift && ev_down {
                self.editor.transpose(&mut pat, -1, false, spo);
            }
        }

        // ---- top panel: tuning / tempo / play / clear / demo ----
        egui::Panel::top("controls").show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.heading("simple_pianoroll");
                ui.separator();

                let mut kind = self.engine_guard().tuning_kind();
                let mut kind_changed = false;
                egui::ComboBox::from_label("Tuning")
                    .selected_text(kind.label())
                    .show_ui(ui, |ui| {
                        for k in TuningKind::all() {
                            let selected = kind == *k;
                            if ui.selectable_label(selected, k.label()).clicked() {
                                kind = *k;
                                kind_changed = true;
                            }
                        }
                    });
                if kind_changed {
                    self.engine_guard().set_tuning(kind);
                    let n = if kind == TuningKind::Custom {
                        self.custom_ratios_input.len().max(1)
                    } else {
                        kind.steps_per_octave()
                    };
                    self.editor.names = pianoroll::default_names(n);
                }
                if ui.button("Custom…").clicked() {
                    self.show_custom_window = true;
                }

                let mut tempo = self.engine_guard().tempo();
                if ui
                    .add(egui::Slider::new(&mut tempo, 40.0..=240.0).text("BPM"))
                    .changed()
                {
                    self.engine_guard().set_tempo(tempo);
                }
                let playing = self.engine_guard().playing();
                let lbl = if playing { "⏸ Pause" } else { "▶ Play" };
                if ui.button(lbl).clicked() {
                    let mut e = self.engine_guard();
                    e.set_playing(!playing);
                }
                if ui.button("⏮ Stop & Home").clicked() {
                    let mut e = self.engine_guard();
                    e.set_playing(false);
                    e.rewind();
                }

                ui.separator();
                let mut met = self.engine_guard().metronome();
                if ui.checkbox(&mut met, "Metronome").changed() {
                    self.engine_guard().set_metronome(met);
                }
                let mut mvol = self.engine_guard().metronome_volume();
                if ui.add(egui::Slider::new(&mut mvol, 0.0..=1.0).text("Met vol")).changed() {
                    self.engine_guard().set_metronome_volume(mvol);
                }

                ui.separator();
                if ui.button("Clear").clicked() {
                    self.editor.begin_edit(&mut pat);
                    pat.notes.clear();
                    self.editor.selection.clear();
                }
                if ui.button("Demo").clicked() {
                    self.editor.begin_edit(&mut pat);
                    let _ = std::mem::replace(&mut pat, Pattern::demo());
                    self.editor.selection.clear();
                }

                ui.separator();
                if ui.button("💾 Save").clicked() {
                    self.save_project(&pat);
                }
                #[cfg(target_arch = "wasm32")]
                {
                    ui.add_enabled(false, egui::Button::new("📂 Open"))
                        .on_hover_text("File dialogs are desktop-only");
                }
                #[cfg(not(target_arch = "wasm32"))]
                if ui.button("📂 Open").clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .add_filter("Project", &["json"])
                        .pick_file()
                        && let Ok(json) = std::fs::read_to_string(&path) {
                            match project::from_json(&json) {
                                Ok(p) => {
                                    // import into the engine, then sync the local
                                    // snapshot so the frame-end write-back doesn't
                                    // immediately revert the loaded pattern.
                                    let mut e = self.engine_guard();
                                    e.import_project(&p);
                                    pat = e.pattern().clone();
                                    drop(e);
                                    if !p.clips.is_empty() {
                                        self.editor.clips = p.clips.clone();
                                        self.editor.clip_names = p.clip_names.clone();
                                        self.editor.active_clip = p.active_clip.min(self.editor.clips.len() - 1);
                                        if let Some(c) = self.editor.clips.get(self.editor.active_clip) {
                                            pat = c.clone();
                                        }
                                        self.engine_guard().set_pattern(pat.clone());
                                    } else {
                                        self.editor.clips = vec![pat.clone()];
                                        self.editor.clip_names = vec!["Clip 0".to_string()];
                                        self.editor.active_clip = 0;
                                    }
                                    self.custom_ratios_input = p.custom_ratios.clone();
                                    self.engine_guard().set_custom_ratios(p.custom_ratios.clone());
                                    self.editor.names = p.note_names;
                                    self.editor.scheme = p.scheme;
                                    self.editor.snap = p.snap;
                                    self.editor.row_h = p.row_h;
                                    self.editor.tonic = p.tonic;
                                    self.editor.selection.clear();
                                    self.editor.begin_edit(&mut pat);
                                }
                                Err(e) => eprintln!("could not parse project: {e}"),
                            }
                        }

                #[cfg(target_arch = "wasm32")]
                {
                    ui.add_enabled(false, egui::Button::new("🎹 MIDI…"))
                        .on_hover_text("File dialogs are desktop-only");
                }
                #[cfg(not(target_arch = "wasm32"))]
                if ui.button("🎹 MIDI…").clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .add_filter("MIDI", &["mid", "midi"])
                        .pick_file()
                    && let Ok(bytes) = std::fs::read(&path)
                {
                    match midi::parse(&bytes) {
                        Ok(data) => self.midi_import = Some(data),
                        Err(e) => eprintln!("MIDI import failed: {e}"),
                    }
                }

                ui.separator();
                ui.label("Clips:");
                let active_name = self
                    .editor
                    .clip_names
                    .get(self.editor.active_clip)
                    .cloned()
                    .unwrap_or_else(|| format!("Clip {}", self.editor.active_clip));
                egui::ComboBox::from_id_salt("clips")
                    .selected_text(active_name)
                    .show_ui(ui, |ui| {
                        for idx in 0..self.editor.clips.len() {
                            let lbl = self
                                .editor
                                .clip_names
                                .get(idx)
                                .cloned()
                                .unwrap_or_else(|| format!("Clip {idx}"));
                            if ui
                                .selectable_label(self.editor.active_clip == idx, lbl)
                                .clicked()
                            {
                                self.editor.clips[self.editor.active_clip] = pat.clone();
                                self.editor.active_clip = idx;
                                if let Some(c) = self.editor.clips.get(idx) {
                                    pat = c.clone();
                                }
                                self.engine_guard().set_pattern(pat.clone());
                                self.editor.selection.clear();
                                let mut p2 = pat.clone();
                                self.editor.begin_edit(&mut p2);
                            }
                        }
                    });
                // rename the active clip
                let mut nm = self
                    .editor
                    .clip_names
                    .get(self.editor.active_clip)
                    .cloned()
                    .unwrap_or_default();
                if ui
                    .add(egui::TextEdit::singleline(&mut nm).desired_width(90.0).hint_text("name"))
                    .changed()
                    && let Some(slot) = self.editor.clip_names.get_mut(self.editor.active_clip) {
                        *slot = nm;
                    }
                if ui.button("+ Clip").clicked() {
                    let total = pat.total_steps;
                    let new_idx = self.editor.clips.len();
                    self.editor.clips.push(pattern::Pattern::empty(total));
                    self.editor.clip_names.push(format!("Clip {new_idx}"));
                    self.editor.active_clip = new_idx;
                    pat = self.editor.clips[new_idx].clone();
                    self.engine_guard().set_pattern(pat.clone());
                    self.editor.selection.clear();
                    let mut p2 = pat.clone();
                    self.editor.begin_edit(&mut p2);
                }
                let del_enabled = self.editor.clips.len() > 1;
                if ui
                    .add_enabled(del_enabled, egui::Button::new("- Clip"))
                    .on_hover_text("Delete the current clip (disabled when it is the only clip)")
                    .clicked()
                {
                    self.editor.clips.remove(self.editor.active_clip);
                    self.editor.clip_names.remove(self.editor.active_clip);
                    if self.editor.active_clip >= self.editor.clips.len() {
                        self.editor.active_clip = self.editor.clips.len() - 1;
                    }
                    if let Some(c) = self.editor.clips.get(self.editor.active_clip) {
                        pat = c.clone();
                    }
                    self.engine_guard().set_pattern(pat.clone());
                    self.editor.selection.clear();
                    let mut p2 = pat.clone();
                    self.editor.begin_edit(&mut p2);
                }
            });
        });

        // ---- bottom panel: snap / clip length / color scheme ----
        egui::Panel::bottom("edit").show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label("Snap:");
                let spb = pattern::steps_per_beat(pat.beat_unit);
                let snap_values = [1.0f64, 2.0, 4.0, 8.0, 16.0, spb / 3.0, spb / 5.0, spb / 7.0];
                let snap_labels = ["1", "2", "4", "8", "16", "Triplet (3)", "Quintuplet (5)", "Septuplet (7)"];
                let snap_text = snap_values
                    .iter()
                    .position(|v| (*v - self.editor.snap).abs() < 1e-6)
                    .map(|i| snap_labels[i].to_string())
                    .unwrap_or_else(|| format!("{:.3}", self.editor.snap));
                egui::ComboBox::from_id_salt("snap")
                    .selected_text(snap_text)
                    .show_ui(ui, |ui| {
                        for (i, v) in snap_values.iter().enumerate() {
                            if ui.selectable_label((*v - self.editor.snap).abs() < 1e-6, snap_labels[i]).clicked() {
                                self.editor.snap = *v;
                            }
                        }
                    });
                ui.separator();
                ui.label("Clip len:");
                let mut tlen = pat.total_steps as i32;
                let slide = ui.add(egui::DragValue::new(&mut tlen).suffix("steps"));
                if slide.drag_started() {
                    self.editor.begin_edit(&mut pat);
                }
                if slide.changed() {
                    tlen = tlen.max(1);
                    pat.set_len(tlen as usize);
                }
                ui.separator();
                ui.label("Time sig:");
                let mut beats = pat.beats_per_bar as i32;
                if ui
                    .add(egui::DragValue::new(&mut beats).range(1..=16).speed(1))
                    .changed()
                {
                    self.editor.begin_edit(&mut pat);
                    pat.beats_per_bar = beats.max(1) as u32;
                }
                let mut unit = pat.beat_unit;
                egui::ComboBox::from_id_salt("beat_unit")
                    .selected_text(format!("{unit}"))
                    .show_ui(ui, |ui| {
                        for u in [2u32, 4, 8, 16] {
                            if ui.selectable_label(unit == u, format!("{u}")).clicked() {
                                unit = u;
                            }
                        }
                    });
                if unit != pat.beat_unit {
                    self.editor.begin_edit(&mut pat);
                    pat.beat_unit = unit;
                }
                ui.separator();
                ui.label("Row h:");
                ui.add(egui::Slider::new(&mut self.editor.row_h, 8.0..=32.0).text("px"));
                ui.separator();
                ui.label("Color:");
                egui::ComboBox::from_id_salt("scheme")
                    .selected_text(self.editor.scheme.label())
                    .show_ui(ui, |ui| {
                        for sc in pianoroll::Scheme::all() {
                            if ui.selectable_label(self.editor.scheme == *sc, sc.label()).clicked() {
                                self.editor.scheme = *sc;
                            }
                        }
                    });
                if ui.button("Colors…").clicked() {
                    self.show_colors_window = true;
                }

                ui.separator();
                ui.horizontal(|ui| {
                    ui.label("Root:");
                    let spo = self.engine_guard().tuning_steps().max(1);
                    let mut root = self.editor.tonic;
                    if ui
                        .add(egui::DragValue::new(&mut root).range(0..=spo - 1).speed(1))
                        .changed()
                    {
                        self.editor.tonic = root;
                    }
                    ui.label("(tonic pitch class)");
                });
                ui.separator();
                ui.label("Note names:");
                let mut nnames = self.editor.names.clone();
                let nres = ui.add(
                    egui::TextEdit::singleline(&mut nnames)
                        .hint_text("C C# D D# E F F# G G# A A# B ...")
                        .desired_width(190.0),
                );
                if nres.changed() {
                    self.editor.names = nnames;
                }
                if ui.button("Reset").clicked() {
                    let spo = self.engine_guard().tuning_steps();
                    self.editor.names = pianoroll::default_names(spo as usize);
                }

                if !self.editor.selection.is_empty() {
                    ui.separator();
                    // velocity of the selected notes
                    let some_id = *self.editor.selection.iter().next().unwrap();
                    let mut vel = pat
                        .notes
                        .iter()
                        .find(|n| n.id == some_id)
                        .map(|n| n.velocity)
                        .unwrap_or(0.8);
                    let vres = ui.add(egui::Slider::new(&mut vel, 0.0..=1.0).text("Vel"));
                    if vres.drag_started() {
                        self.editor.begin_edit(&mut pat);
                    }
                    if vres.changed() {
                        let sel = self.editor.selection.clone();
                        for id in sel {
                            if let Some(n) = pat.notes.iter_mut().find(|n| n.id == id) {
                                n.velocity = vel;
                            }
                        }
                        self.editor.last_velocity = vel;
                    }

                    ui.separator();
                    ui.label("Note label:");
                    let some_id = *self.editor.selection.iter().next().unwrap();
                    let mut note_label = pat
                        .notes
                        .iter()
                        .find(|n| n.id == some_id)
                        .map(|n| n.label.clone())
                        .unwrap_or_default();
                    let lres = ui.add(
                        egui::TextEdit::singleline(&mut note_label)
                            .hint_text("custom label (overrides name)")
                            .desired_width(150.0),
                    );
                    if lres.gained_focus() {
                        self.editor.begin_edit(&mut pat);
                    }
                    if lres.changed() {
                        let sel = self.editor.selection.clone();
                        for id in sel {
                            pat.set_label(id, note_label.clone());
                        }
                    }
                    if ui.button("Clear label").clicked() {
                        self.editor.begin_edit(&mut pat);
                        let sel = self.editor.selection.clone();
                        for id in sel {
                            pat.set_label(id, String::new());
                        }
                    }
                }
            });
        });

        // ---- right panel: track (timbre + effect chain) ----
        egui::Panel::right("track").show(ui, |ui| {
            ui.heading("Track");
            ui.add_space(2.0);

            // Timbre
            let wave_label = |w: audio::Waveform| match w {
                audio::Waveform::Sine => "Sine",
                audio::Waveform::Triangle => "Triangle",
                audio::Waveform::Saw => "Saw",
                audio::Waveform::Square => "Square",
            };
            let mut tb = self.engine_guard().timbre();
            let tb_orig = tb;

            egui::ComboBox::from_label("Wave")
                .selected_text(wave_label(tb.waveform))
                .show_ui(ui, |ui| {
                    for w in [
                        audio::Waveform::Sine,
                        audio::Waveform::Triangle,
                        audio::Waveform::Saw,
                        audio::Waveform::Square,
                    ] {
                        ui.selectable_value(&mut tb.waveform, w, wave_label(w));
                    }
                });
            ui.add(egui::Slider::new(&mut tb.attack, 1.0..=2000.0).text("Attack"));
            ui.add(egui::Slider::new(&mut tb.hold, 0.0..=2000.0).text("Hold"));
            ui.add(egui::Slider::new(&mut tb.decay, 0.0..=2000.0).text("Decay"));
            ui.add(egui::Slider::new(&mut tb.sustain, 0.0..=1.0).text("Sustain"));
            ui.add(egui::Slider::new(&mut tb.release, 1.0..=2000.0).text("Release"));
            ui.add(egui::Slider::new(&mut tb.gain, 0.0..=2.0).text("Gain"));
            if tb != tb_orig {
                self.engine_guard().set_timbre(tb);
            }

            // sample source (user-loaded timbre via the resampler)
            ui.separator();
            if self.engine_guard().using_sample() {
                ui.label("Source: loaded sample");
                if let Some(p) = self.engine_guard().sample_path() {
                    let name = p.file_name().and_then(|x| x.to_str()).unwrap_or("sample").to_string();
                    ui.label(name);
                }
                if ui.button("Use generated wave").clicked() {
                    self.engine_guard().use_wave();
                }
                let mut one = self.engine_guard().sample_one_shot();
                if ui.checkbox(&mut one, "One shot").on_hover_text("Play the sample once; do not loop it").changed() {
                    self.engine_guard().set_sample_one_shot(one);
                }
            } else if let Some(path) = {
                #[cfg(target_arch = "wasm32")]
                {
                    ui.add_enabled(false, egui::Button::new("Load sample…"))
                        .on_hover_text("File dialogs are desktop-only");
                    None::<std::path::PathBuf>
                }
                #[cfg(not(target_arch = "wasm32"))]
                {
                    if ui.button("Load sample…").clicked() {
                        rfd::FileDialog::new()
                            .add_filter("Audio", &["wav", "flac", "mp3", "ogg"])
                            .pick_file()
                    } else {
                        None
                    }
                }
            } {
                self.engine_guard().load_sample(&path);
            }

            ui.separator();
            ui.heading("Effects");
            let count = self.engine_guard().effect_count();
            for i in 0..count {
                let name = self.engine_guard().effect_name(i).to_string();
                let mut on = self.engine_guard().effect_on(i);
                let mut mix = self.engine_guard().effect_mix(i);
                let on0 = on;
                let mix0 = mix;
                ui.horizontal(|ui| {
                    ui.checkbox(&mut on, name);
                    ui.add_enabled(on, egui::Slider::new(&mut mix, 0.0..=1.0).text("mix"));
                });
                if on != on0 {
                    self.engine_guard().set_effect_on(i, on);
                }
                if mix != mix0 {
                    self.engine_guard().set_effect_mix(i, mix);
                }
            }
        });

        // ---- central: status + piano-roll editor (NO engine lock held) ----
        let mut preview_out: Vec<i32> = Vec::new();
        let mut seek_out: Option<f64> = None;
        egui::CentralPanel::default().show(ui, |ui| {
            let tempo = self.engine_guard().tempo();
            ui.label(format!(
                "{} · {} rows/octave · BPM {:.0} · {} notes · {} selected",
                self.engine_guard().tuning_kind().label(),
                spo,
                tempo,
                pat.notes.len(),
                self.editor.selection.len(),
            ));
            ui.add_space(4.0);
            pianoroll::show(ui, &mut self.editor, &mut pat, spo, ph, &mut preview_out, &mut seek_out);
        });

        // ---- custom scale editor (separate window) ----
        if self.show_custom_window {
            let mut open = true;
            egui::Window::new("Custom scale (note · ratio to root)")
                .open(&mut open)
                .show(ui.ctx(), |ui| {
                    // number of notes per octave
                    let mut n = self.custom_ratios_input.len() as i32;
                    if ui
                        .add(egui::DragValue::new(&mut n).range(1..=128))
                        .changed()
                    {
                        let n = n.clamp(1, 128) as usize;
                        self.custom_ratios_input.resize(n, 2.0);
                        if let Some(r) = self.custom_ratios_input.first_mut() {
                            *r = 1.0; // root ratio is 1.0
                        }
                    }
                    ui.add_space(4.0);
                    ui.label("Ratios (each note / root):");
                    ui.horizontal_wrapped(|ui| {
                        for i in 0..self.custom_ratios_input.len() {
                            ui.add(
                                egui::DragValue::new(&mut self.custom_ratios_input[i])
                                    .speed(0.001)
                                    .fixed_decimals(4),
                            );
                        }
                    });
                    ui.add_space(4.0);
                    if ui.button("Apply tuning").clicked() {
                        self.engine_guard().set_custom_ratios(self.custom_ratios_input.clone());
                        let mut e = self.engine_guard();
                        e.set_tuning(TuningKind::Custom);
                        let spo = e.tuning_steps() as usize;
                        drop(e);
                        self.editor.names = pianoroll::default_names(spo);
                    }
                    if ui.button("Close").clicked() {
                        self.show_custom_window = false;
                    }
                });
            if !open {
                self.show_custom_window = false;
            }
        }

        // ---- MIDI import: pick tracks, then apply ----
        // Take the parsed data out of self so the window closure never needs to
        // borrow both self.midi_import and self at the same time.
        let mut import = self.midi_import.take();
        if let Some(ref mut data) = import {
            let mut open = true;
            let mut apply = false;
            let mut cancelled = false;
            let mut warn = false;
            let mut separate = self.midi_separate;
            egui::Window::new("Import MIDI")
                .open(&mut open)
                .show(ui.ctx(), |ui| {
                    ui.label(format!(
                        "{} track(s) · {} BPM · {} ticks/quarter",
                        data.tracks.len(),
                        data.tempo as i32,
                        data.ppq
                    ));
                    ui.add_space(4.0);
                    for t in data.tracks.iter_mut() {
                        ui.checkbox(&mut t.selected, format!("{} — {} notes", t.name, t.notes.len()));
                    }
                    ui.add_space(4.0);
                    if ui
                        .checkbox(&mut separate, "Import each track to its own clip")
                        .changed()
                    {
                        self.midi_separate = separate;
                    }
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button("Import").clicked() {
                            if data.tracks.iter().any(|t| t.selected) {
                                apply = true;
                            } else {
                                warn = true;
                            }
                        }
                        if ui.button("Cancel").clicked() {
                            cancelled = true;
                        }
                    });
                    if warn {
                        ui.colored_label(egui::Color32::RED, "Select at least one track");
                    }
                });
            if apply {
                let (clips, names) = midi::build_clips(data, separate);
                self.editor.begin_edit(&mut pat);
                self.editor.clips = clips;
                self.editor.clip_names = names;
                self.editor.active_clip = 0;
                pat = self.editor.clips[0].clone();
                self.engine_guard().set_tempo(data.tempo);
                self.engine_guard().set_pattern(pat.clone());
                self.editor.selection.clear();
                // import dropped on success
            } else if !cancelled && open {
                self.midi_import = import; // keep for the next frame
            }
        }

        // ---- per-degree note colors (separate window) ----
        if self.show_colors_window {
            let mut open = true;
            egui::Window::new("Note colors — one swatch per pitch class (used by the Custom scheme)")
                .open(&mut open)
                .show(ui.ctx(), |ui| {
                    let spo = self.engine_guard().tuning_steps();
                    let cap = (spo.min(24)) as usize;
                    while self.editor.custom_colors.len() < cap {
                        self.editor.custom_colors.push([140, 140, 140]);
                    }
                    self.editor.custom_colors.truncate(cap);
                    ui.label(format!("{cap} pitch classes (select Custom in Color to apply)"));
                    ui.add_space(4.0);
                    ui.horizontal_wrapped(|ui| {
                        for i in 0..cap {
                            ui.color_edit_button_srgb(&mut self.editor.custom_colors[i]);
                        }
                    });
                    ui.add_space(6.0);
                    if ui.button("Close").clicked() {
                        self.show_colors_window = false;
                    }
                });
            if !open {
                self.show_colors_window = false;
            }
        }

        // ---- write the edited pattern back + apply preview/seek (brief lock) ----
        {
            let mut e = self.engine_guard();
            if *e.pattern() != pat {
                e.set_pattern(pat.clone());
            }
            for p in &preview_out {
                e.preview_note(*p);
            }
            if let Some(s) = seek_out {
                e.seek_to_step(s);
            }
            if e.playing() {
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(16));
            }
        }
    }
}

impl PianoRollApp {
    /// Lock the engine, tolerating a poisoned mutex: a transient panic on the
    /// audio thread while holding the lock must not take the whole GUI down.
    fn engine_guard(&self) -> std::sync::MutexGuard<'_, Engine> {
        self.engine.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Save the project via a file dialog (also Ctrl+S).
    fn save_project(&mut self, pat: &Pattern) {
        #[cfg(target_arch = "wasm32")]
        {
            let _ = (self, pat);
            eprintln!("file save is desktop-only for now");
            return;
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(mut path) = rfd::FileDialog::new()
            .set_file_name("project.json")
            .save_file()
        {
            // make sure the file gets a .json extension automatically
            if path.extension().map(|e| e != "json").unwrap_or(true) {
                path.set_extension("json");
            }
            let mut p = self.engine_guard().export_project();
            p.note_names = self.editor.names.clone();
            p.scheme = self.editor.scheme;
            p.snap = self.editor.snap;
            p.tonic = self.editor.tonic;
            p.custom_ratios = self.custom_ratios_input.clone();
            p.row_h = self.editor.row_h;
            // keep the live edits of the active clip before saving
            if !self.editor.clips.is_empty() {
                let a = self.editor.active_clip;
                if a < self.editor.clips.len() {
                    self.editor.clips[a] = pat.clone();
                }
            }
            p.clips = self.editor.clips.clone();
            p.clip_names = self.editor.clip_names.clone();
            p.active_clip = self.editor.active_clip;
            match project::to_json(&p) {
                Ok(json) => {
                    if let Err(e) = std::fs::write(&path, json) {
                        eprintln!("save failed: {e}");
                    }
                }
                Err(e) => eprintln!("could not serialize project: {e}"),
            }
        }
    }
}

/// Build the app, shared by the desktop and web entry points.
fn make_app(
    cc: &eframe::CreationContext<'_>,
    engine: Arc<Mutex<Engine>>,
) -> Result<Box<dyn eframe::App>, Box<dyn std::error::Error + Send + Sync>> {
    cc.egui_ctx.set_theme(Theme::Dark);
    let mut editor = EditorState::default();
    let initial = engine.lock().unwrap_or_else(|p| p.into_inner()).pattern().clone();
    editor.clips = vec![initial.clone()];
    editor.clip_names = vec!["Clip 0".to_string()];
    editor.active_clip = 0;
    let mut seed = initial.clone();
    editor.begin_edit(&mut seed);
    Ok(Box::new(PianoRollApp {
        engine,
        editor,
        custom_ratios_input: vec![1.0, 9.0 / 8.0, 5.0 / 4.0, 3.0 / 2.0],
        show_custom_window: false,
        show_colors_window: false,
        midi_import: None,
        midi_separate: false,
    }))
}

/// Desktop: native window + cpal audio stream.
#[cfg(not(target_arch = "wasm32"))]
fn main() -> eframe::Result<()> {
    let (engine, _stream) = audio::start(TuningKind::Equal12);
    let options = eframe::NativeOptions {
        // renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    eframe::run_native("simple_pianoroll", options, Box::new(move |cc| make_app(cc, engine)))
}

/// Web: run inside the browser via eframe's WebRunner. cpal's wasm backend
/// drives Web Audio; the AudioContext starts suspended (autoplay policy), so
/// the first user interaction resumes it (see resume_audio calls in ui()).
#[cfg(target_arch = "wasm32")]
fn main() -> eframe::Result<()> {
    use wasm_bindgen::JsCast;
    wasm_bindgen_futures::spawn_local(async move {
        let (engine, _stream) = audio::start(TuningKind::Equal12);
        let canvas = web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.get_element_by_id("simple_pianoroll_canvas"))
            .and_then(|el| el.dyn_into::<web_sys::HtmlCanvasElement>().ok())
            .expect("canvas #simple_pianoroll_canvas not found");
        let web_options = eframe::WebOptions::default();
        eframe::WebRunner::new()
            .start(
                canvas,
                web_options,
                Box::new(move |cc| make_app(cc, engine)),
            )
            .await
            .expect("failed to start eframe");
    });
    Ok(())
}