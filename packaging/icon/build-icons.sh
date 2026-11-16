#!/bin/bash
# 由 draw-icon.swift 生成各平台图标：SuperControl.icns（macOS）、SuperControl.ico（Windows）、icon-256.png（窗口与界面里的标志）。
# 生成结果已提交到仓库，修改图标设计后重新运行本脚本即可。
set -euo pipefail
cd "$(dirname "$0")"
TMP=$(mktemp -d)
swift draw-icon.swift "$TMP/mac.png"
swift draw-icon.swift "$TMP/flat.png" --flat

SET="$TMP/SuperControl.iconset"
mkdir -p "$SET"
for s in 16 32 128 256 512; do
    sips -z $s $s "$TMP/mac.png" --out "$SET/icon_${s}x${s}.png" >/dev/null
    sips -z $((s * 2)) $((s * 2)) "$TMP/mac.png" --out "$SET/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns "$SET" -o SuperControl.icns

for s in 16 24 32 48 64 128 256; do
    sips -z $s $s "$TMP/flat.png" --out "$TMP/ico-$s.png" >/dev/null
done
cp "$TMP/ico-256.png" icon-256.png
# ICO 文件：头部 + 目录项 + 直接内嵌的 PNG 数据
python3 - "$TMP" <<'PY'
import struct, sys
tmp = sys.argv[1]
sizes = [16, 24, 32, 48, 64, 128, 256]
blobs = [open(f"{tmp}/ico-{s}.png", "rb").read() for s in sizes]
out = struct.pack("<HHH", 0, 1, len(sizes))
offset = 6 + 16 * len(sizes)
for s, b in zip(sizes, blobs):
    out += struct.pack("<BBBBHHII", s % 256, s % 256, 0, 0, 1, 32, len(b), offset)
    offset += len(b)
open("SuperControl.ico", "wb").write(out + b"".join(blobs))
PY
rm -rf "$TMP"
echo "已生成 SuperControl.icns、SuperControl.ico、icon-256.png"
