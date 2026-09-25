//! AkiBloom editor — mint-fresh botanical UI (same palette as AkiGraft).
//!
//! Layout follows the workflow in order: ① 种子 SOURCE → ② 生长 GROW →
//! ③ 杂交 HYBRIDIZE → ④ 培育 CULTIVATE → ⑤ 演奏 PLAY. Same interaction
//! conventions as AkiGraft: circular dials dragged horizontally (double-click
//! resets, Shift is fine), file loading and variant rendering run on worker
//! threads — the modal rfd dialog must never open on the baseview window
//! thread, and rendering must never touch the audio thread.
//!
//! The virtual keyboard is a first-class control: no MIDI device is needed
//! to audition the grown timbre. Click (or drag across) keys to play; the
//! audio thread polls the shared held-note slot.
//!
//! Every control that changes what the variant *is* (seeds, side, α, the
//! resolved variation state) feeds a fingerprint; a change spawns a render
//! worker that publishes a fresh `Arc<PlayableVariant>`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use nih_plug::prelude::{Editor, Param, ParamSetter};
use nih_plug_egui::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, RichText, Sense, Stroke,
    Vec2,
};
use nih_plug_egui::{create_egui_editor, EguiState};

use crate::cultivate::Cultivate;
use crate::render::{self, RenderJob};
use crate::seeds::{self, SharedSeeds, Slot};
use crate::{AkiBloomParams, SeedSide, SharedVariant, VariateTag, VirtualNote};

// ── Palette (mint-fresh-botanica, identical to AkiGraft) ────────────────

const INK: Color32 = Color32::from_rgb(0x2F, 0x4A, 0x3A); // 深墨绿 · titles
const BODY: Color32 = Color32::from_rgb(0x6B, 0x72, 0x68); // 暖灰 · body
const MINT_BG: Color32 = Color32::from_rgb(0xE8, 0xF0, 0xE5); // 奶白绿 · panel
const CARD_BG: Color32 = Color32::from_rgb(0xF6, 0xFA, 0xF2); // 卡片 · near-white green
const CARD_STROKE: Color32 = Color32::from_rgb(0xC9, 0xDC, 0xCE); // 细线
const MINT: Color32 = Color32::from_rgb(0xB5, 0xD7, 0xC3); // 浅薄荷青 · accent
const GRAY_GREEN: Color32 = Color32::from_rgb(0x8F, 0xA8, 0x9A); // 灰绿

const DEFAULT_SIZE: (u32, u32) = (980, 860);

/// Status text shared between the GUI and load/render workers.
pub type SharedStatus = Arc<Mutex<String>>;

