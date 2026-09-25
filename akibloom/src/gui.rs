//! AkiBloom editor — mint-fresh botanical UI (same palette as AkiGraft).
//!
//! Layout follows the workflow in order: ① 种子 SOURCE → ② 生长 GROW →
//! ③ 演奏 PLAY. Same interaction conventions as AkiGraft: circular dials
//! dragged horizontally (double-click resets, Shift is fine), file loading
//! and variant rendering run on worker threads — the modal rfd dialog must
//! never open on the baseview window thread, and rendering must never touch
//! the audio thread.
//!
//! The virtual keyboard is a first-class control: no MIDI device is needed
//! to audition the grown timbre. Click (or drag across) keys to play; the
//! audio thread polls the shared held-note slot.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use nih_plug::prelude::{Editor, Param, ParamSetter};
use nih_plug_egui::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, RichText, Sense, Stroke,
    Vec2,
};
use nih_plug_egui::{create_egui_editor, EguiState};

use crate::render::{self, RenderJob};
use crate::seeds::{self, SharedSeeds, Slot};
use crate::{AkiBloomParams, SeedSide, SharedVariant, VirtualNote};

// ── Palette (mint-fresh-botanica, identical to AkiGraft) ────────────────

const INK: Color32 = Color32::from_rgb(0x2F, 0x4A, 0x3A); // 深墨绿 · titles
const BODY: Color32 = Color32::from_rgb(0x6B, 0x72, 0x68); // 暖灰 · body
const MINT_BG: Color32 = Color32::from_rgb(0xE8, 0xF0, 0xE5); // 奶白绿 · panel
const CARD_BG: Color32 = Color32::from_rgb(0xF6, 0xFA, 0xF2); // 卡片 · near-white green
const CARD_STROKE: Color32 = Color32::from_rgb(0xC9, 0xDC, 0xCE); // 细线
const MINT: Color32 = Color32::from_rgb(0xB5, 0xD7, 0xC3); // 浅薄荷青 · accent
const GRAY_GREEN: Color32 = Color32::from_rgb(0x8F, 0xA8, 0x9A); // 灰绿

const DEFAULT_SIZE: (u32, u32) = (980, 700);

/// Status text shared between the GUI and load/render workers.
pub type SharedStatus = Arc<Mutex<String>>;

/// Editor state owned by the GUI thread.
pub struct EditorState {
    pub params: Arc<AkiBloomParams>,
    pub seeds: SharedSeeds,
    pub variant: SharedVariant,
    pub status: SharedStatus,
    pub virtual_note: VirtualNote,
    /// Set while a render worker is running (GUI skips spawning).
    pub render_in_flight: Arc<AtomicBool>,
    /// Fingerprint of everything a render depends on.
    pub last_render_key: u64,
}

pub fn create(
    params: Arc<AkiBloomParams>,
    seeds: SharedSeeds,
    variant: SharedVariant,
    status: SharedStatus,
    virtual_note: VirtualNote,
) -> Option<Box<dyn Editor>> {
    let egui_state = EguiState::from_size(DEFAULT_SIZE.0, DEFAULT_SIZE.1);

    create_egui_editor(
        egui_state,
        EditorState {
            params,
            seeds,
            variant,
            status,
            virtual_note,
            render_in_flight: Arc::new(AtomicBool::new(false)),
            last_render_key: 0,
        },
        |ctx, _state| {
            load_fonts(ctx);
            configure_visuals(ctx);
        },
        move |ctx, setter, state| {
            egui::CentralPanel::default().show(ctx, |ui| {
                maybe_spawn_render(state);
                egui::ScrollArea::vertical().show(ui, |ui| render(ui, setter, state));
            });
        },
    )
}

// ── Render scheduling ───────────────────────────────────────────────────

