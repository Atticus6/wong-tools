#!/usr/bin/env python3
"""应用图标重生成：纯标准库，无第三方依赖。

主图（1024 紫底圆角 + 黑 W）在本文件里用解析式重画，W 比老版大约 1.3 倍，
Dock 里不再小一圈；然后派生 hicolor / .iconset / .icns / .ico 全套落盘提交。
macOS 的 iconutil 完全不用（icns 元素直接按 PNG 打包，10.7+ 都认），
Windows / Linux 跑同样出全套，CI 只管拷贝（见 AGENTS）。
"""

import os
import struct
import sys
import zlib

HERE = os.path.dirname(os.path.abspath(__file__))
SIZE = 1024
# 紫底（老图取色）、W 墨色（老图取色）、圆角半径。
BG = (150, 137, 247, 255)
INK = (11, 11, 13, 255)
RADIUS = 228
# W 五点（老图五点绕中心 (512,520) 放 1.28 倍），描边宽。
W_PTS = [(196.0, 200.0), (330.0, 840.0), (512.0, 488.0), (694.0, 840.0), (828.0, 200.0)]
W_WIDTH = 155.0
# 成品只占画布 80% 居中：满幅 tile 在 Dock 里比邻居大一圈，
# 留透明边和其他图标视觉一致（Terminal 同款做法）。
ART_SCALE = 0.8


def sd_round_box(x, y):
    """圆角矩形 tile 的符号距离（tile 铺满全幅，角落透明）。"""
    cx, cy = SIZE / 2.0, SIZE / 2.0
    qx, qy = abs(x - cx) - (SIZE / 2.0 - RADIUS), abs(y - cy) - (SIZE / 2.0 - RADIUS)
    ax, ay = max(qx, 0.0), max(qy, 0.0)
    return (ax * ax + ay * ay) ** 0.5 + min(max(qx, qy), 0.0) - RADIUS


def dist_to_polyline(x, y):
    """点到 W 折线的距离（分段 min 即天然圆角连接，端点钳制即圆头）。"""
    best = float("inf")
    for (ax, ay), (bx, by) in zip(W_PTS, W_PTS[1:]):
        dx, dy = bx - ax, by - ay
        l2 = dx * dx + dy * dy
        t = 0.0 if l2 < 1e-9 else max(0.0, min(1.0, ((x - ax) * dx + (y - ay) * dy) / l2))
        ddx, ddy = x - (ax + dx * t), y - (ay + dy * t)
        d = (ddx * ddx + ddy * ddy) ** 0.5
        if d < best:
            best = d
    return best


def render_master():
    """1024 主图 RGBA（行主序字节串）。整幅逐像素解析式，1~2 分钟。"""
    hw = W_WIDTH / 2.0
    out = bytearray(SIZE * SIZE * 4)
    for py in range(SIZE):
        base = py * SIZE * 4
        for px in range(SIZE):
            o = base + px * 4
            # 底：圆角内外 1px 羽化。
            d_box = sd_round_box(px + 0.5, py + 0.5)
            if d_box >= 0.5:
                continue
            bg_a = min(1.0, max(0.0, 0.5 - d_box))
            # 墨：线宽内外 1px 羽化后源覆盖（两边都不透明，直接 lerp）。
            d_ink = dist_to_polyline(px + 0.5, py + 0.5)
            ink_a = min(1.0, max(0.0, (hw + 0.5 - d_ink)))
            r = BG[0] + (INK[0] - BG[0]) * ink_a
            g = BG[1] + (INK[1] - BG[1]) * ink_a
            b = BG[2] + (INK[2] - BG[2]) * ink_a
            out[o : o + 4] = bytes((int(r + 0.5), int(g + 0.5), int(b + 0.5), int(bg_a * 255 + 0.5)))
    return bytes(out)