/// Editor state owned by the GUI thread.
pub struct EditorState {
    pub params: Arc<AkiBloomParams>,
    pub seeds: SharedSeeds,
    pub variant: SharedVariant,
    pub status: SharedStatus,
    pub virtual_note: VirtualNote,
    pub cultivate: Arc<parking_lot::RwLock<Cultivate>>,
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
    cultivate: Arc<parking_lot::RwLock<Cultivate>>,
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
            cultivate,
            render_in_flight: Arc::new(AtomicBool::new(false)),
            last_render_key: 0,
        },
        |ctx, state| {
            load_fonts(ctx);
            configure_visuals(ctx);
            // Entropy for the walker's RNG, once per editor.
            state.cultivate.write().reseed_from_clock();
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

/// FNV-1a over everything the rendered variant depends on. The variation is
/// quantized (α to 1%, amount to 1%) so smoothing doesn't spam renders.
fn render_key(st: &EditorState) -> (u64, bool) {
    let a = st.params.seed_a_path.lock().clone();
    let b = st.params.seed_b_path.lock().clone();
    let side = st.params.side.value();
    let alpha = st.params.alpha.value();
    let cultivating = st.params.cultivate_on.value();
    let variate = crate::resolve_variate_from(
        cultivating,
        crate::manual_variate(&st.params),
        {
            let c = st.cultivate.read();
            c.active()
        },
    );
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut mix = |bytes: &[u8]| {
        for byte in bytes {
            h ^= *byte as u64;
            h = h.wrapping_mul(0x1000_0000_01b3);
        }
    };
    mix(a.as_bytes());
    mix(b.as_bytes());
    mix(&[side as u8, (alpha * 100.0).round() as u8, cultivating as u8]);
    if let Some(v) = variate {
        mix(&[v.tag, (v.amount * 100.0).round() as u8]);
        mix(&v.seed.to_le_bytes());
    }
    (h, cultivating)
}

/// Spawn a render worker whenever anything the variant depends on changed.
/// Skipped while a worker is already running — the next frame re-checks.
fn maybe_spawn_render(st: &mut EditorState) {
    let (key, cultivating) = render_key(st);
    if key == st.last_render_key || st.render_in_flight.load(Ordering::SeqCst) {
        return;
    }
    st.last_render_key = key;
    spawn_render(st, cultivating);
}

fn spawn_render(st: &mut EditorState, cultivating: bool) {
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
    let cultivate = st.cultivate.clone();
    let params = st.params.clone();

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
        let variate = crate::resolve_variate_from(
            cultivating,
            crate::manual_variate(&params),
            cultivate.read().active(),
        );
        let job = RenderJob {
            source: &source,
            donor: donor.as_deref(),
            alpha,
            variate,
        };
        let variant = Arc::new(render::render_variant(job));
        let has_var = variate.is_some();
        *variant_slot.write() = Some(variant);
        *status.lock().unwrap() = if has_var {
            "音色已生长 · 点击下方琴键试听".to_owned()
        } else {
            format!("音色已就绪（α={:.0}%）· 点击琴键试听", alpha * 100.0)
        };
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
    preset_row(ui, setter, &st.params);
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

    let cultivating = st.params.cultivate_on.value();

    // ── ③ 杂交 HYBRIDIZE（手动变异）────────────────────────────────
    section(
        ui,
        "③ 杂交 · HYBRIDIZE",
        "按方向标签做确定性变异（同一种子 = 同一结果），与生长叠加生效",
        |ui| {
            ui.add_enabled_ui(!cultivating, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("方向").size(12.5).color(INK));
                    for (variant, label) in [
                        (VariateTag::Wet, "湿润 Wet"),
                        (VariateTag::Grainy, "颗粒 Grainy"),
                        (VariateTag::Breath, "呼吸感 Breath"),
                    ] {
                        let active = st.params.tag.value() == variant;
                        let btn = ui
                            .add(
                                egui::Button::new(RichText::new(label).color(if active {
                                    MINT_BG
                                } else {
                                    INK
                                }))
                                .fill(if active { INK } else { CARD_BG })
                                .corner_radius(CornerRadius::same(9)),
                            )
                            .on_hover_text(match variant {
                                VariateTag::Wet => "湿润：中低频更饱满、高频收敛，缓慢的液态流动感",
                                VariateTag::Grainy => "颗粒：高频提亮 + 频谱梳状纹理，细碎的颗粒闪光感",
                                VariateTag::Breath => "呼吸感：轻微高频气声，叠加缓慢的音量起伏（像呼吸）",
                            });
                        if btn.clicked() {
                            setter.begin_set_parameter(&st.params.tag);
                            setter.set_parameter(&st.params.tag, variant);
                            setter.end_set_parameter(&st.params.tag);
                        }
                    }

                    param_knob(
                        ui,
                        setter,
                        &st.params.var_amount,
                        52.0,
                        RichText::new("变异量").size(12.5).color(INK),
                        "变异深度：0% = 不变异。横向拖动，双击恢复默认",
                    );

                    ui.label(
                        RichText::new(format!("种子 {}", st.params.seed.value()))
                            .size(12.0)
                            .color(BODY),
                    );
                    if ui
                        .add(
                            egui::Button::new(RichText::new("⟳ 新变异").color(INK))
                                .fill(MINT)
                                .corner_radius(CornerRadius::same(9)),
                        )
                        .on_hover_text("换一个随机种子——同样的种子永远得到同样的变异")
                        .clicked()
                    {
                        let new_seed = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| (d.subsec_nanos() as i32).max(1))
                            .unwrap_or(1);
                        setter.begin_set_parameter(&st.params.seed);
                        setter.set_parameter(&st.params.seed, new_seed);
                        setter.end_set_parameter(&st.params.seed);
                    }
                });
            });
            if cultivating {
                ui.label(
                    RichText::new("培育模式进行中——手动杂交控件暂时交由培育接管")
                        .size(11.0)
                        .color(BODY),
                );
            }
        },
    );

    // ── ④ 培育 CULTIVATE ────────────────────────────────────────────
    section(
        ui,
        "④ 培育 · CULTIVATE",
        "自动漫步探索：⟳ 生成变异 → 弹琴试听 → 👍保留 / ✗丢弃，地图可点回任何一步",
        |ui| {
            ui.horizontal(|ui| {
                let on = cultivating;
                let toggle = ui
                    .add(
                        egui::Button::new(
                            RichText::new(if on { "● 培育中" } else { "○ 培育模式" }).color(
                                if on { MINT_BG } else { INK },
                            ),
                        )
                        .fill(if on { INK } else { CARD_BG })
                        .corner_radius(CornerRadius::same(9)),
                    )
                    .on_hover_text("开启后，培育接管③的变异，自动在音色空间里漫步探索");
                if toggle.clicked() {
                    setter.begin_set_parameter(&st.params.cultivate_on);
                    setter.set_parameter(&st.params.cultivate_on, !on);
                    setter.end_set_parameter(&st.params.cultivate_on);
                    if !on {
                        st.cultivate.write().reseed_from_clock();
                    }
                }

                param_knob(
                    ui,
                    setter,
                    &st.params.walk,
                    52.0,
                    RichText::new("漫步幅度").size(12.5).color(INK),
                    "每一步变异的探索范围：越大跳得越远。横向拖动，双击恢复默认",
                );

                ui.add_enabled_ui(on, |ui| {
                    if ui
                        .add(
                            egui::Button::new(RichText::new("⟳ 下一变异").color(INK))
                                .fill(CARD_BG)
                                .corner_radius(CornerRadius::same(9)),
                        )
                        .on_hover_text("生成并试听下一步变异（弹琴试听后再保留/丢弃）")
                        .clicked()
                    {
                        st.cultivate.write().next_candidate();
                    }
                    if ui
                        .add(
                            egui::Button::new(RichText::new("👍 保留").color(MINT_BG))
                                .fill(INK)
                                .corner_radius(CornerRadius::same(9)),
                        )
                        .on_hover_text("当前变异不错：设为新基准，并让漫步偏向这个方向")
                        .clicked()
                    {
                        st.cultivate.write().keep();
                    }
                    if ui
                        .add(
                            egui::Button::new(RichText::new("✗ 丢弃").color(INK))
                                .fill(CARD_BG)
                                .corner_radius(CornerRadius::same(9)),
                        )
                        .on_hover_text("不喜欢：退回上一个保留的基准")
                        .clicked()
                    {
                        st.cultivate.write().discard();
                    }
                });
            });

            timbre_map(ui, st);
        },
    );

    // ── ⑤ 演奏 PLAY ─────────────────────────────────────────────────
    section(
        ui,
        "⑤ 演奏 · PLAY",
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
                    &st.params.tune,
                    56.0,
                    RichText::new("微调 Tune").size(12.5).color(INK),
                    "相对根音的微调（±1 个半音），用于对齐音高。横向拖动，双击归零",
                );
                param_knob(
                    ui,
                    setter,
                    &st.params.gain,
                    56.0,
                    RichText::new("输出 Output").size(12.5).color(INK),
                    "输出音量。横向拖动，双击恢复 0 dB",
                );
                ui.add_space(10.0);
                if ui
                    .add(
                        egui::Button::new(RichText::new("⇩ 导出 Export").color(INK))
                            .fill(MINT)
                            .corner_radius(CornerRadius::same(9)),
                    )
                    .on_hover_text("把当前音色导出为 wav（可直接当 AkiGraft 的母本使用）")
                    .clicked()
                {
                    spawn_export(st);
                }
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
                "操作：旋钮横向拖动 · Shift 微调 · 双击恢复默认 · α=0 原声 · 同种子=同变异 · 键盘 C3–C5 点击/拖动",
            )
            .size(10.5)
            .color(BODY),
        );
    });
}