/// Spawn a render worker whenever anything the variant depends on changed
/// (seed files, source side, α in 1% steps). Skipped while a worker is
/// already running — when it finishes, the next frame re-checks.
fn maybe_spawn_render(st: &mut EditorState) {
    let key = {
        let a = st.params.seed_a_path.lock().clone();
        let b = st.params.seed_b_path.lock().clone();
        let mut h = 0xcbf2_9ce4_8422_2325u64; // FNV-1a
        for byte in a.bytes().chain(b.bytes()).chain(
            [
                st.params.side.value() as u8,
                (st.params.alpha.value() * 100.0).round() as u8,
            ]
            .iter()
            .copied(),
        ) {
            h ^= byte as u64;
            h = h.wrapping_mul(0x1000_0000_01b3);
        }
        h
    };
    if key == st.last_render_key || st.render_in_flight.load(Ordering::SeqCst) {
        return;
    }
    st.last_render_key = key;
    spawn_render(st);
}

fn spawn_render(st: &mut EditorState) {
    st.render_in_flight.store(true, Ordering::SeqCst);
    {
        let mut s = st.status.lock().unwrap();
        s.clear();
        s.push_str("正在生长音色…");
    }
    let seeds = st.seeds.clone();
    let variant_slot = st.variant.clone();
    let status = st.status.clone();
    let in_flight = st.render_in_flight.clone();
    let side = st.params.side.value();
    let alpha = st.params.alpha.value();

    std::thread::spawn(move || {
        let (source, donor) = {
            let pair = seeds.read();
            let src = pair.get(side.slot()).cloned();
            let don = pair.get(side.other()).cloned();
            (src, don)
        };
        let Some(source) = source else {
            *status.lock().unwrap() = "先加载至少一个种子，才能生长音色".to_owned();
            in_flight.store(false, Ordering::SeqCst);
            return;
        };
        let job = RenderJob {
            source: &source,
            donor: donor.as_deref(),
            alpha,
            variate: None,
        };
        let variant = Arc::new(render::render_variant(job));
        *variant_slot.write() = Some(variant);
        *status.lock().unwrap() = "音色已生长 · 点击下方琴键试听".to_owned();
        in_flight.store(false, Ordering::SeqCst);
    });
}

// ── Rendering ───────────────────────────────────────────────────────────

