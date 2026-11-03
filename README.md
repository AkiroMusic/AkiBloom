# AkiBloom — Timbre-Growing Instrument / 音色生长乐器

[![Website](https://img.shields.io/badge/site-akiromusic.github.io-6D82FF)](https://akiromusic.github.io/AkiBloom/)
**English** | [中文](#中文)

Grow new playable timbres from your own samples. AkiBloom is an instrument
plugin built on three "organic operations" — **graft** one seed's timbre
toward another, **hybridize** it along metaphor directions (wet · grainy ·
breathing), **cultivate** it with keep/discard feedback and a walkable
timbre map. Every grown timbre can be exported as WAV and used as a parent
in the sibling effect [AkiGraft](https://github.com/AkiroMusic/AkiGraft) —
the two plugins form one closed workflow.

The interaction design comes from the research project
[Botanica-OS](https://github.com/AkiroMusic/Botanica-OS) (*Organic Operations
on Neural Latent Spaces*): reorganizing latent-space interaction as graft /
hybridize / cultivate. AkiBloom is that framework realized as a shipping
instrument — no neural runtime required, pure real-time-safe DSP.

## Quick start

1. Load **two of your own WAV samples** as seeds (16/24/32-bit, mono or
   stereo; longer than 10 s is trimmed).
2. Drag **Grow α** — the source seed is pulled toward the other seed's
   timbre. α = 0 keeps the original sound.
3. Play: click the on-screen keyboard (C3–C5) or plug in a MIDI keyboard.
4. **Hybridize**: pick a direction (wet / grainy / breathing), set the
   amount, reroll the seed. Same seed → same variation, always.
5. **Cultivate**: turn it on, click *next mutation*, audition on the keys,
   then keep 👍 or discard ✗. The timbre map remembers every step; click
   any dot to roll back to it.
6. **Export** the current timbre as WAV — drop it into AkiGraft as a parent.

## Formats & requirements

- Windows x64: **VST3** · **CLAP** · **standalone** (WASAPI)
- Any DAW for VST3/CLAP; the standalone needs no DAW (use `--midi-input` to
  pick a MIDI device)

## Controls

| Control | What it does |
|---|---|
| 源 Source A/B | Which seed is the played source; the other donates its timbre |
| 生长量 Grow α | How far the source is pulled toward the donor (0 = original) |
| 方向 / 变异量 / 种子 | Hybridize direction, depth, deterministic seed |
| 培育 Cultivate | Walker takes over the variation; keep/discard + map rollback |
| 根音 Root / 微调 Tune | Native pitch note (C4=60) and ±1 semitone fine tune |
| 输出 Output | Output level |

Knobs: drag horizontally, Shift for fine, double-click to reset.

## Building from source

```bash
cargo test --release
cargo xtask bundle akibloom --release   # VST3 + CLAP + standalone
python scripts/verify_render.py         # offline render self-test
```

Rust toolchain (MSVC) required; bundling uses [nih-plug](https://github.com/robbert-vdh/nih-plug)'s xtask.

## License

GPL-3.0. © AkiroMusic. Entry for the 2026 叮咚音频「投肯杯」AI audio software
contest; sibling project of AkiGraft.

---

## 中文

把**你自己的采样**当种子，长出可以演奏的新音色。AkiBloom 是一件建立在三种「有机化操作」上的乐器插件：**嫁接**——把一颗种子的音色向另一颗拉拢；**杂交**——沿隐喻方向（湿润 · 颗粒 · 呼吸感）做确定性变异；**培育**——保留/丢弃反馈引导演化，音色地图记录每一步、点击即可回溯。长好的音色可一键导出 WAV，直接当作姊妹项目 [AkiGraft](https://github.com/AkiroMusic/AkiGraft) 嫁接效果器的母本——两件插件构成一个完整闭环。

交互框架来自研究项目 [Botanica-OS](https://github.com/AkiroMusic/Botanica-OS)（《神经潜在空间的「有机化」操作》）：把潜在空间交互重组为嫁接 / 杂交 / 培育。AkiBloom 是这一框架的产品化落地——无需神经网络运行时，纯实时安全 DSP。

### 快速上手

1. 加载**两颗你自己的 wav 采样**作种子（16/24/32bit，单声道或立体声；超 10 秒自动截取）。
2. 推**生长量 α**——源种子向另一种子的音色靠拢；α=0 保持原声。
3. 演奏：点击界面琴键（C3–C5），或接入 MIDI 键盘。
4. **杂交**：选方向（湿润/颗粒/呼吸感）、推变异量、换种子。同一种子永远得到同一个变异。
5. **培育**：开启后点「下一变异」，弹琴试听，👍保留 / ✗丢弃；地图记住每一步，点任意圆点回溯。
6. **导出**当前音色为 wav——直接拖进 AkiGraft 当母本。

### 格式与环境

- Windows x64：**VST3** · **CLAP** · **独立运行版**（WASAPI）
- VST3/CLAP 适用于任意宿主；独立版无需宿主（`--midi-input` 选 MIDI 设备）

### 旋钮操作

横向拖动调节 · Shift 微调 · 双击恢复默认。

### 从源码构建

```bash
cargo test --release
cargo xtask bundle akibloom --release   # VST3 + CLAP + 独立版
python scripts/verify_render.py         # 离线渲染自测
```

### 许可

GPL-3.0 · © AkiroMusic · 2026 叮咚音频「投肯杯」参赛作品 · AkiGraft 的姊妹项目
