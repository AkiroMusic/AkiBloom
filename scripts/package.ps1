# Package a portable Windows zip: VST3 + CLAP + standalone + readme.
# Usage: powershell -ExecutionPolicy Bypass -File scripts/package.ps1
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$bundled = Join-Path $root "target\bundled"
if (-not (Test-Path $bundled)) {
    Write-Error "target\bundled 不存在——先运行: cargo xtask bundle akibloom --release"
}

$version = "0.1.0"
$stage = Join-Path $root "target\package\AkiBloom-$version-win64"
if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
New-Item -ItemType Directory -Force -Path $stage | Out-Null

Copy-Item -Recurse -Force (Join-Path $bundled "AkiBloom.vst3") $stage
Copy-Item -Force (Join-Path $bundled "AkiBloom.clap") $stage
Copy-Item -Force (Join-Path $bundled "AkiBloom.exe") $stage

$readme = @"
AkiBloom $version — 音色生长乐器 / Timbre-Growing Instrument
================================================

【安装 · Install】
- VST3: 把 AkiBloom.vst3 整个文件夹放入
  C:\Program Files\Common Files\VST3\（全局）
  或 %LOCALAPPDATA%\Programs\Common\VST3\（仅当前用户）
- CLAP: 把 AkiBloom.clap 放入
  C:\Program Files\Common Files\CLAP\ 或对应 CLAP 目录
- 独立运行: 双击 AkiBloom.exe（无需 DAW；--midi-input 选 MIDI 设备）

【快速上手 · Quick Start】
1. 加载两颗你自己的 wav 采样作种子（超 10 秒自动截取）
2. 推「生长量 α」——源种子向另一种子的音色靠拢；α=0 保持原声
3. 点击界面琴键（C3–C5）或接 MIDI 键盘演奏
4. ③ 杂交：选方向（湿润/颗粒/呼吸感），推变异量，⟳ 换种子
   （同一种子永远得到同一个变异）
5. ④ 培育：开启培育模式 → ⟳ 下一变异 → 弹琴试听 → 👍保留 / ✗丢弃，
   地图上点任意圆点可回溯到那一步
6. ⑤ 演奏：根音/微调对齐音高；⇩ 导出当前音色为 wav
   （可直接当姊妹项目 AkiGraft 嫁接效果器的母本）

【小贴士 · Tips】
- 旋钮：横向拖动调节 / Shift 微调 / 双击恢复默认
- 建议种子选与目标音色气质相近的采样
- 湿润=中低频饱满·高频收敛；颗粒=高频提亮·梳状纹理；呼吸感=气声+缓慢起伏

GPL-3.0 · (c) AkiroMusic · 投肯杯参赛作品 · AkiGraft 姊妹项目
"@
$readme | Out-File -FilePath (Join-Path $stage "使用说明_README.txt") -Encoding utf8

$zip = Join-Path $root "target\package\AkiBloom-$version-win64.zip"
if (Test-Path $zip) { Remove-Item -Force $zip }
Compress-Archive -Path $stage -DestinationPath $zip
Write-Output "打包完成: $zip"
Get-Item $zip | Select-Object Name, @{n="SizeMB"; e={[math]::Round($_.Length/1MB, 2)}}
