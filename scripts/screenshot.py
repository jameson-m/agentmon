#!/usr/bin/env python3
"""Regenerates docs/screenshot.png from `agentmon --demo`.

Runs the release build in tmux at two sizes, turns the captured ANSI into HTML
and has headless Chrome photograph it. Needs tmux and Chrome.
"""
import html
import os
import re
import subprocess
import sys
import tempfile
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BINARY = os.path.join(ROOT, "target", "release", "agentmon")
CHROME = os.environ.get(
    "CHROME", "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
)
PANES = [(58, 34), (128, 34)]  # columns, rows
FG, BG = "#d8d8d8", "#16181d"
CELL = (8.5, 16)  # px
GAP, PAD = 28, 24
ANSI = ["#000", "#c33", "#3a3", "#aa3", "#36c", "#a3a", "#3aa", "#ccc"]
BRIGHT = ["#666", "#f55", "#5f5", "#ff5", "#59f", "#f5f", "#5ff", "#fff"]
CUBE = [0, 95, 135, 175, 215, 255]


def indexed(n):
    if n < 16:
        return (ANSI + BRIGHT)[n]
    if n >= 232:
        grey = 8 + (n - 232) * 10
        return f"rgb({grey},{grey},{grey})"
    n -= 16
    return f"rgb({CUBE[n // 36]},{CUBE[n // 6 % 6]},{CUBE[n % 6]})"


def cell(char, style):
    code = ord(char)
    if 0x2800 <= code <= 0x28FF:
        # Braille as evenly spaced dots, so graphs read as one surface.
        dots = [(0, 0, 1), (0, 1, 2), (0, 2, 4), (1, 0, 8), (1, 1, 16), (1, 2, 32), (0, 3, 64), (1, 3, 128)]
        shadow = ",".join(
            f"{2.1 + col * 4.25:.2f}px {2 + row * 4}px 0 0 currentColor"
            for col, row, bit in dots
            if (code - 0x2800) & bit
        )
        return f'<i style="{style}"><b style="box-shadow:{shadow}"></b></i>'
    return f'<i style="{style}">{html.escape(char)}</i>'


def to_html(text):
    out, fg, bg, bold, dim, reverse = [], None, None, False, False, False
    for part in re.split(r"(\x1b\[[0-9;]*m)", text):
        if part.startswith("\x1b["):
            codes = [int(c or 0) for c in part[2:-1].split(";")]
            i = 0
            while i < len(codes):
                code = codes[i]
                if code == 0:
                    fg, bg, bold, dim, reverse = None, None, False, False, False
                elif code == 1:
                    bold = True
                elif code == 2:
                    dim = True
                elif code == 22:
                    bold = dim = False
                elif code in (7, 27):
                    reverse = code == 7
                elif code in (38, 48):
                    if codes[i + 1] == 2:
                        colour = f"rgb({codes[i + 2]},{codes[i + 3]},{codes[i + 4]})"
                        i += 4
                    else:
                        colour = indexed(codes[i + 2])
                        i += 2
                    if code == 38:
                        fg = colour
                    else:
                        bg = colour
                elif code == 39:
                    fg = None
                elif code == 49:
                    bg = None
                i += 1
        elif part:
            front, back = fg or FG, bg or BG
            if reverse:
                front, back = back, front
            style = f"color:{front};background:{back};"
            style += "font-weight:bold;" if bold else ""
            style += "opacity:.55;" if dim else ""
            out.append("".join("\n" if c == "\n" else cell(c, style) for c in part))
    return "<pre>" + "".join(out) + "</pre>"


def capture(columns, rows):
    tmux = ["tmux", "-L", "agentmon-shot"]
    subprocess.run(tmux + ["kill-server"], stderr=subprocess.DEVNULL)
    size = ["-x", str(columns), "-y", str(rows)]
    env = ["-e", "COLORTERM=truecolor"]
    subprocess.run(tmux + ["new-session", "-d"] + size + env + [BINARY, "--demo"], check=True)
    time.sleep(3)
    shot = subprocess.run(
        tmux + ["capture-pane", "-e", "-p"], check=True, capture_output=True, text=True
    )
    subprocess.run(tmux + ["kill-server"])
    return shot.stdout


def main():
    subprocess.run(["cargo", "build", "--release"], cwd=ROOT, check=True)
    panes = "".join(to_html(capture(*pane)) for pane in PANES)
    width, height = CELL
    style = (
        f"html,body{{margin:0;background:{BG}}}"
        f".r{{display:inline-flex;gap:{GAP}px;padding:{PAD}px;align-items:flex-start}}"
        f"pre{{margin:0;font:14px/{height}px Menlo,monospace}}"
        f"i{{position:relative;font-style:normal;display:inline-block;width:{width}px;"
        f"height:{height}px;overflow:visible;white-space:pre;vertical-align:top}}"
        "b{position:absolute;left:-1px;top:-1px;width:2px;height:2px}"
    )
    page = f"<meta charset=utf-8><style>{style}</style><div class=r>{panes}</div>"
    columns = sum(pane[0] for pane in PANES)
    size = (
        int(columns * width) + GAP * (len(PANES) - 1) + PAD * 2 + 4,
        max(pane[1] for pane in PANES) * height + PAD * 2,
    )
    out = os.path.join(ROOT, "docs", "screenshot.png")
    with tempfile.NamedTemporaryFile("w", suffix=".html", delete=False) as file:
        file.write(page)
    try:
        subprocess.run(
            [CHROME, "--headless", "--disable-gpu", "--hide-scrollbars",
             "--force-device-scale-factor=2", f"--window-size={size[0]},{size[1]}",
             f"--screenshot={out}", f"file://{file.name}"],
            check=True, stderr=subprocess.DEVNULL,
        )
    finally:
        os.unlink(file.name)
    print(out)


if __name__ == "__main__":
    sys.exit(main())