// ── Timbre map (培育地图) ───────────────────────────────────────────────

/// The cultivate map: every landed step as a dot over the seed×amount plane.
/// Click a dot to roll the base back to it (snapshot rollback) — the change
/// re-renders the variant through the fingerprint.
fn timbre_map(ui: &mut egui::Ui, st: &mut EditorState) {
    let height = 168.0;
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

    for i in 1..8 {
        let x = rect.left() + rect.width() * (i as f32 / 8.0);
        painter.line_segment(
            [egui::pos2(x, rect.top() + 4.0), egui::pos2(x, rect.bottom() - 4.0)],
            Stroke::new(0.5, CARD_STROKE),
        );
    }
    for i in 1..4 {
        let y = rect.top() + rect.height() * (i as f32 / 4.0);
        painter.line_segment(
            [egui::pos2(rect.left() + 4.0, y), egui::pos2(rect.right() - 4.0, y)],
            Stroke::new(0.5, CARD_STROKE),
        );
    }

    painter.text(
        egui::pos2(rect.right() - 6.0, rect.bottom() - 22.0),
        egui::Align2::RIGHT_BOTTOM,
        "种子 →",
        egui::TextStyle::Small.resolve(ui.style()),
        BODY,
    );
    painter.text(
        egui::pos2(rect.left() + 6.0, rect.top() + 6.0),
        egui::Align2::LEFT_TOP,
        "↑ 变异量",
        egui::TextStyle::Small.resolve(ui.style()),
        BODY,
    );

    let (history, candidate, base) = {
        let c = st.cultivate.read();
        (c.history.clone(), c.candidate, c.base)
    };

    let to_pos = |state: crate::graft::VariateState| {
        let fx = (state.seed % 10_007) as f32 / 10_007.0;
        let x = rect.left() + 14.0 + fx * (rect.width() - 28.0);
        let y = rect.top() + 14.0 + (1.0 - state.amount) * (rect.height() - 28.0);
        egui::pos2(x, y)
    };

    let mut dots: Vec<(egui::Pos2, usize)> = Vec::new();
    for step in history.iter() {
        let p = to_pos(step.state);
        dots.push((p, step.n));
        let color = if step.kept { INK } else { GRAY_GREEN };
        if step.kept {
            painter.circle_filled(p, 3.5, color);
        } else {
            painter.circle_stroke(p, 3.0, Stroke::new(1.2, color));
        }
    }
    // Current base: mint disc with ink outline.
    let base_pos = to_pos(base);
    painter.circle_filled(base_pos, 5.0, MINT);
    painter.circle_stroke(base_pos, 5.5, Stroke::new(1.5, INK));
    // Auditioned candidate: hollow bright ring.
    if let Some(c) = candidate {
        let p = to_pos(c);
        painter.circle_stroke(p, 5.5, Stroke::new(2.0, INK));
    }

    // Click → rollback to the nearest landed step within range.
    if response.clicked() {
        if let Some(pos) = response.interact_pointer_pos() {
            let mut best: Option<(f32, usize)> = None;
            for (p, n) in &dots {
                let d = p.distance(pos);
                if d < 12.0 && best.map(|(bd, _)| d < bd).unwrap_or(true) {
                    best = Some((d, *n));
                }
            }
            if let Some((_, n)) = best {
                st.cultivate.write().rollback(n);
                *st.status.lock().unwrap() = format!("已回溯到第 {n} 步（音色重新生长中）");
            }
        }
    }

    painter.text(
        egui::pos2(rect.left() + 8.0, rect.bottom() - 8.0),
        egui::Align2::LEFT_BOTTOM,
        "● 已保留　○ 尝试过　◉ 当前基准　◎ 试听中　点击圆点可回溯",
        egui::TextStyle::Small.resolve(ui.style()),
        BODY,
    );
}

