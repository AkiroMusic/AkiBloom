# AkiBloom 开发日志

## M0 · 2026-09-26——内核日：能弹（当日完成，提前于 09-30 检查点）

### 完成

- **工程脚手架**：workspace（akibloom + xtask），nih-plug 同 pin f36931f，依赖与 AkiGraft 完全一致
  （nih_plug_egui + rustfft + parking_lot + hound + rfd）；`.cargo/config.toml` xtask 别名、bundler.toml、
  deploy.ps1 全部就位。
- **内核单向复制**（文件头注明来源，`parents.rs` 唯一改动 = `analyze_profile` 改 pub 供种子分析复用）：
  stft.rs（恒等重构精确到浮点误差的 OLA 引擎）、mel.rs（24 频带）、wav.rs（16/24/32bit + 32f）、
  parents.rs（母本画像系统）、graft.rs（GraftCore：频谱包络嫁接 ±12dB 钳制 + 杂交变换 + 种子化哈希）。
- **seeds.rs**（新）：种子 = 画像 + 音频本体（mono 自动复制为立体声），超 10 秒截断（§5 内存约束）。
- **render.rs**（新）：离线渲染管线——种子音频经 GraftCore 拉向供体画像（α 由 GUI 拍照进任务）→
  延迟裁剪（对齐种子）→ 环绕点预交叉渐变（512 样本，消循环接缝咔哒）→ 峰值天花板 0.98（只降不升）。
- **voices.rs**（新）：8 复音播放引擎——线性插值读变体、12-TET 音高映射（根音可调）、
  1ms attack / 10ms release 渐变、最老优先抢音、同音重触发改写。
- **lib.rs**（新）：乐器插件（VST3 Instrument+Sampler / CLAP Instrument）——无音频输入、MIDI 输入、
  事件驱动 note on/off + GUI 虚拟键盘轮询过渡；参数：源 A/B、生长量 α、根音、输出；
  种子路径持久化（会话重载自动恢复）。渲染在 GUI 侧 worker 线程（依赖指纹 FNV 变更触发，
  α 量化 1%，in-flight 跳过），音频线程零渲染零 IO。
- **gui.rs**（新）：薄荷 UI（色板/字体/旋钮/卡片与 AkiGraft 完全同源）——①种子卡×2 ②生长
  （源 A/B + α + 就绪/生长中状态）③演奏（C3–C5 虚拟键盘，点击/拖动滑音，黑键覆盖，无需 MIDI 设备）。
- **akibloom_demo**（新）：离线渲染 CLI（种子 → 变体 wav），无宿主无 GUI 验证链路。

### 验证

- **32/32 单元测试通过**：复制内核的 14 项（恒等/块大小无关/延迟/混合插值/钳制/画像等）
  + 新增 18 项（α=0 恒等且长度=种子−2048−512、α=1 拉向暗供体 RMS 明显下降、
  循环渐变缩短且有限、wav 往返、12-TET 音高比、最老抢音、同音重触发改写、
  attack/ GainSmoother 60ms 后电平到位、release 渐弱到静音且 retire、无变体静音等）。
- **端到端离线渲染**：合成暗/亮两颗种子 → 双向嫁接渲染数值确认
  （暗→亮 α=0.8：RMS −10.2→−5.3 dB，长度 3.00s→2.95s=−延迟−渐变，方向与量级正确）。
- **打包三产物**：`cargo xtask bundle` → VST3 / CLAP / standalone 全部生成。
- **standalone 冒烟**：WASAPI 后端启动存活，正常关闭。

### 已知事项 / M1 待办（10-01 → 10-08，含 10-05 提前过 10-12 门槛）

- 杂交（3 标签 + amount + seed，引擎已就绪，差 GUI + 渲染参数接线）与培育（保留/丢弃 + 音色地图回溯）
- 导出 wav 按钮（复用 render::write_variant）
- 变体索引参数（培育历史间自动化过渡）+ 6 个出厂预设
- REAPER 挂轨实测（乐器 + MIDI 键盘）；mac 字体路径沿 AkiGraft 待办
- α 渲染量化 1%——拖动时变体分步刷新（设计如此），如体验突兀可加淡入交叉

### 复现命令

```
cd AkiBloom
cargo test --release
cargo xtask bundle akibloom --release
powershell -ExecutionPolicy Bypass -File scripts/deploy.ps1
targeteleasekibloom_demo.exe tmp/seed_dark.wav tmp/seed_bright.wav tmp/variant.wav --alpha 0.8
```


---

## D0 · 2026-09-26——立项日：工作区与章程

- 建工作区（独立 git 仓库，main），从 mint-fresh-botanica.zip 归档视觉参考到 `design/mint-fresh/`。
- **PLAN.md v1.0** 定稿：乐器定位（种子→三操作生长→MIDI 演奏→导出闭环）、10-12 投稿硬门槛、
  M0–M3 里程碑、与 AkiGraft 的单向复制边界（只读 `../AkiGraft/`，文件头注明来源）。
- 核实 AkiGraft 现状（W1 完成：STFT 引擎恒等重构精确、嫁接引擎、母本系统、薄荷 UI 骨架、
  14/14 单测、VST3/CLAP/standalone 打包部署）——M0 复制清单以此为据。

### M0 待办（09-26 → 09-30）

1. 脚手架：workspace（akibloom + xtask），nih-plug 同 pin f36931f，依赖同 AkiGraft
2. 复制内核：stft.rs / mel.rs / wav 加载（自 parents.rs 拆）/ GUI 主题·字体·旋钮组件，注明来源
3. 乐器形态：MIDI 输入 + 8 复音采样播放 + 虚拟键盘（无 MIDI 也可试听）
4. 渲染管线：种子 → STFT 嫁接 → iFFT 缓冲 → 变音高映射 → 演奏；恒等/钳制对照单测
5. `cargo xtask bundle` 三产物走通
