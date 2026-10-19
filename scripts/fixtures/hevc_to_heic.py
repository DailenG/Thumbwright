"""Wrap a single-frame HEVC Annex B stream (ffmpeg -f hevc) in a minimal HEIF still image.

Layout: ftyp(heic) + meta{hdlr, pitm, iloc, iinf/infe(hvc1), iprp{ipco{hvcC, ispe}, ipma}}
+ mdat with the frame as length-prefixed NAL units; the parameter sets go in hvcC only.

usage: hevc2heic.py in.hevc out.heic width height
"""
import struct
import sys


def box(kind, payload):
    return struct.pack(">I", 8 + len(payload)) + kind + payload


def full(kind, version, flags, payload):
    return box(kind, bytes([version]) + flags.to_bytes(3, "big") + payload)


def nal_units(data):
    out, i, starts = [], 0, []
    while True:
        j = data.find(b"\x00\x00\x01", i)
        if j < 0:
            break
        starts.append(j + 3)
        i = j + 3
    for n, s in enumerate(starts):
        e = starts[n + 1] - 3 if n + 1 < len(starts) else len(data)
        nal = data[s:e]
        while nal.endswith(b"\x00") and n + 1 < len(starts):
            nal = nal[:-1]
        out.append(nal)
    return out


src, dst, width, height = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4])
nals = nal_units(open(src, "rb").read())
by_type = {}
for n in nals:
    by_type.setdefault((n[0] >> 1) & 0x3F, []).append(n)
vps, sps, pps = by_type[32][0], by_type[33][0], by_type[34][0]
frame = [n for n in nals if ((n[0] >> 1) & 0x3F) < 32]
assert frame, "no slice NAL in input"

# profile_tier_level sits right after the 2-byte NAL header and 1 byte of VPS id/layers.
def unescape(nal):
    out, zeros = bytearray(), 0
    for b in nal:
        if zeros >= 2 and b == 3:
            zeros = 0
            continue
        out.append(b)
        zeros = zeros + 1 if b == 0 else 0
    return bytes(out)


ptl = unescape(sps)[3:3 + 12]
profile_byte = ptl[0]
compat = ptl[1:5]
constraint = ptl[5:11]
level = ptl[11]


def hvcc():
    arrays = b""
    for typ, units in ((32, [vps]), (33, [sps]), (34, [pps])):
        arrays += bytes([0x80 | typ]) + struct.pack(">H", len(units))
        for u in units:
            arrays += struct.pack(">H", len(u)) + u
    return box(
        b"hvcC",
        bytes([1, profile_byte]) + compat + constraint + bytes([level])
        + struct.pack(">H", 0xF000)  # min_spatial_segmentation_idc
        + bytes([0xFC])              # parallelismType
        + bytes([0xFD])              # chroma_format_idc 1 (4:2:0)
        + bytes([0xF8])              # bit_depth_luma_minus8 = 0
        + bytes([0xF8])              # bit_depth_chroma_minus8 = 0
        + struct.pack(">H", 0)       # avgFrameRate
        + bytes([0x03])              # constantFrameRate 0, numTemporalLayers 0, nested 0, lengthSizeMinusOne 3
        + bytes([3])                 # numOfArrays
        + arrays,
    )


payload = b"".join(struct.pack(">I", len(n)) + n for n in frame)

ftyp = box(b"ftyp", b"heic" + struct.pack(">I", 0) + b"mif1heic")
hdlr = full(b"hdlr", 0, 0, struct.pack(">I", 0) + b"pict" + b"\0" * 12 + b"\0")
pitm = full(b"pitm", 0, 0, struct.pack(">H", 1))
infe = full(b"infe", 2, 0, struct.pack(">HH", 1, 0) + b"hvc1" + b"\0")
iinf = full(b"iinf", 0, 0, struct.pack(">H", 1) + infe)
ispe = full(b"ispe", 0, 0, struct.pack(">II", width, height))
ipco = box(b"ipco", hvcc() + ispe)
# ipma: entry_count=1, item_ID=1, association_count=2, essential|index1 (hvcC), index2 (ispe)
ipma = full(b"ipma", 0, 0, struct.pack(">I", 1) + struct.pack(">H", 1) + bytes([2, 0x81, 0x02]))
iprp = box(b"iprp", ipco + ipma)


def iloc(offset):
    body = bytes([0x44, 0x00])  # offset_size 4, length_size 4, base_offset_size 0, index_size 0
    body += struct.pack(">H", 1)
    body += struct.pack(">HHH", 1, 0, 1)  # item_ID, data_reference_index, extent_count
    body += struct.pack(">II", offset, len(payload))
    return full(b"iloc", 0, 0, body)


meta_len = len(full(b"meta", 0, 0, hdlr + pitm + iloc(0) + iinf + iprp))
mdat_at = len(ftyp) + meta_len + 8
meta = full(b"meta", 0, 0, hdlr + pitm + iloc(mdat_at) + iinf + iprp)
open(dst, "wb").write(ftyp + meta + box(b"mdat", payload))
print("wrote", dst, len(ftyp + meta) + 8 + len(payload), "bytes; hvcC ptl", ptl.hex())
