"""Generate the README progress illustration; requires Pillow."""

from pathlib import Path
from PIL import Image, ImageDraw, ImageFont

PROGRESSO = 80  # Editorial estimate, not a measured completion percentage.
ROOT = Path(__file__).resolve().parents[1]


def font(size):
    for candidate in ("C:/Windows/Fonts/segoeuib.ttf", "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf"):
        if Path(candidate).is_file():
            return ImageFont.truetype(candidate, size)
    return ImageFont.load_default()


frames = []
for step in range(24):
    im = Image.new("RGB", (800, 112), "#10101a")
    draw = ImageDraw.Draw(im)
    draw.text((24, 14), "PROGRESSO DO PROJETO", font=font(21), fill="#f6eafa")
    draw.text((690, 12), f"~{PROGRESSO}%", font=font(27), fill="#f586c6")
    draw.rounded_rectangle((24, 61, 776, 84), radius=11, fill="#303044")
    end = 24 + round(752 * PROGRESSO / 100)
    draw.rounded_rectangle((24, 61, end, 84), radius=11, fill="#e94eaa")
    x = 35 + step * 24
    if x < end - 20:
        draw.rounded_rectangle((x, 64, min(x + 58, end - 3), 68), radius=2, fill="#ffb9df")
    frames.append(im)
frames[0].save(ROOT / "docs" / "progresso.gif", save_all=True, append_images=frames[1:], duration=100, loop=0, optimize=True)
