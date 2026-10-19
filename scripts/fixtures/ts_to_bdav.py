"""Turn a 188-byte MPEG transport stream into BDAV (Blu-ray .m2ts) 192-byte source packets.

Each source packet is a 4-byte TP_extra_header (2 bits copy_permission_indicator = 0,
30 bits arrival_time_stamp on a 27 MHz clock) followed by the unchanged 188-byte TS packet.

The arrival time stamps follow the stream's own clock: each packet gets the PCR value
interpolated linearly (by packet index) between the PCRs on either side of it, extrapolated
at the ends, the way a BDAV muxer records when each packet arrived. A constant-rate stamp
that disagrees with the PCRs makes players disagree about the file's timeline.

usage: ts_to_bdav.py in.ts out.m2ts
"""
import struct
import sys

TS = 188
MASK30 = 0x3FFFFFFF

data = open(sys.argv[1], "rb").read()
assert len(data) % TS == 0, "input is not a whole number of TS packets"
count = len(data) // TS

# (packet index, PCR in 27 MHz units) for every packet that carries one.
pcrs = []
for n in range(count):
    pkt = data[n * TS : (n + 1) * TS]
    assert pkt[0] == 0x47, f"lost sync at packet {n}"
    has_adaptation = pkt[3] & 0x20
    if has_adaptation and pkt[4] >= 7 and pkt[5] & 0x10:
        b = pkt[6:12]
        base = (b[0] << 25) | (b[1] << 17) | (b[2] << 9) | (b[3] << 1) | (b[4] >> 7)
        ext = ((b[4] & 1) << 8) | b[5]
        pcrs.append((n, base * 300 + ext))
assert len(pcrs) >= 2, "need at least two PCRs to derive arrival times"


def arrival(n):
    # Segment containing n (the first or last one when n lies outside the PCR range).
    seg = 0
    while seg + 2 < len(pcrs) and pcrs[seg + 1][0] <= n:
        seg += 1
    (n0, t0), (n1, t1) = pcrs[seg], pcrs[seg + 1]
    return t0 + (n - n0) * (t1 - t0) // (n1 - n0)


out = bytearray()
for n in range(count):
    out += struct.pack(">I", arrival(n) & MASK30) + data[n * TS : (n + 1) * TS]
open(sys.argv[2], "wb").write(out)
print(count, "packets,", len(pcrs), "PCRs,", len(out), "bytes")
