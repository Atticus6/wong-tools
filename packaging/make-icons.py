#!/usr/bin/env python3
"""Wong Tools 应用图标生成器（只用标准库 + macOS 自带 sips/iconutil）。

设计：紫色渐变圆角方块 + 深色几何 W，和标题栏 monogram 同一语言。
颜色全取 themes/wong.json 的 Wong Dark（primary hover/active、primary.foreground），
不发明新色。用法（仓库根）：python3 packaging/make-icons.py

产物（全提交，CI 只拷贝不再生成）：
  assets/icons/appicon-1024.png   主图（含透明边角）
  assets/icons/wong-tools.icns    macOS（iconutil 打包）
  assets/icons/wong-tools.ico     Windows（PNG 压缩表项， struct 手拼）
  assets/icons/hicolor/<s>x<s>/apps/wong-tools.png  Linux（16~512）
"""

import os
import struct
import subprocess
import zlib

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "assets", "icons")

# 主图尺寸；BUFFER = 2 倍超采样，边缘一次成型抗锯齿。
SIZE = 1024
SS = 2

# Wong Dark 主题色（见 themes/wong.json）。
TOP = (0x9A, 0x8D, 0xF8)  # button.primary.hover.background（渐变上）
BOTTOM = (0x7C, 0x6C, 0xF0)  # button.primary.active.background（渐变下）
INK = (0x0B, 0x0B, 0x0D)  # primary.foreground（W 字）
RADIUS = 0.225  # 圆角半径（ tile 边长的比例，macOS 连续圆角的近似）

# W 字形（单位坐标）：四段等宽折线 + 圆头，笔画宽 0.115。
W_PTS = [(0.25, 0.30), (0.375, 0.70), (0.50, 0.45), (0.625, 0.70), (0.75, 0.30)]
W_WIDTH = 0.115


def _seg_dist(px, py, ax, ay, bx, by):
    """点到线段距离的平方（超采样缓冲里按像素算，不开方，够用）"""
    dx, dy = bx - ax, by - ay
    length_sq = dx * dx + dy * dy
    t = ((px - ax) * dx + (py - ay) * dy) / length_sq if length_sq else 0.0
    t = 0.0 if t < 0.0 else (1.0 if t > 1.0 else t)
    cx, cy = ax + t * dx, ay + t * dy
    return (px - cx) ** 2 + (py - cy) ** 2


def render(buf_size):
    """光栅化整块 RGBA 缓冲：圆角渐变底 + W 字（距离场，天然抗锯齿）"""
    n = buf_size
    r = RADIUS * n
    half_w = W_WIDTH * n / 2.0
    pts = [(x * n, y * n) for x, y in W_PTS]
    buf = bytearray(n * n * 4)
    for y in range(n):
        v = y / (n - 1)
        # 垂直渐变（主题 hover 紫 → active 紫）。
        tile = tuple(int(TOP[i] + (BOTTOM[i] - TOP[i]) * v) for i in range(3))
        for x in range(n):
            # 圆角矩形内测（角上按圆心距离，其余按边）。
            dx = min(x, n - 1 - x)
            dy = min(y, n - 1 - y)
            if dx >= r or dy >= r:
                inside = min(dx, dy) >= 0
            else:
                inside = (r - dx) ** 2 + (r - dy) ** 2 <= r * r
            if not inside:
                continue
            # W 距离场：任一段或端点圆帽命中即 ink。
            hit = any(
                _seg_dist(x, y, *pts[i], *pts[i + 1]) <= half_w * half_w
                for i in range(len(pts) - 1)
            )
            color = INK if hit else tile
            off = (y * n + x) * 4
            buf[off:off + 4] = bytes((color[0], color[1], color[2], 255))
    return buf


def write_png(path, width, height, rgba):
    """最小 PNG 编码器（8 位真彩 + alpha，zlib 压缩）"""
    raw = b"".join(
        b"\x00" + rgba[y * width * 4:(y + 1) * width * 4] for y in range(height)
    )

    def chunk(tag, data):
        out = struct.pack(">I", len(data)) + tag + data
        return out + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)

    ihdr = struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0)
    with open(path, "wb") as f:
        f.write(b"\x89PNG\r\n\x1a\n")
        f.write(chunk(b"IHDR", ihdr))
        f.write(chunk(b"IDAT", zlib.compress(raw, 9)))
        f.write(chunk(b"IEND", b""))