// ── Export ──────────────────────────────────────────────────────────────

/// Save-dialog + WAV export on a worker (never rfd on the window thread).
fn spawn_export(st: &mut EditorState) {
    let variant = st.variant.read().clone();
    let status = st.status.clone();
    std::thread::spawn(move || {
        let Some(variant) = variant else {
            *status.lock().unwrap() = "还没有可导出的音色——先加载种子生长一个".to_owned();
            return;
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            rfd::FileDialog::new()
                .add_filter("WAV audio", &["wav"])
                .set_file_name("AkiBloom-timbre.wav")
                .save_file()
        }))
        .unwrap_or(None);
        let Some(path) = result else {
            return; // cancelled
        };
        match render::write_variant(&path, &variant) {
            Ok(()) => {
                *status.lock().unwrap() =
                    format!("已导出 {}（可作 AkiGraft 母本）", path.display());
            }
            Err(e) => {
                *status.lock().unwrap() = format!("导出失败：{e}");
            }
        }
    });
}

// ── Factory presets ─────────────────────────────────────────────────────

/// Six bilingual factory presets — one-click starting points for the demo.
fn preset_row(ui: &mut egui::Ui, setter: &ParamSetter, p: &AkiBloomParams) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("预设 Presets:").size(11.5).color(BODY));
        let presets: [(&str, &str, fn(&ParamSetter, &AkiBloomParams)); 6] = [
            ("原种", "Seed As-Is", apply_seed_asis),
            ("均衡嫁接", "Balanced Graft", apply_balanced),
            ("深度嫁接", "Deep Graft", apply_deep),
            ("湿润之雾", "Wet Haze", apply_wet_haze),
            ("颗粒碎化", "Grain Crush", apply_grain_crush),
            ("呼吸氛围", "Breath Pad", apply_breath_pad),
        ];
        for (zh, en, apply) in presets {
            if ui
                .add(
                    egui::Button::new(RichText::new(format!("{zh} {en}")).size(10.5).color(INK))
                        .fill(CARD_BG)
                        .corner_radius(CornerRadius::same(8)),
                )
                .on_hover_text(format!("一键切换预设：{zh} · {en}"))
                .clicked()
            {
                apply(setter, p);
            }
        }
    });
}

