#!/usr/bin/env python3
"""Screen assets for docs/ui/home.html: build/font.bin and build/logo.bin
(made by tools/build.sh) as build/ui-assets.js, which the page loads with
a <script> tag (works from file://). The font stays out of the repo.

    docs/ui/assets.py
"""
import base64, pathlib

build = pathlib.Path(__file__).resolve().parents[2] / "build"
out = build / "ui-assets.js"
parts = []
for name, var in (("font.bin", "NXK_FONT"), ("logo.bin", "NXK_LOGO")):
    data = (build / name).read_bytes()
    parts.append(f'window.{var} = "{base64.b64encode(data).decode()}";\n')
out.write_text("".join(parts))
print(f"{out} ({out.stat().st_size} bytes)")