fn render(ui: &mut egui::Ui, setter: &ParamSetter, st: &mut EditorState) {
    // Header: name + an explicit statement of what this does.
    ui.horizontal(|ui| {
        ui.label(RichText::new("❀ AkiBloom").size(28.0).color(INK).strong());
        ui.add_space(10.0);
        ui.vertical(|ui| {
            ui.label(
                RichText::new("音色生长乐器 · Timbre-Growing Instrument")
                    .size(15.0)
                    .color(INK)
                    .strong(),
            );
            ui.label(
                RichText::new(
                    "把自己的采样当种子：加载两颗种子 → 推「生长量 α」长出新音色 → 点击琴键演奏",
                )
                .size(12.5)
                .color(BODY),
            );
        });
    });
    ui.add_space(4.0);

    // ── ① 种子 SOURCE ───────────────────────────────────────────────
    section(
        ui,
        "① 种子 · SOURCE",
        "生长的原料。两颗种子时：当前「源」向另一种子的音色靠拢",
        |ui| {
            ui.columns(2, |cols| {
                seed_card(&mut cols[0], st, Slot::A);
                seed_card(&mut cols[1], st, Slot::B);
            });
        },
    );

    // ── ② 生长 GROW ─────────────────────────────────────────────────
    let side = st.params.side.value();
    section(
        ui,
        "② 生长 · GROW",
        "把源种子拉向另一种子的音色（α=0 时保持原声；改动后自动重新生长）",
        |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("源").size(12.5).color(INK));
                for (candidate, label) in [(SeedSide::A, "A"), (SeedSide::B, "B")] {
                    let active = side == candidate;
                    let btn = ui
                        .add(
                            egui::Button::new(
                                RichText::new(label).color(if active { MINT_BG } else { INK }),
                            )
                            .fill(if active { INK } else { CARD_BG })
                            .corner_radius(CornerRadius::same(9)),
                        )
                        .on_hover_text("作为演奏原料的种子；另一种子（若已加载）提供音色");
                    if btn.clicked() {
                        setter.begin_set_parameter(&st.params.side);
                        setter.set_parameter(&st.params.side, candidate);
                        setter.end_set_parameter(&st.params.side);
                    }
                }
                ui.add_space(8.0);
                param_knob(
                    ui,
                    setter,
                    &st.params.alpha,
                    84.0,
                    RichText::new("生长量 α").size(14.0).color(INK).strong(),
                    "0% = 完全原声；拖动向右，音色逐渐长成另一种子的颜色。横向拖动，双击恢复默认",
                );
                ui.add_space(8.0);
                let in_flight = st.render_in_flight.load(Ordering::SeqCst);
                ui.label(
                    RichText::new(if in_flight { "⏳ 生长中…" } else { "✓ 音色就绪" })
                        .size(12.0)
                        .color(if in_flight { GRAY_GREEN } else { BODY }),
                );
            });
        },
    );

    // ── ③ 演奏 PLAY ─────────────────────────────────────────────────
    section(
        ui,
        "③ 演奏 · PLAY",
        "点击琴键试听（拖动可滑音），或接入 MIDI 键盘；根音 = 音色原调",
        |ui| {
            ui.horizontal(|ui| {
                param_knob(
                    ui,
                    setter,
                    &st.params.root,
                    56.0,
                    RichText::new("根音 Root").size(12.5).color(INK),
                    "以该 MIDI 音高演奏音色的原始音高（C4=60）。横向拖动，双击恢复默认",
                );
                param_knob(
                    ui,
                    setter,
                    &st.params.gain,
                    56.0,
                    RichText::new("输出 Output").size(12.5).color(INK),
                    "输出音量。横向拖动，双击恢复 0 dB",
                );
            });
            ui.add_space(4.0);
            virtual_keyboard(ui, st);
        },
    );

    // Footer: status + usage hints (no paper references).
    ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
        let status = st.status.lock().unwrap().clone();
        if !status.is_empty() {
            ui.label(RichText::new(&status).size(11.5).color(BODY));
        }
        ui.label(
            RichText::new(
                "操作：旋钮横向拖动 · Shift 微调 · 双击恢复默认 · α=0 原声 · 键盘 C3–C5 点击/拖动演奏",
            )
            .size(10.5)
            .color(BODY),
        );
    });
}

// ── Virtual keyboard ────────────────────────────────────────────────────

