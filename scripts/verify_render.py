# -*- coding: utf-8 -*-
"""AkiBloom 离线渲染自测：标签方向性 + 种子确定性。

用法: python scripts/verify_render.py   (在 AkiBloom 根目录, 需先 cargo build --release
     且 tmp/ 下有用 akibloom_demo 生成的 v_wet/v_grain/v_breath/v_det_a/v_det_b/v_seed9)

也可以带 --regen 让脚本自己调 demo CLI 重新生成全部样本。
"""
import math
import struct
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DEMO = ROOT / "target" / "release" / "akibloom_demo.exe"
TMP = ROOT / "tmp"


def read_wav_32f(path):
    b = path.read_bytes()
    i = 12
    fmt = data = None
    while i + 8 <= len(b):
        cid = b[i:i + 4]
        sz = struct.unpack("<I", b[i + 4:i + 8])[0]
        body = b[i + 8:i + 8 + sz]
        if cid == b"fmt ":
            fmt = body
        if cid == b"data":
            data = body
        i += 8 + sz + (sz & 1)
    tag, ch, sr = struct.unpack("<HHI", fmt[:8])
    assert tag in (3, 0xFFFE), f"{path.name}: 不是 32f wav (tag={tag:#x})"
    n = len(data) // 4
    xs = list(struct.unpack("<%df" % n, data))
    # 左声道，跳过循环渐变头 1024 帧
    xs = xs[1024 * ch::ch]
    return xs, sr


def diff_rms_db(xs):
    d = [b - a for a, b in zip(xs, xs[1:])]
    return 20 * math.log10(max(math.sqrt(sum(v * v for v in d) / len(d)), 1e-12))


def render(name, seed, tag):
    out = TMP / name
    r = subprocess.run(
        [str(DEMO), str(TMP / "seed_dark.wav"), str(TMP / "seed_bright.wav"), str(out),
         "--alpha", "0.3", "--tag", str(tag), "--amount", "1.0", "--seed", str(seed)],
        capture_output=True, text=True)
    assert r.returncode == 0, r.stderr
    return out


def main():
    if "--regen" in sys.argv:
        assert DEMO.exists(), "先 cargo build --release"
        TMP.mkdir(exist_ok=True)
        if not (TMP / "seed_dark.wav").exists():
            print("缺少 tmp/seed_dark.wav / seed_bright.wav —— 先用 make 测试种子生成")
            return 1
        render("v_wet.wav", 42, 0)
        render("v_grain.wav", 42, 1)
        render("v_breath.wav", 42, 2)
        render("v_det_a.wav", 42, 0)
        render("v_det_b.wav", 42, 0)
        render("v_seed9.wav", 9, 0)

    ok = True

    def ck(name, cond):
        nonlocal ok
        print(("  OK  " if cond else "  FAIL") + " " + name)
        ok &= cond

    wet, _ = read_wav_32f(TMP / "v_wet.wav")
    grain, _ = read_wav_32f(TMP / "v_grain.wav")
    breath, _ = read_wav_32f(TMP / "v_breath.wav")
    dw, dg, db = diff_rms_db(wet), diff_rms_db(grain), diff_rms_db(breath)
    print(f"高频加权能量(dB):  wet={dw:.2f}  breath={db:.2f}  grain={dg:.2f}")
    ck("标签方向: grainy 比 wet 亮 ≥1dB", dg > dw + 1.0)
    ck("标签方向: breath 介于两者之间", dw < db < dg)
    ck("全有限", all(math.isfinite(v) for v in wet + grain + breath))

    ha = (TMP / "v_det_a.wav").read_bytes()
    hb = (TMP / "v_det_b.wav").read_bytes()
    h9 = (TMP / "v_seed9.wav").read_bytes()
    ck("同种子 → 字节级相同", ha == hb)
    ck("不同种子 → 不同", ha != h9)

    print("ALL PASS" if ok else "FAILURES PRESENT")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
