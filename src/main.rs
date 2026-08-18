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
}

impl eframe::App for PianoRollApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Space toggles play/pause (brief lock).
        if ui.input(|i| i.key_pressed(egui::Key::Space)) {
            let mut e = self.engine.lock().unwrap();
            let p = !e.playing();
            e.set_playing(p);
        }

        // Snapshot engine state (brief locks), then the rest runs lock-free.
        let mut pat = self.engine.lock().unwrap().pattern().clone();
        let spo = self.engine.lock().unwrap().tuning_kind().steps_per_octave() as i32;
        let ph = self.engine.lock().unwrap().playhead_step();

        // ---- keyboard shortcuts (work on the local pattern + editor) ----
        {
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
                let slide = ui.add(egui::Slider::new(&mut tlen, 8..=256).step_by(1.0).text("steps"));
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
                    self.editor.names = "C C# D D# E F F# G G# A A# B".to_string();
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
            ui.add(egui::Slider::new(&mut tb.attack, 1.0..=2000.0).text("Attack").logarithmic(true));
            ui.add(egui::Slider::new(&mut tb.hold, 0.0..=2000.0).text("Hold").logarithmic(true));
            ui.add(egui::Slider::new(&mut tb.decay, 0.0..=2000.0).text("Decay").logarithmic(true));
            ui.add(egui::Slider::new(&mut tb.sustain, 0.0..=1.0).text("Sustain"));
            ui.add(egui::Slider::new(&mut tb.release, 1.0..=2000.0).text("Release").logarithmic(true));
            ui.add(egui::Slider::new(&mut tb.gain, 0.0..=2.0).text("Gain"));
            if tb != tb_orig {
                self.engine.lock().unwrap().set_timbre(tb);
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
            let mut initial = engine.lock().unwrap().pattern().clone();
            editor.begin_edit(&mut initial); // seed the history with the initial state
            Ok(Box::new(PianoRollApp { engine, editor }))
        }),
    )
}