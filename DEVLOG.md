# AkiBloom 开发日志

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