def downsample(src, src_n, dst_n):
    """盒式降采样 SS 倍：超采样缓冲压回目标尺寸，边缘平滑。"""
    step = src_n // dst_n
    out = bytearray(dst_n * dst_n * 4)
    for y in range(dst_n):
        for x in range(dst_n):
            acc = [0, 0, 0, 0]
            for dy in range(step):
                for dx in range(step):
                    off = ((y * step + dy) * src_n + x * step + dx) * 4
                    for c in range(4):
                        acc[c] += src[off + c]
            div = step * step
            off = (y * dst_n + x) * 4
            out[off:off + 4] = bytes(a // div for a in acc)
    return out


def run(*args):
    subprocess.run(args, check=True)


def main():
    os.makedirs(OUT, exist_ok=True)
    print("光栅化主图（2048 缓冲超采样）…")
    buf = render(SIZE * SS)
    master = downsample(buf, SIZE * SS, SIZE)
    master_path = os.path.join(OUT, "appicon-1024.png")
    write_png(master_path, SIZE, SIZE, master)

    # 派生尺寸全走 sips（Lanczos，系统原生，CI 不需要它，产物直接提交）。
    sizes = [512, 256, 128, 64, 48, 32, 24, 16]
    derived = {}
    for s in sizes:
        path = os.path.join(OUT, f"appicon-{s}.png")
        run("sips", "-z", str(s), str(s), master_path, "--out", path)
        derived[s] = path

    # .icns：iconset 命名规范（@2x 即双倍像素图），iconutil 打包。
    print("打包 icns…")
    iconset = os.path.join(OUT, "wong.iconset")
    os.makedirs(iconset, exist_ok=True)
    pairs = [
        ("icon_16x16.png", 16), ("icon_16x16@2x.png", 32),
        ("icon_32x32.png", 32), ("icon_32x32@2x.png", 64),
        ("icon_128x128.png", 128), ("icon_128x128@2x.png", 256),
        ("icon_256x256.png", 256), ("icon_256x256@2x.png", 512),
        ("icon_512x512.png", 512), ("icon_512x512@2x.png", 1024),
    ]
    for name, s in pairs:
        src = master_path if s == 1024 else derived[s]
        run("cp", src, os.path.join(iconset, name))
    run("iconutil", "-c", "icns", iconset, "-o", os.path.join(OUT, "wong-tools.icns"))

    # .ico：PNG 压缩表项（Vista 起通用），struct 手拼。
    print("打包 ico…")
    entries = []
    for s in [16, 24, 32, 48, 64, 128, 256]:
        src = derived[s] if s != 24 else None
        if src is None:
            tmp = os.path.join(OUT, "appicon-24.png")
            run("sips", "-z", "24", "24", master_path, "--out", tmp)
            src = tmp
        with open(src, "rb") as f:
            entries.append((s, f.read()))
    header = struct.pack("<HHH", 0, 1, len(entries))
    offset = 6 + 16 * len(entries)
    directory, blobs = b"", b""
    for s, png in entries:
        # 256 在头里记 0（BYTE 溢出，格式规定）。
        directory += struct.pack(
            "<BBBBHHII", s % 256, s % 256, 0, 0, 1, 32, len(png), offset
        )
        offset += len(png)
        blobs += png
    with open(os.path.join(OUT, "wong-tools.ico"), "wb") as f:
        f.write(header + directory + blobs)

    # Linux hicolor：各尺寸落位，deb 打包时按树拷贝。
    print("铺 hicolor…")
    for s in [512, 256, 128, 64, 48, 32, 24, 16]:
        src = master_path if s == 512 else derived[s]
        d = os.path.join(OUT, "hicolor", f"{s}x{s}", "apps")
        os.makedirs(d, exist_ok=True)
        run("cp", src, os.path.join(d, "wong-tools.png"))

    # 中间 PNG 只留主图 + hicolor，其余删掉（iconset 也不提交）。
    for s in sizes:
        os.remove(derived[s])
    if os.path.exists(os.path.join(OUT, "appicon-24.png")):
        os.remove(os.path.join(OUT, "appicon-24.png"))
    import shutil
    shutil.rmtree(iconset)
    print("完成：", sorted(os.listdir(OUT)))


if __name__ == "__main__":
    main()

