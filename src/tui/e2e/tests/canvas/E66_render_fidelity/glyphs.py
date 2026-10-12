"""A SEEDED glyph/colour sweep for one terminal round (E66): hide the cursor,
clear, then a screen of mixed scripts in random 256-colour and true colour with
every SGR style — enough distinct (glyph, colour, style) keys to grow the atlas,
merge its pages and evict. NEVER pure-red-ish colours or emoji: placeholder red
(255,0,0) is the corruption the probe looks for."""
import random
import sys

seed, rnd = int(sys.argv[1]), int(sys.argv[2])
R = random.Random(seed * 1000 + rnd)
# the emoji whose own pixels come near pure red (r > 200, g and b < 50), MEASURED
# in Chrome's 2D canvas (the atlas's rasterizer) at 11-48 px, headless and on the
# GPU — kept out like the red colours (the margin: the atlas's own sizes vary)
RED_EMOJI = {int(h, 16) for h in (
    "1f301 1f306 1f307 1f308 1f321 1f336 1f339 1f33a 1f341 1f344 1f345 1f349 1f34e 1f352 1f353 1f354 "
    "1f355 1f35b 1f35d 1f35f 1f363 1f364 1f36b 1f370 1f371 1f377 1f379 1f37f 1f381 1f385 1f388 1f38b "
    "1f38c 1f38e 1f38f 1f392 1f396 1f3a0 1f3a1 1f3a2 1f3aa 1f3ad 1f3af 1f3b0 1f3b2 1f3b3 1f3b4 1f3b8 "
    "1f3c5 1f3ce 1f3cf 1f3d2 1f3d3 1f3d5 1f3d6 1f3df 1f3e3 1f3e5 1f3ec 1f3ed 1f3f0 1f3f5 1f3f8 1f413 "
    "1f414 1f41e 1f438 1f460 1f46b 1f46c 1f46d 1f472 1f479 1f47a 1f482 1f483 1f484 1f488 1f48a 1f48b "
    "1f48c 1f494 1f4a2 1f4a5 1f4ae 1f4af 1f4c8 1f4cc 1f4cd 1f4d5 1f4da 1f4db 1f4df 1f4e4 1f4e5 1f4e9 "
    "1f4ea 1f4eb 1f4ec 1f4ed 1f4ee 1f4ef 1f4f5 1f507 1f515 1f516 1f51e 1f525 1f534 1f53a 1f53b 1f579 "
    "1f58d 1f5c2 1f5fc 1f60d 1f618 1f63b").split()}
SETS = [
    [chr(c) for c in range(0x21, 0x7F)],                      # ASCII
    [chr(c) for c in range(0x2500, 0x2580)],                  # box drawing
    [chr(c) for c in range(0x2580, 0x25A0)],                  # blocks
    [chr(c) for c in range(0x2800, 0x2900)],                  # braille
    [chr(c) for c in range(0x4E00, 0x4E00 + 3000)],           # CJK (wide)
    [chr(c) for c in range(0xAC00, 0xAC00 + 2000)],           # Hangul (wide)
    [chr(c) for c in range(0x1F300, 0x1F650) if c not in RED_EMOJI],  # emoji (wide)
    [b + chr(m) for b in "aeiouAEIOU" for m in (0x301, 0x308, 0x303, 0x327)],  # combining
    [chr(c) for c in range(0x2190, 0x2200)],                  # arrows, maths
]
STYLES = ["1", "2", "3", "4", "7", "9", "1;3", "4;9"]


def colour() -> str:
    while True:
        if R.random() < 0.5:
            n = R.randrange(256)
            if n in (1, 9, 160, 196):  # the reds
                continue
            return f"5;{n}"
        r, g, b = R.randrange(256), R.randrange(256), R.randrange(256)
        if r > 180 and g < 70 and b < 70:
            continue
        return f"2;{r};{g};{b}"


out = ["\x1b[?25l\x1b[2J\x1b[H"]
for _ in range(14):
    line = []
    width = 0
    while width < 40:
        s = R.choice(SETS)
        g = R.choice(s)
        w = 2 if s in (SETS[4], SETS[5], SETS[6]) else 1
        line.append(f"\x1b[0;{R.choice(STYLES)};38;{colour()}" + (f";48;{colour()}" if R.random() < 0.3 else "") + f"m{g}")
        width += w
    out.append("".join(line) + "\x1b[0m\r\n")
sys.stdout.write("".join(out))
