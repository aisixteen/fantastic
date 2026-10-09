"""A SEEDED glyph/colour sweep for one terminal round (E66): hide the cursor,
clear, then a screen of mixed scripts in random 256-colour and true colour with
every SGR style — enough distinct (glyph, colour, style) keys to grow the atlas,
merge its pages and evict. NEVER pure-red-ish colours: placeholder red
(255,0,0) is the corruption the probe looks for."""
import random
import sys

seed, rnd = int(sys.argv[1]), int(sys.argv[2])
R = random.Random(seed * 1000 + rnd)
SETS = [
    [chr(c) for c in range(0x21, 0x7F)],                      # ASCII
    [chr(c) for c in range(0x2500, 0x2580)],                  # box drawing
    [chr(c) for c in range(0x2580, 0x25A0)],                  # blocks
    [chr(c) for c in range(0x2800, 0x2900)],                  # braille
    [chr(c) for c in range(0x4E00, 0x4E00 + 3000)],           # CJK (wide)
    [chr(c) for c in range(0xAC00, 0xAC00 + 2000)],           # Hangul (wide)
    [chr(c) for c in range(0x1F300, 0x1F650)],                # emoji (wide)
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