fn set_f32(setter: &ParamSetter, param: &nih_plug::prelude::FloatParam, v: f32) {
    setter.begin_set_parameter(param);
    setter.set_parameter(param, v);
    setter.end_set_parameter(param);
}

fn set_i32(setter: &ParamSetter, param: &nih_plug::prelude::IntParam, v: i32) {
    setter.begin_set_parameter(param);
    setter.set_parameter(param, v);
    setter.end_set_parameter(param);
}

fn preset_common_off(setter: &ParamSetter, p: &AkiBloomParams) {
    setter.begin_set_parameter(&p.cultivate_on);
    setter.set_parameter(&p.cultivate_on, false);
    setter.end_set_parameter(&p.cultivate_on);
}

fn apply_seed_asis(setter: &ParamSetter, p: &AkiBloomParams) {
    set_f32(setter, &p.alpha, 0.0);
    set_f32(setter, &p.var_amount, 0.0);
    preset_common_off(setter, p);
}

fn apply_balanced(setter: &ParamSetter, p: &AkiBloomParams) {
    set_f32(setter, &p.alpha, 0.5);
    set_f32(setter, &p.var_amount, 0.0);
    preset_common_off(setter, p);
}

fn apply_deep(setter: &ParamSetter, p: &AkiBloomParams) {
    set_f32(setter, &p.alpha, 0.9);
    set_f32(setter, &p.var_amount, 0.0);
    preset_common_off(setter, p);
}

fn apply_wet_haze(setter: &ParamSetter, p: &AkiBloomParams) {
    set_f32(setter, &p.alpha, 0.35);
    set_f32(setter, &p.var_amount, 0.35);
    setter.begin_set_parameter(&p.tag);
    setter.set_parameter(&p.tag, VariateTag::Wet);
    setter.end_set_parameter(&p.tag);
    set_i32(setter, &p.seed, 7);
    preset_common_off(setter, p);
}

fn apply_grain_crush(setter: &ParamSetter, p: &AkiBloomParams) {
    set_f32(setter, &p.alpha, 0.3);
    set_f32(setter, &p.var_amount, 0.5);
    setter.begin_set_parameter(&p.tag);
    setter.set_parameter(&p.tag, VariateTag::Grainy);
    setter.end_set_parameter(&p.tag);
    set_i32(setter, &p.seed, 11);
    preset_common_off(setter, p);
}

fn apply_breath_pad(setter: &ParamSetter, p: &AkiBloomParams) {
    set_f32(setter, &p.alpha, 0.45);
    set_f32(setter, &p.var_amount, 0.45);
    setter.begin_set_parameter(&p.tag);
    setter.set_parameter(&p.tag, VariateTag::Breath);
    setter.end_set_parameter(&p.tag);
    set_i32(setter, &p.seed, 23);
    preset_common_off(setter, p);
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
