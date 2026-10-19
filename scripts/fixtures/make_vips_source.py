import zlib, struct, sys

W, H = 12, 8


def px(x, y):
    if (x, y) == (0, 0):
        return (255, 0, 0)
    if (x, y) == (W - 1, H - 1):
        return (0, 0, 255)
    if (x, y) == (5, 3):
        return (10, 200, 30)
    return (x * 20, y * 30, 128)


def chunk(t, d):
    c = struct.pack(">I", len(d)) + t + d
    return c + struct.pack(">I", zlib.crc32(t + d) & 0xFFFFFFFF)


raw = b"".join(b"\0" + bytes(c for x in range(W) for c in px(x, y)) for y in range(H))
png = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", W, H, 8, 2, 0, 0, 0)) + chunk(b"IDAT", zlib.compress(raw)) + chunk(b"IEND", b"")
open(sys.argv[1], "wb").write(png)