def write_png(path, w, h, rgba):
    """RGBA 行主序 → PNG 文件（filter 0，够图标用）。"""
    raw = bytearray()
    stride = w * 4
    for y in range(h):
        raw.append(0)
        raw += rgba[y * stride : (y + 1) * stride]
    comp = zlib.compress(bytes(raw), 6)

    def chunk(typ, payload):
        c = struct.pack(">I", len(payload)) + typ + payload
        return c + struct.pack(">I", zlib.crc32(typ + payload) & 0xFFFFFFFF)

    ihdr = struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0)
    with open(path, "wb") as f:
        f.write(b"\x89PNG\r\n\x1a\n")
        f.write(chunk(b"IHDR", ihdr))
        f.write(chunk(b"IDAT", comp))
        f.write(chunk(b"IEND", b""))


def read_png(path):
    """读回 (w, h, rgba)，只认非交错真彩（本脚本自己写的可往返）。"""
    with open(path, "rb") as f:
        data = f.read()
    assert data[:8] == b"\x89PNG\r\n\x1a\n", "不是 PNG"
    pos, idat, w, h = 8, b"", 0, 0
    while pos < len(data):
        (ln,) = struct.unpack(">I", data[pos : pos + 4])
        typ = data[pos + 4 : pos + 8]
        payload = data[pos + 8 : pos + 8 + ln]
        if typ == b"IHDR":
            w, h, bd, ct, _, _, inter = struct.unpack(">IIBBBBB", payload)
            assert (bd, ct, inter) == (8, 6, 0), "只认 8 位真彩非交错"
        elif typ == b"IDAT":
            idat += payload
        pos += 12 + ln
    raw = zlib.decompress(idat)
    stride = w * 4 + 1
    rgba = bytearray(w * h * 4)
    for y in range(h):
        f = raw[y * stride]
        assert f == 0, "只认 filter 0"
        rgba[y * w * 4 : (y + 1) * w * 4] = raw[y * stride + 1 : (y + 1) * stride]
    return w, h, bytes(rgba)


def downscale(rgba, src, dst):
    """面积平均缩小（图标链都是 2 的幂整除，走快路；零头尺寸走通用路）。"""
    if src % dst == 0:
        k = src // dst
        out = bytearray(dst * dst * 4)
        for y in range(dst):
            for x in range(dst):
                rs = gs = bs = asum = 0
                for dy in range(k):
                    base = ((y * k + dy) * src + x * k) * 4
                    for dx in range(k):
                        o = base + dx * 4
                        a = rgba[o + 3]
                        rs += rgba[o] * a
                        gs += rgba[o + 1] * a
                        bs += rgba[o + 2] * a
                        asum += a
                o = (y * dst + x) * 4
                if asum == 0:
                    out[o : o + 4] = b"\x00\x00\x00\x00"
                else:
                    out[o : o + 4] = bytes(
                        (rs // asum, gs // asum, bs // asum, min(255, (asum + k * k // 2) // (k * k)))
                    )
        return bytes(out)
    out = bytearray(dst * dst * 4)
    f = src / dst
    for y in range(dst):
        y0, y1 = y * f, (y + 1) * f
        for x in range(dst):
            x0, x1 = x * f, (x + 1) * f
            rs = gs = bs = asum = wsum = 0.0
            for sy in range(int(y0), min(src, int(y1) + 1)):
                wy = min(y1, sy + 1) - max(y0, sy)
                if wy <= 0:
                    continue
                for sx in range(int(x0), min(src, int(x1) + 1)):
                    wx = min(x1, sx + 1) - max(x0, sx)
                    if wx <= 0:
                        continue
                    o = (sy * src + sx) * 4
                    a = rgba[o + 3] / 255.0
                    wgt = wx * wy
                    rs += rgba[o] * a * wgt
                    gs += rgba[o + 1] * a * wgt
                    bs += rgba[o + 2] * a * wgt
                    asum += a * wgt
                    wsum += wgt
            o = (y * dst + x) * 4
            if asum <= 0:
                out[o : o + 4] = b"\x00\x00\x00\x00"
            else:
                out[o : o + 4] = bytes(
                    (
                        min(255, int(rs / asum + 0.5)),
                        min(255, int(gs / asum + 0.5)),
                        min(255, int(bs / asum + 0.5)),
                        min(255, int(asum / wsum * 255 + 0.5)),
                    )
                )
    return bytes(out)


def png_bytes(w, h, rgba):
    """内存 PNG（装 icns/ico 条目用）。"""
    raw = bytearray()
    stride = w * 4
    for y in range(h):
        raw.append(0)
        raw += rgba[y * stride : (y + 1) * stride]
    comp = zlib.compress(bytes(raw), 9)

    def chunk(typ, payload):
        c = struct.pack(">I", len(payload)) + typ + payload
        return c + struct.pack(">I", zlib.crc32(typ + payload) & 0xFFFFFFFF)

    ihdr = struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0)
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"IDAT", comp)
        + chunk(b"IEND", b"")
    )


