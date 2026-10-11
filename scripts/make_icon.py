"""生成应用图标 assets/icon.ico：深色圆角底 + 渐变色耳机。"""
from __future__ import annotations

from pathlib import Path

from PIL import Image, ImageDraw

S = 256
img = Image.new("RGBA", (S, S), (0, 0, 0, 0))
d = ImageDraw.Draw(img)

# 深色圆角底
d.rounded_rectangle([8, 8, S - 8, S - 8], radius=56, fill=(22, 26, 34, 255))

# 渐变头梁（紫→青，逐段绘制）
purple, cyan = (124, 92, 255), (34, 211, 238)
steps = 48
band_box = [64, 52, 192, 176]
for i in range(steps):
    t = i / (steps - 1)
    c = tuple(int(purple[k] + (cyan[k] - purple[k]) * t) for k in range(3))
    seg = 180 / steps
    d.arc(band_box, start=180 + i * seg, end=180 + (i + 1) * seg + 1, fill=c, width=18)

# 左右耳罩（渐变色圆角竖条）
for x, t in ((60, 0.0), (172, 1.0)):
    c = tuple(int(purple[k] + (cyan[k] - purple[k]) * t) for k in range(3))
    d.rounded_rectangle([x, 140, x + 26, 208], radius=12, fill=c)

# 底部点缀：三条声波弧线（半透明青）
for r, a in ((18, 160), (30, 110), (42, 70)):
    d.arc([128 - r, 196 - r // 2, 128 + r, 196 + r * 2], start=250, end=290,
          fill=(34, 211, 238, a), width=6)

out = Path(__file__).resolve().parent.parent / "assets" / "icon.ico"
out.parent.mkdir(parents=True, exist_ok=True)
img.save(out, sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)])
img.save(out.with_suffix(".png"))
print("icon ->", out)
