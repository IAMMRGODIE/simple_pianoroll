//! Piano-roll editor — every note gets its own egui `Response`.
//!
//! The editor works on a `&mut Pattern` (a snapshot owned by the caller) and
//! reports `preview` / `seek` requests instead of touching the engine, so the
//! UI never holds the engine lock across rendering — the real-time audio thread
//! can always run, avoiding lock starvation / freezes.

use std::collections::HashSet;

use eframe::egui;
use eframe::egui::{Align2, Color32, PointerButton, Pos2, Rect, Response, Sense, Stroke, Vec2};

use crate::pattern::{BAR_STEPS, Note, Pattern, STEPS_PER_BEAT};

const KEY_W: f32 = 58.0;
const TOP_H: f32 = 26.0;
/// Top strip of the ruler: wheel here pans horizontally; below it zooms.
const RULER_PAN_H: f32 = 9.0;
const ROW_H: f32 = 13.0;
/// Edge-grab zone (px), shrinks with note width so short notes keep a movable body.
const EDGE_PX: f32 = 16.0;
const SCROLL_MIN: f32 = -16.0;
const SCROLL_MAX: f32 = 90.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Edge {
    Left,
    Right,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scheme {
    ByPitchClass,
    ByOctave,
    Flat,
}

impl Scheme {
    pub fn all() -> &'static [Scheme] {
        &[Scheme::ByPitchClass, Scheme::ByOctave, Scheme::Flat]
    }
    pub fn label(self) -> &'static str {
        match self {
            Scheme::ByPitchClass => "By pitch class",
            Scheme::ByOctave => "By octave",
            Scheme::Flat => "Flat",
        }
    }
}

/// Active gesture state (persisted on the App between frames).
pub enum Drag {
    /// shift+drag on empty: draw a long note (committed on drag_stopped).
    Draw { pitch: i32, start_step: usize, cur_step: usize },
    /// ctrl+drag: rubber-band selection (toggles).
    Marquee { start: Pos2, cur: Pos2 },
    /// dragging a note body: move the selection group.
    NoteMove {
        ids: Vec<u64>,
        orig: Vec<(i32, usize)>, // (pitch, start) per id
        hit_id: u64,
        last_pitch: Option<i32>,
    },
    /// dragging a note edge: resize the selection group.
    NoteResize {
        ids: Vec<u64>,
        orig: Vec<(usize, usize)>, // (start, len) per id
        edge: Edge,
        hit_id: u64,
    },
}

/// All persistent editor UI state (kept on the App between frames).
pub struct EditorState {
    pub selection: HashSet<u64>,
    pub drag: Option<Drag>,
    pub last_note_len: usize,
    pub step_px: f32,
    pub view_left: f32,
    pub view_top: f32,
    pub scheme: Scheme,
    pub snap: usize,
    /// User-configurable base note names (space/comma separated), fed to the auto-namer.
    pub names: String,
    pub clipboard: Vec<Note>,
    erasing: bool,
    history: Vec<Pattern>,
    history_pos: usize,
}

impl Default for EditorState {
    fn default() -> Self {
        Self {
            selection: HashSet::new(),
            drag: None,
            last_note_len: BAR_STEPS,
            step_px: 16.0,
            view_left: 0.0,
            view_top: 60.0,
            scheme: Scheme::ByPitchClass,
            snap: 1,
            names: "C C# D D# E F F# G G# A A# B".to_string(),
            clipboard: Vec::new(),
            erasing: false,
            history: Vec::new(),
            history_pos: 0,
        }
    }
}

impl EditorState {
    /// Record the current pattern as an undo point (call *before* an edit).
    pub fn begin_edit(&mut self, pat: &mut Pattern) {
        self.history.truncate(self.history_pos + 1);
        self.history.push(pat.clone());
        if self.history.len() > 100 {
            self.history.remove(0);
        }
        self.history_pos = self.history.len() - 1;
    }

    pub fn undo(&mut self, pat: &mut Pattern) {
        if self.history_pos > 0 {
            self.history_pos -= 1;
            *pat = self.history[self.history_pos].clone();
            self.selection.clear();
        }
    }

