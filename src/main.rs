//! simple_pianoroll — a real-time piano-roll tool built on i_am_dsp.
//!
//! UI architecture: the editor never holds the engine lock while rendering. We
//! briefly lock the engine to snapshot the `Pattern` (and read tuning / tempo /
//! playhead), run the whole UI against a local `Pattern`, then briefly lock
//! again to write changes back and request a repaint. That keeps the real-time
//! audio thread from being starved by the UI.

mod audio;
mod pattern;
mod pianoroll;
mod project;
mod tuning;

use std::sync::{Arc, Mutex};

use eframe::egui;

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
}

impl eframe::App for PianoRollApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // If a text field (label / name / ratio editor) is focused, don't let
        // the piano-roll shortcuts trigger while the user is typing.
        let typing = ui.ctx().egui_wants_keyboard_input();

        // Space toggles play/pause (brief lock).
        if !typing && ui.input(|i| i.key_pressed(egui::Key::Space)) {
            let mut e = self.engine.lock().unwrap();
            let p = !e.playing();
            e.set_playing(p);
        }
        // Home / W: rewind the transport to the start.
        if !typing && ui.input(|i| i.key_pressed(egui::Key::Home)) {
            self.engine.lock().unwrap().rewind();
        }

        // Snapshot engine state (brief locks), then the rest runs lock-free.
        let mut pat = self.engine.lock().unwrap().pattern().clone();
        let spo = self.engine.lock().unwrap().tuning_steps();
        let ph = self.engine.lock().unwrap().playhead_step();

        // ---- keyboard shortcuts (work on the local pattern + editor) ----
        if !typing {
            let mods = ui.input(|i| i.modifiers);
            // egui turns Ctrl+C/V/X into Event::Copy/Paste/Cut before the reader sees them.
            let ev_copy = ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Copy)));
            let ev_cut = ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Cut)));
            let ev_paste = ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Paste(_))));
            let ev_del = ui.input(|i| i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace));
            let ev_d = mods.command && ui.input(|i| i.key_pressed(egui::Key::D));
            let ev_z = mods.command && ui.input(|i| i.key_pressed(egui::Key::Z));
            let ev_y = mods.command && ui.input(|i| i.key_pressed(egui::Key::Y));
            // egui turns Ctrl+A into Event::SelectAll; keep Key::A as a fallback.
            let ev_selall = mods.command && ui.input(|i| i.key_pressed(egui::Key::A));

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
            }
        }

        // ---- top panel: tuning / tempo / play / clear / demo ----
        egui::Panel::top("controls").show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.heading("simple_pianoroll");
                ui.separator();

                let mut kind = self.engine.lock().unwrap().tuning_kind();
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
                    self.engine.lock().unwrap().set_tuning(kind);
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

                let mut tempo = self.engine.lock().unwrap().tempo();
                if ui
                    .add(egui::Slider::new(&mut tempo, 40.0..=240.0).text("BPM"))
                    .changed()
                {
                    self.engine.lock().unwrap().set_tempo(tempo);
                }
                let playing = self.engine.lock().unwrap().playing();
                let lbl = if playing { "⏸ Pause" } else { "▶ Play" };
                if ui.button(lbl).clicked() {
                    let mut e = self.engine.lock().unwrap();
                    e.set_playing(!playing);
                }
                if ui.button("⏮ Stop & Home").clicked() {
                    let mut e = self.engine.lock().unwrap();
                    e.set_playing(false);
                }

                ui.separator();
                let mut met = self.engine.lock().unwrap().metronome();
                if ui.checkbox(&mut met, "Metronome").changed() {
                    self.engine.lock().unwrap().set_metronome(met);
                }
                let mut mvol = self.engine.lock().unwrap().metronome_volume();
                if ui.add(egui::Slider::new(&mut mvol, 0.0..=1.0).text("Met vol")).changed() {
                    self.engine.lock().unwrap().set_metronome_volume(mvol);
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
                if ui.button("💾 Save").clicked()
                    && let Some(mut path) = rfd::FileDialog::new()
                        .set_file_name("project.json")
                        .save_file()
                    {
                        // make sure the file gets a .json extension automatically
                        if path.extension().map(|e| e != "json").unwrap_or(true) {
                            path.set_extension("json");
                        }
                        let mut p = self.engine.lock().unwrap().export_project();
                        p.note_names = self.editor.names.clone();
                        p.scheme = self.editor.scheme;
                        p.snap = self.editor.snap;
                        p.tonic = self.editor.tonic;
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
                                    let mut e = self.engine.lock().unwrap();
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
                                        self.engine.lock().unwrap().set_pattern(pat.clone());
                                    } else {
                                        self.editor.clips = vec![pat.clone()];
                                        self.editor.clip_names = vec!["Clip 0".to_string()];
                                        self.editor.active_clip = 0;
                                    }
                                    self.editor.names = p.note_names;
                                    self.editor.scheme = p.scheme;
                                    self.editor.snap = p.snap;
                                    self.editor.tonic = p.tonic;
                                    self.editor.selection.clear();
                                    self.editor.begin_edit(&mut pat);
                                }
                                Err(e) => eprintln!("could not parse project: {e}"),
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
                                self.engine.lock().unwrap().set_pattern(pat.clone());
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
                    self.engine.lock().unwrap().set_pattern(pat.clone());
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
                    self.engine.lock().unwrap().set_pattern(pat.clone());
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
                egui::ComboBox::from_id_salt("snap")
                    .selected_text(format!("{} step{}", self.editor.snap, if self.editor.snap == 1 { "" } else { "s" }))
                    .show_ui(ui, |ui| {
                        for v in [1usize, 2, 4, 8, 16, pattern::BAR_STEPS] {
                            if ui.selectable_label(self.editor.snap == v, format!("{v}")).clicked() {
                                self.editor.snap = v;
                            }
                        }
                    });
                ui.separator();
                ui.label("Clip len:");
                let mut tlen = pat.total_steps as i32;
                let slide = ui.add(egui::Slider::new(&mut tlen, 8..=256).step_by(1.0).drag_value_speed(0.5).text("steps"));
                if slide.drag_started() {
                    self.editor.begin_edit(&mut pat);
                }
                if slide.changed() {
                    pat.set_len(tlen as usize);
                }
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
                    let spo = self.engine.lock().unwrap().tuning_steps().max(1);
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
                    let spo = self.engine.lock().unwrap().tuning_steps();
                    self.editor.names = pianoroll::default_names(spo as usize);
                }

                if !self.editor.selection.is_empty() {
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
            let mut tb = self.engine.lock().unwrap().timbre();
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
                self.engine.lock().unwrap().set_timbre(tb);
            }

            // sample source (user-loaded timbre via the resampler)
            ui.separator();
            if self.engine.lock().unwrap().using_sample() {
                ui.label("Source: loaded sample");
                if let Some(p) = self.engine.lock().unwrap().sample_path() {
                    let name = p.file_name().and_then(|x| x.to_str()).unwrap_or("sample").to_string();
                    ui.label(name);
                }
                if ui.button("Use generated wave").clicked() {
                    self.engine.lock().unwrap().use_wave();
                }
            } else if ui.button("Load sample…").clicked()
                && let Some(path) = rfd::FileDialog::new()
                    .add_filter("Audio", &["wav", "flac", "mp3", "ogg"])
                    .pick_file()
                {
                    self.engine.lock().unwrap().load_sample(&path);
                }

            ui.separator();
            ui.heading("Effects");
            let count = self.engine.lock().unwrap().effect_count();
            for i in 0..count {
                let name = self.engine.lock().unwrap().effect_name(i).to_string();
                let mut on = self.engine.lock().unwrap().effect_on(i);
                let mut mix = self.engine.lock().unwrap().effect_mix(i);
                let on0 = on;
                let mix0 = mix;
                ui.horizontal(|ui| {
                    ui.checkbox(&mut on, name);
                    ui.add_enabled(on, egui::Slider::new(&mut mix, 0.0..=1.0).text("mix"));
                });
                if on != on0 {
                    self.engine.lock().unwrap().set_effect_on(i, on);
                }
                if mix != mix0 {
                    self.engine.lock().unwrap().set_effect_mix(i, mix);
                }
            }
        });

        // ---- central: status + piano-roll editor (NO engine lock held) ----
        let mut preview_out: Option<i32> = None;
        let mut seek_out: Option<usize> = None;
        egui::CentralPanel::default().show(ui, |ui| {
            let tempo = self.engine.lock().unwrap().tempo();
            ui.label(format!(
                "{} · {} rows/octave · BPM {:.0} · {} notes · {} selected",
                self.engine.lock().unwrap().tuning_kind().label(),
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
                        self.engine
                            .lock()
                            .unwrap()
                            .set_custom_ratios(self.custom_ratios_input.clone());
                        let mut e = self.engine.lock().unwrap();
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

        // ---- per-degree note colors (separate window) ----
        if self.show_colors_window {
            let mut open = true;
            egui::Window::new("Note colors — one swatch per pitch class (used by the Custom scheme)")
                .open(&mut open)
                .show(ui.ctx(), |ui| {
                    let spo = self.engine.lock().unwrap().tuning_steps();
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
            let mut e = self.engine.lock().unwrap();
            if *e.pattern() != pat {
                e.set_pattern(pat.clone());
            }
            if let Some(p) = preview_out {
                e.preview_note(p);
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

fn main() -> eframe::Result<()> {
    let (engine, _stream) = audio::start(TuningKind::Equal12);

    let options = eframe::NativeOptions::default();
    eframe::run_native(
        "simple_pianoroll",
        options,
        Box::new(move |_cc| {
            let mut editor = EditorState::default();
            let initial = engine.lock().unwrap().pattern().clone();
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
            }))
        }),
    )
}