/// Two octaves, C3 (48) → C5 (72). Click to hold a note, release to stop;
/// drag glissandos across keys. Held note goes into the shared slot the
/// audio thread polls.
fn virtual_keyboard(ui: &mut egui::Ui, st: &mut EditorState) {
    const LOW: u8 = 48;
    const HIGH: u8 = 72;
    let is_white = |n: u8| matches!(n % 12, 0 | 2 | 4 | 5 | 7 | 9 | 11);

    let height = 104.0;
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), height),
        Sense::click_and_drag(),
    );
    let painter = ui.painter_at(rect);

    painter.rect_filled(rect, CornerRadius::same(10), MINT_BG);
    painter.rect_stroke(
        rect,
        CornerRadius::same(10),
        Stroke::new(1.0, CARD_STROKE),
        egui::StrokeKind::Middle,
    );

    let held = *st.virtual_note.lock().unwrap();

    // White keys first, then blacks overlay them.
    let whites: Vec<u8> = (LOW..=HIGH).filter(|n| is_white(*n)).collect();
    let w_w = rect.width() / whites.len() as f32;
    let mut white_rects: Vec<(u8, egui::Rect)> = Vec::new();
    for (i, note) in whites.iter().enumerate() {
        let r = egui::Rect::from_min_size(
            egui::pos2(rect.left() + i as f32 * w_w + 1.0, rect.top() + 4.0),
            Vec2::new(w_w - 2.0, height - 8.0),
        );
        white_rects.push((*note, r));
    }
    for (note, r) in &white_rects {
        let active = held == Some(*note);
        painter.rect_filled(
            *r,
            CornerRadius::same(4),
            if active { MINT } else { CARD_BG },
        );
        painter.rect_stroke(
            *r,
            CornerRadius::same(4),
            Stroke::new(1.0, CARD_STROKE),
            egui::StrokeKind::Middle,
        );
    }

    let mut black_rects: Vec<(u8, egui::Rect)> = Vec::new();
    for note in LOW..=HIGH {
        if is_white(note) {
            continue;
        }
        // A black key sits on the boundary between its lower white neighbor
        // and the next white key.
        if let Some((_, below)) = white_rects.iter().rev().find(|(n, _)| *n < note) {
            let x = below.right() - w_w * 0.28;
            let r = egui::Rect::from_min_size(
                egui::pos2(x, rect.top() + 4.0),
                Vec2::new(w_w * 0.56, (height - 8.0) * 0.62),
            );
            black_rects.push((note, r));
        }
    }
    for (note, r) in &black_rects {
        let active = held == Some(*note);
        painter.rect_filled(
            *r,
            CornerRadius::same(3),
            if active { MINT } else { INK },
        );
    }

    // Key labels on C notes.
    for (note, r) in &white_rects {
        if note % 12 == 0 {
            painter.text(
                egui::pos2(r.center().x, r.bottom() - 6.0),
                egui::Align2::CENTER_CENTER,
                match note / 12 {
                    4 => "C3",
                    5 => "C4",
                    6 => "C5",
                    _ => "",
                },
                egui::TextStyle::Small.resolve(ui.style()),
                BODY,
            );
        }
    }

    // Interaction: press → hit test (blacks take priority) and hold the
    // note; any pointer release — inside or after dragging away — stops it.
    let pointer_down = ui.input(|i| i.pointer.any_down());
    let just_released = ui.input(|i| i.pointer.any_released());
    if pointer_down {
        if let Some(pos) = response.interact_pointer_pos() {
            let hit = black_rects
                .iter()
                .find(|(_, r)| r.contains(pos))
                .or_else(|| white_rects.iter().find(|(_, r)| r.contains(pos)));
            if let Some((note, _)) = hit {
                let mut held_slot = st.virtual_note.lock().unwrap();
                if *held_slot != Some(*note) {
                    *held_slot = Some(*note);
                }
            }
        }
    }
    if just_released {
        *st.virtual_note.lock().unwrap() = None;
    }
}

// ── Building blocks (shared with AkiGraft's editor) ─────────────────────

/// A numbered section card, in workflow order.
fn section(
    ui: &mut egui::Ui,
    title: &str,
    subtitle: &str,
    content: impl FnOnce(&mut egui::Ui),
) {
    egui::Frame::default()
        .fill(CARD_BG)
        .stroke(Stroke::new(1.0, CARD_STROKE))
        .corner_radius(CornerRadius::same(14))
        .inner_margin(egui::Margin::same(12))
        .outer_margin(egui::Margin::symmetric(2, 3))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(title).size(14.0).color(INK).strong());
                ui.label(RichText::new(subtitle).size(11.0).color(BODY));
            });
            ui.add_space(3.0);
            content(ui);
        });
}