    pub fn redo(&mut self, pat: &mut Pattern) {
        if self.history_pos + 1 < self.history.len() {
            self.history_pos += 1;
            *pat = self.history[self.history_pos].clone();
            self.selection.clear();
        }
    }

    /// Copy currently-selected notes into the internal clipboard.
    pub fn copy_selected(&mut self, pat: &Pattern) {
        self.clipboard = pat.notes.iter().filter(|n| self.selection.contains(&n.id)).cloned().collect();
    }

    /// Paste the clipboard so its first note starts at the playhead.
    pub fn paste_at_playhead(&mut self, pat: &mut Pattern, ph: usize) {
        if self.clipboard.is_empty() {
            return;
        }
        self.begin_edit(pat);
        let clip = self.clipboard.clone();
        let min_s = clip.iter().map(|n| n.start_step).min().unwrap_or(0);
        let off = ph as i64 - min_s as i64;
        let total = pat.total_steps as i64;
        let mut new_sel = HashSet::new();
        for n in &clip {
            let ns = (n.start_step as i64 + off).clamp(0, (total - 1).max(0)) as usize;
            let id = pat.duplicate(n, ns, n.length_steps);
            new_sel.insert(id);
        }
        self.selection = new_sel;
    }

    /// Duplicate the selection, offset right by each note's length; selects only copies.
    pub fn duplicate_selected(&mut self, pat: &mut Pattern) {
        if self.selection.is_empty() {
            return;
        }
        self.begin_edit(pat);
        let selected: Vec<Note> = pat.notes.iter().filter(|n| self.selection.contains(&n.id)).cloned().collect();
        let total = pat.total_steps;
        let mut new_sel = HashSet::new();
        for n in &selected {
            let ns = (n.start_step + n.length_steps).min(total.saturating_sub(1));
            let id = pat.duplicate(n, ns, n.length_steps);
            new_sel.insert(id);
        }
        self.selection = new_sel;
    }
}

fn note_color(pitch: i32, spo: i32, scheme: Scheme) -> Color32 {
    match scheme {
        Scheme::Flat => Color32::from_rgb(96, 200, 96),
        Scheme::ByOctave => {
            let o = pitch.div_euclid(spo.max(1));
            let h = ((o as f32) * 0.15).rem_euclid(1.0);
            Color32::from(egui::ecolor::Hsva::new(h, 0.6, 0.75, 1.0))
        }
        Scheme::ByPitchClass => {
            let d = pitch.rem_euclid(spo.max(1)) as f32 / spo.max(1) as f32;
            Color32::from(egui::ecolor::Hsva::new(d, 0.65, 0.75, 1.0))
        }
    }
}

/// Build the auto note name for a pitch: a user-configurable base spelling
/// (per degree within the octave) plus the current octave. Degrees without a
/// configured name fall back to plain degree numbers, so unfamiliar EDOs stay
/// readable and users who know another notation can supply their own names.
fn auto_note_name(pitch: i32, spo: i32, names_spec: &str) -> String {
    let names: Vec<String> = names_spec
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    let deg = pitch.rem_euclid(spo) as usize;
    let octave = 4 + pitch.div_euclid(spo);
    let base = match names.get(deg) {
        Some(name) => name.clone(),
        None => deg.to_string(),
    };
    format!("{base}{octave}")
}

/// Horizontal zoom keeping the pitch under `mx` fixed; returns (factor-applied
/// step width, new left-edge step).
fn zoom_at(
    view_left: f32,
    step_px: f32,
    mx: f32,
    ui_left: f32,
    scroll_y: f32,
    total: f32,
) -> (f32, f32) {
    let cur_step = (view_left + (mx - ui_left) / step_px).clamp(0.0, total);
    let factor = (scroll_y * 0.01).exp();
    let new_sp = (step_px * factor).clamp(6.0, 64.0);
    let new_left = (cur_step - (mx - ui_left) / new_sp).clamp(-1.0, total);
    (new_sp, new_left)
}

fn snap_to(v: i32, snap: usize) -> i32 {
    if snap < 1 {
        return v;
    }
    ((v as f32 / snap as f32).round() as i32) * snap as i32
}

