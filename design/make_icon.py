"""Render the Space Maker app icon (1024x1024 PNG)."""
from PIL import Image, ImageDraw, ImageFilter
import colorsys

S = 2048  # supersample, downscaled at the end
img = Image.new("RGBA", (S, S), (0, 0, 0, 0))

def hsl(h, s, l, a=255):
    r, g, b = colorsys.hls_to_rgb(h / 360, l / 100, s / 100)
    return (int(r * 255), int(g * 255), int(b * 255), a)

# macOS icon grid: 824px body inside 1024 canvas, ~185px corner radius.
m = int(S * 100 / 1024)
body = (m, m, S - m, S - m)
radius = int(S * 185 / 1024)

# Drop shadow.
shadow = Image.new("RGBA", (S, S), (0, 0, 0, 0))
ImageDraw.Draw(shadow).rounded_rectangle((body[0], body[1] + 24, body[2], body[3] + 24), radius, fill=(0, 0, 0, 150))
img.alpha_composite(shadow.filter(ImageFilter.GaussianBlur(40)))

# Background gradient.
bg = Image.new("RGBA", (S, S))
bd = ImageDraw.Draw(bg)
for y in range(S):
    t = y / S
    bd.line([(0, y), (S, y)], fill=(int(24 - 10 * t), int(27 - 11 * t), int(44 - 18 * t), 255))
mask = Image.new("L", (S, S), 0)
ImageDraw.Draw(mask).rounded_rectangle(body, radius, fill=255)
img.paste(bg, (0, 0), mask)

# Treemap tiles (fractions of the inner area).
inner = int(S * 170 / 1024)
x0, y0 = inner, inner
W = H = S - 2 * inner
gap = int(S * 16 / 1024)
tiles = [
    # x, y, w, h (0..1), hue, sat, light
    (0.00, 0.00, 0.56, 0.62, 349, 82, 62),
    (0.56, 0.00, 0.44, 0.36, 42, 92, 58),
    (0.56, 0.36, 0.24, 0.26, 204, 85, 60),
    (0.80, 0.36, 0.20, 0.26, 276, 70, 66),
    (0.00, 0.62, 0.34, 0.38, 152, 58, 52),
    (0.34, 0.62, 0.30, 0.38, 234, 72, 68),
    (0.64, 0.62, 0.36, 0.20, 18, 88, 60),
    (0.64, 0.82, 0.18, 0.18, 84, 52, 52),
    (0.82, 0.82, 0.18, 0.18, 204, 60, 72),
]
tr = int(S * 34 / 1024)
for fx, fy, fw, fh, h, s, l in tiles:
    bx = (int(x0 + fx * W + gap / 2), int(y0 + fy * H + gap / 2),
          int(x0 + (fx + fw) * W - gap / 2), int(y0 + (fy + fh) * H - gap / 2))
    w, hh = bx[2] - bx[0], bx[3] - bx[1]
    tile = Image.new("RGBA", (w, hh))
    td = ImageDraw.Draw(tile)
    # Diagonal "cushion" gradient: light top-left to deep bottom-right.
    for i in range(w + hh):
        t = i / (w + hh)
        td.line([(i, 0), (0, i)], fill=hsl(h, s, l + 14 * (1 - t) - 12 * t))
    tm = Image.new("L", (w, hh), 0)
    ImageDraw.Draw(tm).rounded_rectangle((0, 0, w - 1, hh - 1), tr, fill=255)
    # Inner highlight along the top edge.
    hl = Image.new("RGBA", (w, hh), (0, 0, 0, 0))
    ImageDraw.Draw(hl).rounded_rectangle((0, 0, w - 1, hh - 1), tr, outline=(255, 255, 255, 70), width=int(S * 3 / 1024))
    tile.alpha_composite(hl)
    img.paste(tile, bx[:2], tm)

# Subtle glass sheen over the body.
sheen = Image.new("RGBA", (S, S), (0, 0, 0, 0))
sd = ImageDraw.Draw(sheen)
for y in range(body[1], S // 2):
    a = int(26 * (1 - (y - body[1]) / (S // 2 - body[1])))
    sd.line([(0, y), (S, y)], fill=(255, 255, 255, a))
sm = Image.new("L", (S, S), 0)
ImageDraw.Draw(sm).rounded_rectangle(body, radius, fill=255)
img.paste(Image.alpha_composite(img, sheen), (0, 0), sm)
ImageDraw.Draw(img).rounded_rectangle(body, radius, outline=(255, 255, 255, 40), width=int(S * 3 / 1024))

img.resize((1024, 1024), Image.LANCZOS).save("design/icon.png")
print("ok")