fn seed_card(ui: &mut egui::Ui, st: &mut EditorState, slot: Slot) {
    let slot_label = match slot {
        Slot::A => "种子 A · Seed A",
        Slot::B => "种子 B · Seed B",
    };
    let hint = "生长的原料采样。两颗都加载后，「生长量 α」把源种子的音色拉向这里的另一颗";

    egui::Frame::default()
        .fill(MINT_BG)
        .stroke(Stroke::new(1.0, CARD_STROKE))
        .corner_radius(CornerRadius::same(11))
        .inner_margin(egui::Margin::same(10))
        .show(ui, |ui| {
            ui.set_min_height(96.0);
            let title = ui.label(RichText::new(slot_label).size(13.0).color(INK).strong());
            title.on_hover_text(hint);
            ui.add_space(3.0);

            let (name, info) = {
                let pair = st.seeds.read();
                match pair.get(slot) {
                    Some(s) => (s.name().to_owned(), s.info_line()),
                    None => (
                        "（空 · empty）".to_owned(),
                        "点下方按钮加载一个 wav".to_owned(),
                    ),
                }
            };
            let name_resp = ui.label(RichText::new(name).size(13.5).color(INK));
            name_resp.on_hover_text(hint);
            ui.label(RichText::new(info).size(11.0).color(BODY));
            ui.add_space(5.0);

            let button = ui
                .add(
                    egui::Button::new(RichText::new("⇪ 加载 Load").color(INK))
                        .fill(MINT)
                        .corner_radius(CornerRadius::same(9)),
                )
                .on_hover_text("选择一个 wav 采样作为种子（支持 16/24/32bit，超过 10 秒自动截断）");
            if button.clicked() {
                spawn_load(
                    st.status.clone(),
                    st.seeds.clone(),
                    st.params.clone(),
                    slot,
                );
            }
        });
}

/// Open the file dialog and load the seed on a dedicated thread (same
/// crash-avoidance pattern as AkiGraft: never rfd on the window thread,
/// panics surface as status text). The path is persisted for session reload.
fn spawn_load(
    status: SharedStatus,
    seeds: SharedSeeds,
    params: Arc<AkiBloomParams>,
    slot: Slot,
) {
    {
        let mut s = status.lock().unwrap();
        s.clear();
        s.push_str("正在打开文件选择器…");
    }
    std::thread::spawn(move || {
        let status_worker = status.clone();
        let result = std::panic::catch_unwind(
            move || -> Result<Option<(seeds::SeedSample, String)>, String> {
                let Some(path) = rfd::FileDialog::new()
                    .add_filter("WAV audio", &["wav", "wave"])
                    .pick_file()
                else {
                    return Ok(None);
                };
                *status_worker.lock().unwrap() = "正在分析采样…".to_owned();
                let path_str = path.to_string_lossy().into_owned();
                seeds::load_seed(&path).map(|s| Some((s, path_str)))
            },
        )
        .unwrap_or_else(|_| Err("加载线程内部错误（请更换文件重试）".to_owned()));

        match result {
            Ok(Some((seed, path_str))) => {
                let info = seed.info_line();
                seeds.write().set(slot, Arc::new(seed));
                let path_lock = match slot {
                    Slot::A => &params.seed_a_path,
                    Slot::B => &params.seed_b_path,
                };
                *path_lock.lock() = path_str;
                *status.lock().unwrap() = format!("已加载 {info}");
            }
            Ok(None) => {} // user cancelled the dialog
            Err(e) => {
                *status.lock().unwrap() = format!("加载失败：{e}");
            }
        }
    });
}

// ── Knob (horizontal-drag dial, same convention as AkiGraft) ────────────

