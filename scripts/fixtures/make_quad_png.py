"""usage: mkquad.py out.png W H -- four flat quadrants (even-aligned so 4:2:0 chroma is exact)."""
import struct
import sys
import zlib

W, H = int(sys.argv[2]), int(sys.argv[3])
COL = {
    (0, 0): (255, 0, 0),    # top-left red
    (1, 0): (0, 255, 0),    # top-right green
    (0, 1): (0, 0, 255),    # bottom-left blue
    (1, 1): (255, 255, 0),  # bottom-right yellow
}


def chunk(t, d):
    c = struct.pack(">I", len(d)) + t + d
    return c + struct.pack(">I", zlib.crc32(t + d) & 0xFFFFFFFF)


raw = b"".join(
    b"\0" + bytes(c for x in range(W) for c in COL[(x * 2 // W, y * 2 // H)]) for y in range(H)
)
png = (
    b"\x89PNG\r\n\x1a\n"
    + chunk(b"IHDR", struct.pack(">IIBBBBB", W, H, 8, 2, 0, 0, 0))
    + chunk(b"IDAT", zlib.compress(raw))
    + chunk(b"IEND", b"")
)
open(sys.argv[1], "wb").write(png)