#[allow(clippy::too_many_arguments)]
pub fn show(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    pat: &mut Pattern,
    spo: i32,
    playhead_step: usize,
    preview: &mut Option<i32>,
    seek: &mut Option<usize>,
) {
    let total_steps = pat.total_steps.max(1) as i32;
    let snap = state.snap;

    let size = ui.available_size();
    let (bg, painter) = ui.allocate_painter(size, Sense::hover());
    let origin = bg.rect.min;
    let ui_left = origin.x + KEY_W;
    let ui_top = origin.y + TOP_H;
    let width = bg.rect.width();
    let height = bg.rect.height();
    let grid_bottom = origin.y + height;
    let rows_visible = ((height - TOP_H) / ROW_H).ceil().max(0.0) as i32;

    // ---- input: modifiers & wheel ----
    // Read the raw wheel event(s): egui converts ctrl (and ctrl+shift) wheel
    // into Event::Zoom and zeroes smooth_scroll_delta, so we read the raw delta
    // plus the wheel event's own modifiers and decide pan/zoom/scroll ourselves.
    let wheel = ui.input(|i| {
        let mut d = Vec2::ZERO;
        let mut wc = false;
        let mut ws = false;
        for e in &i.events {
            if let egui::Event::MouseWheel { delta, modifiers, .. } = e {
                d = *delta;
                wc = modifiers.command || modifiers.ctrl;
                ws = modifiers.shift;
            }
        }
        (d, wc, ws)
    });
    let (scrolled, wheel_ctrl, wheel_shift) = wheel;
    let scrolled = scrolled * 16.0;

    let hover = ui.input(|i| i.pointer.hover_pos());
    let shift = ui.input(|i| i.modifiers.shift);
    let ctrl = ui.input(|i| i.modifiers.command || i.modifiers.ctrl);

    // Wheel, in priority order:
    //  - shift (on the wheel) -> pan horizontally (ctrl is ignored when shift is held)
    //  - ctrl (on the wheel)  -> zoom around the cursor (anywhere)
    //  - ruler top strip -> pan; lower ruler -> zoom; grid -> vertical scroll
    let ruler_pan_bottom = origin.y + RULER_PAN_H;
    if let Some(hp) = hover
        && scrolled != Vec2::ZERO {
            if wheel_shift {
                state.view_left += (- scrolled.x - scrolled.y) * 0.10;
            } else if wheel_ctrl {
                let (sp, vl) = zoom_at(state.view_left, state.step_px, hp.x, ui_left, scrolled.y, total_steps as f32);
                state.step_px = sp;
                state.view_left = vl;
            } else if hp.y < ruler_pan_bottom {
                state.view_left += (- scrolled.x - scrolled.y) * 0.10;
            } else if hp.y < ui_top {
                let (sp, vl) = zoom_at(state.view_left, state.step_px, hp.x, ui_left, scrolled.y, total_steps as f32);
                state.step_px = sp;
                state.view_left = vl;
            } else {
                state.view_top = (state.view_top + scrolled.y * 0.15).clamp(SCROLL_MIN, SCROLL_MAX);
            }
            state.view_left = state.view_left.clamp(-2.0, total_steps as f32);
        }

    let step_px = state.step_px;
    let view_left = state.view_left;
    let view_top = state.view_top;

    let x_of = |s: f32| ui_left + (s - view_left) * step_px;
    let step_at = |x: f32| view_left + (x - ui_left) / step_px;
    let y_of = |p: f32| ui_top + (view_top - p) * ROW_H;
    let pitch_of = |y: f32| view_top - (y - ui_top) / ROW_H;
    let top_pitch = view_top.ceil() as i32;
    let first_vis_step = (step_at(ui_left).floor() as i32).max(-1);
    let last_vis_step = (step_at(ui_left + width).ceil() as i32).min(total_steps) + 1;

    // ---- background rows ----
    for p in (top_pitch - rows_visible - 1..=top_pitch + 1).rev() {
        let y = y_of(p as f32);
        if y > grid_bottom || y + ROW_H < ui_top {
            continue;
        }
        let row_color = if p.rem_euclid(spo) == 0 {
            Color32::from_rgb(38, 40, 50)
        } else if p.rem_euclid(2) == 0 {
            Color32::from_rgb(30, 31, 36)
        } else {
            Color32::from_rgb(26, 27, 31)
        };
        painter.rect_filled(
            Rect::from_min_max(Pos2::new(ui_left, y), Pos2::new(ui_left + width - KEY_W, y + ROW_H)),
            0.0,
            row_color,
        );
    }

    // ---- ruler band + gridlines ----
    let ruler_rect = Rect::from_min_max(origin, Pos2::new(origin.x + width, ui_top));
    painter.rect_filled(ruler_rect, 0.0, Color32::from_rgb(20, 20, 26));
    // divider between the "pan" strip (top) and the "zoom" strip (below)
    painter.line_segment(
        [Pos2::new(origin.x, origin.y + RULER_PAN_H), Pos2::new(origin.x + width, origin.y + RULER_PAN_H)],
        Stroke::new(1.0, Color32::from_rgb(70, 70, 84)),
    );
    let grid_rect =
        Rect::from_min_max(Pos2::new(ui_left, ui_top), Pos2::new(ui_left + width - KEY_W, grid_bottom));
    for st in first_vis_step..=last_vis_step {
        let x = x_of(st as f32);
        if x < origin.x || x > origin.x + width {
            continue;
        }
        // Brightness tiers: 16-step bar = brightest, 4-step beat = medium, else faint.
        let tier = if st.rem_euclid(BAR_STEPS as i32) == 0 {
            2
        } else if st.rem_euclid(STEPS_PER_BEAT as i32) == 0 {
            1
        } else {
            0
        };
        let (ruler_c, grid_c, w): (Color32, Color32, f32) = match tier {
            2 => (Color32::from_rgb(150, 150, 170), Color32::from_rgb(78, 78, 92), 1.5),
            1 => (Color32::from_rgb(110, 110, 130), Color32::from_rgb(60, 60, 74), 1.0),
            _ => (Color32::from_rgb(60, 60, 72), Color32::from_rgb(46, 46, 54), 0.5),
        };
        painter.line_segment(
            [Pos2::new(x, origin.y), Pos2::new(x, ui_top)],
            Stroke::new(1.0, ruler_c),
        );
        if tier == 2 && st >= 0 {
            painter.text(
                Pos2::new(x + 2.0, origin.y + 2.0),
                Align2::LEFT_TOP,
                format!("{st}"),
                egui::FontId::proportional(9.0),
                Color32::from_rgb(180, 180, 190),
            );
        }
        painter.line_segment(
            [Pos2::new(x, ui_top), Pos2::new(x, grid_bottom)],
            Stroke::new(w, grid_c),
        );
    }

    // ---- key column ----
    painter.rect_filled(
        Rect::from_min_max(Pos2::new(origin.x, ui_top), Pos2::new(ui_left, grid_bottom)),
        0.0,
        Color32::from_rgb(20, 20, 25),
    );
    for p in (top_pitch - rows_visible - 1..=top_pitch + 1).rev() {
        let y = y_of(p as f32);
        if y > grid_bottom || y + ROW_H < ui_top {
            continue;
        }
        let label = auto_note_name(p, spo, &state.names);
        painter.text(
            Pos2::new(ui_left - 4.0, y + ROW_H * 0.5),
            Align2::RIGHT_CENTER,
            label,
            egui::FontId::proportional(9.0),
            if p.rem_euclid(spo) == 0 { Color32::WHITE } else { Color32::from_rgb(150, 150, 160) },
        );
    }

    // ---- note geometry (index aligns with pat.notes order) ----
    let note_rects: Vec<(u64, Rect)> = pat
        .notes
        .iter()
        .map(|n| {
            let x0 = x_of(n.start_step as f32);
            let y0 = y_of(n.pitch_index as f32) + 1.0;
            let w = (n.length_steps as f32 * step_px - 1.0).max(1.0);
            (n.id, Rect::from_min_size(Pos2::new(x0, y0), Vec2::new(w, ROW_H - 2.0)))
        })
        .collect();
    for (n, r) in pat.notes.iter().zip(note_rects.iter().map(|(_, r)| r)) {
        if r.right() < ui_left || r.left() > ui_left + width - KEY_W {
            continue;
        }
        painter.rect_filled(*r, 3.0, note_color(n.pitch_index, spo, state.scheme));
        painter.rect_stroke(
            *r,
            3.0,
            Stroke::new(1.0, if state.selection.contains(&n.id) { Color32::WHITE } else { Color32::from_rgb(20, 20, 20) }),
            egui::StrokeKind::Outside,
        );
        // note label: custom label, or the auto note name when wide enough
        if r.width() > 17.0 {
            let text = if n.label.is_empty() {
                auto_note_name(n.pitch_index, spo, &state.names)
            } else {
                n.label.clone()
            };
            // left-aligned label, slightly inset from the note's left edge
            painter.text(
                Pos2::new(r.left() + 3.0, r.center().y),
                Align2::LEFT_CENTER,
                text,
                egui::FontId::proportional(9.0),
                Color32::from_rgb(20, 20, 20),
            );
        }
    }

    // ---- playhead ----
    let px = x_of(playhead_step as f32);
    painter.line_segment(
        [Pos2::new(px, ui_top), Pos2::new(px, grid_bottom)],
        Stroke::new(2.0, Color32::from_rgb(255, 180, 60)),
    );

    // ---- gesture previews ----
    if let Some(Drag::Marquee { start, cur }) = &state.drag {
        let m = Rect::from_two_pos(*start, *cur);
        painter.rect_filled(m, 0.0, Color32::from_rgba_unmultiplied(80, 140, 255, 40));
        painter.rect_stroke(m, 0.0, Stroke::new(1.0, Color32::from_rgb(120, 180, 255)), egui::StrokeKind::Outside);
    }
    if let Some(Drag::Draw { pitch, start_step, cur_step, .. }) = &state.drag {
        let s0 = (*start_step).min(*cur_step);
        let s1 = (*start_step).max(*cur_step);
        let preview_rect = Rect::from_min_size(
            Pos2::new(x_of(s0 as f32), y_of(*pitch as f32) + 1.0),
            Vec2::new((s1 - s0 + 1) as f32 * step_px - 1.0, ROW_H - 2.0),
        );
        painter.rect_filled(preview_rect, 3.0, note_color(*pitch, spo, state.scheme).gamma_multiply(0.5));
    }

    // ===================== interaction =====================
    let in_grid = |pos: Pos2| grid_rect.contains(pos);
    let pitch_clamp = |p: i32| p.clamp(-2 * spo, 7 * spo);
    let step_floor = |x: f32| step_at(x).floor() as i32;

    // right-button erase-drag
    let secondary = ui.input(|i| i.pointer.button_down(PointerButton::Secondary));
    if !secondary {
        state.erasing = false;
    }
    if secondary
        && let Some(pos) = hover
            && in_grid(pos) && note_rects.iter().any(|(_, r)| r.contains(pos)) {
                if !state.erasing {
                    state.begin_edit(pat);
                    state.erasing = true;
                }
                if let Some(idx) = note_rects.iter().position(|(_, r)| r.contains(pos)) {
                    let rid = note_rects[idx].0;
                    pat.notes.remove(idx);
                    state.selection.remove(&rid);
                }
            }

    // ---- ruler response: seek / scrub ----
    let ruler_resp = ui.interact(ruler_rect, egui::Id::new("pr_ruler"), Sense::click_and_drag());
    if (ruler_resp.clicked() || ruler_resp.dragged())
        && let Some(pos) = ruler_resp.interact_pointer_pos()
    {
        let st = step_floor(pos.x).clamp(0, total_steps - 1).max(0) as usize;
        *seek = Some(st);
    }

    // ---- grid background response ----
    let grid_resp = ui.interact(grid_rect, egui::Id::new("pr_grid"), Sense::click_and_drag());
    if grid_resp.drag_started()
        && let Some(p0) = grid_resp.interact_pointer_pos()
    {
        let cell = (pitch_of(p0.y).round() as i32, step_floor(p0.x));
        if shift {
            state.begin_edit(pat);
            state.drag = Some(Drag::Draw {
                pitch: pitch_clamp(cell.0),
                start_step: ((snap_to(cell.1, snap)).max(0)) as usize,
                cur_step: ((snap_to(cell.1, snap)).max(0)) as usize,
            });
        } else if ctrl {
            state.drag = Some(Drag::Marquee { start: p0, cur: p0 });
        }
    }
    if grid_resp.dragged()
        && let Some(pos) = grid_resp.interact_pointer_pos()
    {
        match &mut state.drag {
            Some(Drag::Draw { start_step, cur_step, .. }) => {
                let ns = snap_to(step_floor(pos.x), snap).max(0);
                *cur_step = ns.max(0) as usize;
                *start_step = (*start_step as i32).min(ns) as usize;
            }
            Some(Drag::Marquee { cur, .. }) => {
                *cur = pos;
            }
            _ => {}
        }
    }
    if grid_resp.drag_stopped() {
        match state.drag.take() {
            Some(Drag::Draw { pitch, start_step, cur_step, .. }) => {
                let s0 = start_step.min(cur_step);
                let s1 = start_step.max(cur_step);
                let len = (s1 - s0 + 1).max(1);
                let id = pat.add_note(pitch, s0, len, 0.8);
                state.last_note_len = len;
                state.selection.clear();
                state.selection.insert(id);
                *preview = Some(pitch);
            }
            Some(Drag::Marquee { start, cur }) => {
                let m = Rect::from_two_pos(start, cur);
                let mut newsel = state.selection.clone();
                for (n, r) in pat.notes.iter().zip(note_rects.iter().map(|(_, r)| r)) {
                    if r.intersects(m) {
                        if newsel.contains(&n.id) {
                            newsel.remove(&n.id);
                        } else {
                            newsel.insert(n.id);
                        }
                    }
                }
                state.selection = newsel;
            }
            _ => {}
        }
    }
    if grid_resp.clicked()
        && let Some(pos) = grid_resp.interact_pointer_pos()
    {
        let st = snap_to(step_floor(pos.x), snap);
        let p = pitch_of(pos.y).round() as i32;
        state.begin_edit(pat);
        let id = pat.add_note(p, st.max(0) as usize, state.last_note_len.max(1).min(total_steps as usize), 0.8);
        if !shift {
            state.selection.clear();
        }
        state.selection.insert(id);
        *preview = Some(p);
    }

    // ---- per-note responses ----
    let edge_zone = |r: &Rect| EDGE_PX.min(r.width() * 0.30);
    let mut note_resps: Vec<(u64, Rect, Response)> = Vec::new();
    for (id, r) in &note_rects {
        let resp = ui.interact(*r, egui::Id::new(("pr_note", *id)), Sense::click_and_drag());
        note_resps.push((*id, *r, resp));
    }

    for (nid, r, resp) in &note_resps {
        // click: select / shift-click duplicate or add-to-selection
        if resp.clicked()
            && let Some(n) = pat.notes.iter().find(|x| x.id == *nid).cloned()
        {
            if shift {
                if state.selection.contains(&n.id) {
                    // shift + click a selected note -> duplicate the WHOLE selection in place
                    state.begin_edit(pat);
                    let selected: Vec<Note> = pat
                        .notes
                        .iter()
                        .filter(|x| state.selection.contains(&x.id))
                        .cloned()
                        .collect();
                    let mut new_sel = HashSet::new();
                    for x in &selected {
                        let id = pat.duplicate(x, x.start_step, x.length_steps);
                        new_sel.insert(id);
                    }
                    state.selection = new_sel;
                    if let Some(x) = selected.last() {
                        state.last_note_len = x.length_steps;
                        *preview = Some(x.pitch_index);
                    }
                } else {
                    // shift + click an unselected note -> add it to the selection
                    state.selection.insert(n.id);
                    state.last_note_len = n.length_steps;
                }
            } else {
                state.selection.clear();
                state.selection.insert(n.id);
                state.last_note_len = n.length_steps;
            }
        }

        // drag start: decide move vs resize
        if resp.drag_started()
            && let Some(p0) = resp.interact_pointer_pos()
        {
            let ez = edge_zone(r);
            let dl = (p0.x - r.left()).abs();
            let dr = (r.right() - p0.x).abs();

            let was_selected = state.selection.contains(nid);
            let mut sel = state.selection.clone();
            if !was_selected && !shift {
                sel.clear();
                sel.insert(*nid);
            } else {
                sel.insert(*nid);
            }
            let group_ids: Vec<u64> = sel.iter().cloned().collect();
            let hit_note = pat.notes.iter().find(|x| x.id == *nid).cloned();
            state.begin_edit(pat);
            state.selection = sel;

            let edge = if dl <= ez { Some(Edge::Left) } else if dr <= ez { Some(Edge::Right) } else { None };
            if let Some(edge) = edge {
                let orig: Vec<(usize, usize)> = group_ids
                    .iter()
                    .map(|iid| pat.notes.iter().find(|n| n.id == *iid).map(|n| (n.start_step, n.length_steps)).unwrap_or((0, 1)))
                    .collect();
                state.drag = Some(Drag::NoteResize { ids: group_ids, orig, edge, hit_id: *nid });
            } else if let Some(n) = hit_note {
                let orig: Vec<(i32, usize)> = group_ids
                    .iter()
                    .map(|iid| pat.notes.iter().find(|x| x.id == *iid).map(|n| (n.pitch_index, n.start_step)).unwrap_or((0, 0)))
                    .collect();
                state.drag = Some(Drag::NoteMove { ids: group_ids, hit_id: n.id, last_pitch: Some(n.pitch_index), orig });
            }
        }

        // drag update
        if resp.dragged()
            && let Some(delta) = resp.total_drag_delta()
        {
            let mut resize_len_after: Option<usize> = None;
            match &mut state.drag {
                Some(Drag::NoteMove { ids, orig, hit_id, last_pitch }) => {
                    let d_pitch = (-delta.y / ROW_H).round() as i32;
                    let d_step = snap_to((delta.x / step_px).round() as i32, snap);
                    let new_pitch = {
                        let notes_list = &mut pat.notes;
                        for (id, (op, os)) in ids.iter().zip(orig.iter()) {
                            if let Some(n) = notes_list.iter_mut().find(|n| &n.id == id) {
                                n.pitch_index = pitch_clamp(*op + d_pitch);
                                n.start_step = ((*os as i32 + d_step).clamp(0, total_steps - 1)) as usize;
                            }
                        }
                        notes_list.iter().find(|n| n.id == *hit_id).map(|n| n.pitch_index)
                    };
                    if let Some(np) = new_pitch
                        && *last_pitch != Some(np)
                    {
                        *preview = Some(np);
                        *last_pitch = Some(np);
                    }
                }
                Some(Drag::NoteResize { ids, orig, edge, hit_id }) => {
                    let d = snap_to((delta.x / step_px).round() as i32, snap);
                    let notes_list = &mut pat.notes;
                    for (id, (os, ol)) in ids.iter().zip(orig.iter()) {
                        if let Some(n) = notes_list.iter_mut().find(|n| &n.id == id) {
                            match edge {
                                Edge::Right => {
                                    n.length_steps = ((*ol as i32) + d).clamp(1, total_steps - *os as i32) as usize;
                                }
                                Edge::Left => {
                                    let ns = ((*os as i32) + d)
                                        .clamp(0, (*os as i32 + *ol as i32 - 1).min(total_steps - 1));
                                    n.start_step = ns as usize;
                                    n.length_steps = ((*os + *ol) as i32 - ns).max(1) as usize;
                                }
                            }
                        }
                    }
                    // sync the click-to-add default length to the resized note
                    resize_len_after = pat.notes.iter().find(|n| n.id == *hit_id).map(|n| n.length_steps);
                }
                _ => {}
            }
            if let Some(len) = resize_len_after {
                state.last_note_len = len;
            }
        }

        // drag end
        if resp.drag_stopped()
            && matches!(state.drag, Some(Drag::NoteMove { .. }) | Some(Drag::NoteResize { .. }))
        {
            state.drag = None;
        }
    }

    // ---- cursor feedback ----
    if let Some(pos) = hover
        && in_grid(pos) {
            let near_edge = note_rects.iter().any(|(_, r)| {
                let ez = edge_zone(r);
                (pos.x - r.left()).abs() <= ez || (r.right() - pos.x).abs() <= ez
            });
            if near_edge {
                ui.ctx().output_mut(|o| o.cursor_icon = egui::CursorIcon::ResizeHorizontal);
            } else if note_rects.iter().any(|(_, r)| r.contains(pos)) {
                ui.ctx().output_mut(|o| o.cursor_icon = egui::CursorIcon::Grab);
            }
        }

    let _ = bg;
}