/// A circular dial: drag horizontally to adjust, Shift for fine steps,
/// double-click to reset, scroll wheel to nudge. All writes go through the
/// `ParamSetter` so automation gestures are recorded correctly by the host.
fn param_knob(
    ui: &mut egui::Ui,
    setter: &ParamSetter,
    param: &impl Param,
    size: f32,
    label: RichText,
    hint: &str,
) {
    ui.vertical(|ui| {
        let (rect, response) = ui.allocate_exact_size(Vec2::splat(size), Sense::click_and_drag());
        let response = response.on_hover_text(hint);

        if response.dragged() {
            let fine = ui.input(|i| i.modifiers.shift);
            let speed = if fine { 0.0008 } else { 0.004 };
            let delta = response.drag_delta().x * speed;
            if delta != 0.0 {
                let v = (param.modulated_normalized_value() + delta).clamp(0.0, 1.0);
                setter.begin_set_parameter(param);
                setter.set_parameter_normalized(param, v);
                setter.end_set_parameter(param);
                ui.ctx().request_repaint();
            }
        }
        if response.double_clicked() {
            setter.begin_set_parameter(param);
            setter.set_parameter_normalized(param, param.default_normalized_value());
            setter.end_set_parameter(param);
        }
        if response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
            ui.ctx().request_repaint();
        }
        let scroll = ui.input(|i| i.raw_scroll_delta.y);
        if response.hovered() && scroll != 0.0 {
            let v = (param.modulated_normalized_value() - scroll * 0.0008).clamp(0.0, 1.0);
            setter.begin_set_parameter(param);
            setter.set_parameter_normalized(param, v);
            setter.end_set_parameter(param);
            ui.ctx().request_repaint();
        }

        // Paint.
        let painter = ui.painter_at(rect.expand(3.0));
        let c = rect.center();
        let r = rect.width() / 2.0 - 3.0;
        let norm = param.modulated_normalized_value();

        painter.circle_filled(c, r, MINT_BG);
        painter.circle_stroke(c, r, Stroke::new(1.5, CARD_STROKE));

        const SWEEP: f32 = std::f32::consts::PI * 1.5;
        const START: f32 = std::f32::consts::PI * 0.75;
        let end = START + SWEEP * norm;
        if norm > 0.004 {
            let segments = ((SWEEP * norm / 0.15).ceil() as usize).max(2);
            let points: Vec<_> = (0..=segments)
                .map(|s| {
                    let a = START + (end - START) * (s as f32 / segments as f32);
                    c + Vec2::new(a.cos(), a.sin()) * (r - 4.0)
                })
                .collect();
            painter.add(egui::Shape::line(points, Stroke::new(3.0, INK)));
        }

        let dir = Vec2::new(end.cos(), end.sin());
        painter.line_segment(
            [c + dir * (r * 0.35), c + dir * (r - 6.0)],
            Stroke::new(2.0, INK),
        );

        let value_text = param.normalized_value_to_string(norm, true);
        ui.set_min_width(size + 120.0);
        ui.label(RichText::new(value_text).size(11.0).color(BODY));
        let label_resp = ui.label(label);
        label_resp.on_hover_text(hint);
    });
}

// ── Fonts & visuals (identical to AkiGraft) ─────────────────────────────

/// Runtime-load a system CJK font so bilingual labels render. Falls back
/// silently to egui defaults (Latin-only) when absent.
fn load_fonts(ctx: &egui::Context) {
    let candidates = [
        "C:\\Windows\\Fonts\\msyh.ttc",       // Microsoft YaHei (Windows 10/11)
        "C:\\Windows\\Fonts\\msyhl.ttc",      // YaHei Light
        "/System/Library/Fonts/PingFang.ttc", // macOS fallback
    ];
    let mut fonts = FontDefinitions::default();
    for path in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            fonts
                .font_data
                .insert("cjk".to_owned(), FontData::from_owned(bytes).into());
            fonts
                .families
                .get_mut(&FontFamily::Proportional)
                .unwrap()
                .insert(0, "cjk".to_owned());
            break;
        }
    }
    ctx.set_fonts(fonts);
}

fn configure_visuals(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    style.visuals.panel_fill = MINT_BG;
    style.visuals.window_fill = MINT_BG;
    style.visuals.extreme_bg_color = CARD_BG;
    style.visuals.override_text_color = Some(INK);
    style.visuals.window_corner_radius = CornerRadius::same(12);
    style.visuals.menu_corner_radius = CornerRadius::same(12);
    style.visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, INK);
    for widget in [
        &mut style.visuals.widgets.inactive,
        &mut style.visuals.widgets.hovered,
        &mut style.visuals.widgets.active,
        &mut style.visuals.widgets.open,
    ] {
        widget.corner_radius = CornerRadius::same(9);
        widget.fg_stroke = Stroke::new(1.0, INK);
        widget.bg_fill = CARD_BG;
        widget.bg_stroke = Stroke::new(1.0, CARD_STROKE);
    }
    style.visuals.selection.stroke = Stroke::new(1.5, INK);
    style.visuals.selection.bg_fill = MINT;
    ctx.set_style(style);
}