def write_icns(path, master):
    """icns：PNG 元素直装（ic11/ic12/ic07/ic08/ic09/ic10），10.7+ 通吃。"""
    entries = b""
    for ostype, size in (
        (b"ic11", 32),
        (b"ic12", 64),
        (b"ic07", 128),
        (b"ic08", 256),
        (b"ic09", 512),
        (b"ic10", 1024),
    ):
        small = master if size == SIZE else downscale(master, SIZE, size)
        payload = png_bytes(size, size, small)
        entries += ostype + struct.pack(">I", 8 + len(payload)) + payload
    with open(path, "wb") as f:
        f.write(b"icns" + struct.pack(">I", 8 + len(entries)) + entries)


def write_ico(path, master):
    """ico：PNG 压缩条目（Vista+ 通吃，和老文件的 7 档尺寸对齐）。"""
    sizes = (16, 24, 32, 48, 64, 128, 256)
    blobs = []
    for s in sizes:
        blobs.append(png_bytes(s, s, downscale(master, SIZE, s)))
    header = struct.pack("<HHH", 0, 1, len(sizes))
    offset = 6 + 16 * len(sizes)
    out = bytearray(header)
    for s, blob in zip(sizes, blobs):
        out += struct.pack(
            "<BBBBHHII", s % 256, s % 256, 0, 0, 1, 32, len(blob), offset
        )
        offset += len(blob)
    for blob in blobs:
        out += blob
    with open(path, "wb") as f:
        f.write(out)


def compose_master(full):
    """满幅重画缩到 ART_SCALE 居中，四周透明（Dock 视觉和其他图标一致）。"""
    t = int(SIZE * ART_SCALE + 0.5)
    small = downscale(full, SIZE, t)
    out = bytearray(SIZE * SIZE * 4)
    off = (SIZE - t) // 2
    for y in range(t):
        dst = ((off + y) * SIZE + off) * 4
        src = y * t * 4
        out[dst : dst + t * 4] = small[src : src + t * 4]
    return bytes(out)


def main():
    print("画主图 1024（纯 Python 解析式，约 1~2 分钟）…", flush=True)
    master = compose_master(render_master())
    write_png(os.path.join(HERE, "appicon-1024.png"), SIZE, SIZE, master)
    # hicolor（Linux 任务栏/启动器）。
    for s in (16, 24, 32, 48, 64, 128, 256, 512):
        d = os.path.join(HERE, "hicolor", f"{s}x{s}", "apps")
        os.makedirs(d, exist_ok=True)
        write_png(os.path.join(d, "wong-tools.png"), s, s, downscale(master, SIZE, s))
        print(f"hicolor {s}x{s} ok")
    write_icns(os.path.join(HERE, "wong-tools.icns"), master)
    print("wong-tools.icns ok")
    write_ico(os.path.join(HERE, "wong-tools.ico"), master)
    print("wong-tools.ico ok")
    print("全套落盘，直接提交即可（CI 只拷贝）。")


if __name__ == "__main__":
    sys.exit(main())
