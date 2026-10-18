//! Lossless JPEG rotation and flipping (FUNCTIONAL_SPEC.md 6.4): the file is entropy
//! decoded to quantised DCT coefficients, the coefficient blocks are moved and sign-flipped
//! (a transform of a DCT block is a permutation plus sign changes of its coefficients),
//! and the result is entropy coded again. No inverse DCT, requantisation or colour
//! conversion happens, so the decoded pixels of the output are exactly the transformed
//! pixels of the input.
//!
//! Supported input: 8-bit Huffman JPEG, sequential (SOF0/SOF1) or progressive (SOF2),
//! 1 to 4 components, any restart interval. Arithmetic coding, 12-bit precision and the
//! lossless/hierarchical processes are refused (the caller falls back to re-encoding).
//!
//! Output is always one interleaved sequential scan: a progressive input becomes a
//! sequential file with identical pixels (only the scan order differs). The input's own
//! Huffman tables are reused when it was a single sequential scan whose tables cover every
//! symbol the transformed blocks need (so four right turns reproduce the original scan
//! data byte for byte); otherwise optimal tables are generated.
//!
//! Partial edge MCUs: a transform that mirrors an axis would move the padding of a partial
//! MCU to the near edge, so it is only lossless when that dimension is a multiple of the
//! MCU size. Such a transform is refused instead of trimming pixels; the caller re-encodes.
//!
//! Dropped from the output: data after the end-of-image marker (e.g. MPO secondary
//! images) and the MPF segment, whose offsets would be invalid. The EXIF segment is
//! normalised (see `exif::normalise`).

use std::time::{Duration, Instant};

use tw_core::decode::{check_alloc, check_dimensions};

use crate::exif;

pub type Result<T, E = String> = std::result::Result<T, E>;

/// Wall-clock budget for one transform (hostile input guard, REQ-TMO).
const BUDGET: Duration = Duration::from_secs(60);

const ZIGZAG: [u8; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// One of the eight symmetries of the image rectangle: output axis `i` (0 = x, 1 = y) is
/// input axis `src[i]`, mirrored when `neg[i]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Transform {
    src: [u8; 2],
    neg: [bool; 2],
}

impl Transform {
    pub const IDENTITY: Self = Self::new([0, 1], [false, false]);
    pub const RIGHT: Self = Self::new([1, 0], [true, false]);
    pub const LEFT: Self = Self::new([1, 0], [false, true]);
    pub const ROTATE_180: Self = Self::new([0, 1], [true, true]);
    pub const FLIP_H: Self = Self::new([0, 1], [true, false]);
    pub const FLIP_V: Self = Self::new([0, 1], [false, true]);

    const fn new(src: [u8; 2], neg: [bool; 2]) -> Self {
        Self { src, neg }
    }

    /// The `twctl rotate --by` names.
    pub fn by_name(name: &str) -> Option<Self> {
        Some(match name {
            "right" => Self::RIGHT,
            "left" => Self::LEFT,
            "180" => Self::ROTATE_180,
            "fliph" => Self::FLIP_H,
            "flipv" => Self::FLIP_V,
            _ => return None,
        })
    }

    /// What an EXIF Orientation value (1-8) asks a viewer to do to the stored pixels;
    /// anything else is the identity.
    pub fn from_exif(orientation: u16) -> Self {
        match orientation {
            2 => Self::FLIP_H,
            3 => Self::ROTATE_180,
            4 => Self::FLIP_V,
            5 => Self::new([1, 0], [false, false]),
            6 => Self::RIGHT,
            7 => Self::new([1, 0], [true, true]),
            8 => Self::LEFT,
            _ => Self::IDENTITY,
        }
    }

    /// `self` first, then `next`.
    pub fn then(self, next: Self) -> Self {
        let mut t = Self::IDENTITY;
        for i in 0..2 {
            let j = usize::from(next.src[i]);
            t.src[i] = self.src[j];
            t.neg[i] = self.neg[j] ^ next.neg[i];
        }
        t
    }

    fn swaps_axes(self) -> bool {
        self.src[0] == 1
    }

    /// Where each natural-order coefficient of a block goes, and whether it changes sign.
    fn coefficient_map(self) -> [(usize, bool); 64] {
        let mut map = [(0, false); 64];
        for v in 0..8 {
            for u in 0..8 {
                let f = [u, v];
                let fo = [f[usize::from(self.src[0])], f[usize::from(self.src[1])]];
                let flip = (self.neg[0] && fo[0] % 2 == 1) ^ (self.neg[1] && fo[1] % 2 == 1);
                map[v * 8 + u] = (fo[1] * 8 + fo[0], flip);
            }
        }
        map
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct QTable {
    wide: bool,
    /// Natural (row-major) order.
    v: [u16; 64],
}

#[derive(Clone, Debug)]
struct HuffSpec {
    counts: [u8; 16],
    syms: Vec<u8>,
}

struct Comp {
    id: u8,
    h: usize,
    v: usize,
    tq: usize,
    q: Option<QTable>,
    blocks_w: usize,
    blocks_h: usize,
    /// `blocks_w * blocks_h` blocks of 64 natural-order coefficients.
    coefs: Vec<i16>,
}

struct Frame {
    width: usize,
    height: usize,
    progressive: bool,
    comps: Vec<Comp>,
    hmax: usize,
    vmax: usize,
    mcus_x: usize,
    mcus_y: usize,
}

/// Huffman assignment of the original single sequential scan.
struct OrigTables {
    comp_tables: Vec<(usize, usize)>,
    dc: [Option<HuffSpec>; 4],
    ac: [Option<HuffSpec>; 4],
}

/// Everything of the input except its coefficients that the output needs.
struct Meta {
    /// APPn and COM segments in file order: (marker, payload).
    segments: Vec<(u8, Vec<u8>)>,
    restart_interval: usize,
    orig: Option<OrigTables>,
    /// Bytes of real data after the end-of-image marker (vendor trailers, MPO images).
    trailer: usize,
}

struct Parsed {
    frame: Frame,
    meta: Meta,
}

fn be16(d: &[u8], o: usize) -> Result<usize> {
    d.get(o..o + 2)
        .map(|b| u16::from_be_bytes([b[0], b[1]]) as usize)
        .ok_or_else(|| "truncated JPEG".to_string())
}

fn ceil_div(a: usize, b: usize) -> usize {
    a.div_ceil(b)
}

fn check_time(deadline: Instant) -> Result<()> {
    if Instant::now() > deadline {
        return Err("lossless transform time budget exceeded".into());
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Entropy decoding
// ---------------------------------------------------------------------------------------

/// Canonical Huffman decoding table (JPEG Annex C / F).
struct HuffDec {
    /// 9-bit lookahead: `(length << 8) | symbol`, 0 when the code is longer.
    fast: [u16; 512],
    maxcode: [i32; 17],
    valoff: [i32; 17],
    syms: Vec<u8>,
}

impl HuffDec {
    fn new(spec: &HuffSpec) -> Result<Self> {
        let mut t = Self {
            fast: [0; 512],
            maxcode: [-1; 17],
            valoff: [0; 17],
            syms: spec.syms.clone(),
        };
        let (mut code, mut k) = (0u32, 0usize);
        for len in 1..=16usize {
            let n = u32::from(spec.counts[len - 1]);
            if code + n > 1 << len {
                return Err("bad Huffman table".into());
            }
            t.valoff[len] = k as i32 - code as i32;
            if n > 0 {
                t.maxcode[len] = (code + n - 1) as i32;
            }
            if len <= 9 {
                for i in 0..n {
                    let base = ((code + i) << (9 - len)) as usize;
                    let entry = ((len as u16) << 8) | u16::from(spec.syms[k + i as usize]);
                    t.fast[base..base + (1 << (9 - len))].fill(entry);
                }
            }
            code = (code + n) << 1;
            k += n as usize;
        }
        Ok(t)
    }
}

/// Bit reader over entropy-coded data. At a marker (or the end of the data) it feeds zero
/// bits and remembers that it did, so truncation is detected once the real bits are used up.
struct Bits<'a> {
    d: &'a [u8],
    pos: usize,
    acc: u64,
    n: u32,
    /// Zero bits fed after the real data ended; they are the lowest `fake` bits of `acc`.
    fake: u32,
    hit_marker: bool,
    overrun: bool,
}

impl<'a> Bits<'a> {
    fn new(d: &'a [u8], pos: usize) -> Self {
        Self {
            d,
            pos,
            acc: 0,
            n: 0,
            fake: 0,
            hit_marker: false,
            overrun: false,
        }
    }

    fn fill(&mut self) {
        while self.n <= 56 {
            let b = if self.hit_marker {
                self.fake += 8;
                0
            } else {
                match self.d.get(self.pos) {
                    None => {
                        self.hit_marker = true;
                        self.fake += 8;
                        0
                    }
                    Some(0xFF) => match self.d.get(self.pos + 1) {
                        Some(0) => {
                            self.pos += 2;
                            0xFF
                        }
                        _ => {
                            self.hit_marker = true;
                            self.fake += 8;
                            0
                        }
                    },
                    Some(&b) => {
                        self.pos += 1;
                        b
                    }
                }
            };
            self.acc = (self.acc << 8) | u64::from(b);
            self.n += 8;
        }
    }

    fn consume(&mut self, k: u32) {
        self.n -= k;
        if self.n < self.fake {
            self.overrun = true;
            self.fake = self.n;
        }
    }

    fn get(&mut self, k: u32) -> u32 {
        if k == 0 {
            return 0;
        }
        if self.n < 32 {
            self.fill();
        }
        let v = ((self.acc >> (self.n - k)) & ((1 << k) - 1)) as u32;
        self.consume(k);
        v
    }

    fn bit(&mut self) -> bool {
        self.get(1) == 1
    }

    /// `receive` + `extend` of the spec: an `s`-bit magnitude with its sign.
    fn signed(&mut self, s: u32) -> i32 {
        if s == 0 {
            return 0;
        }
        let v = self.get(s) as i32;
        if v < 1 << (s - 1) {
            v - (1 << s) + 1
        } else {
            v
        }
    }

    fn symbol(&mut self, t: &HuffDec) -> Result<u8> {
        if self.n < 32 {
            self.fill();
        }
        let look = ((self.acc >> (self.n - 16)) & 0xFFFF) as u32;
        let f = t.fast[(look >> 7) as usize];
        if f != 0 {
            self.consume(u32::from(f >> 8));
            return Ok(f as u8);
        }
        for len in 10..=16usize {
            let code = (look >> (16 - len)) as i32;
            if code <= t.maxcode[len] {
                let idx = usize::try_from(t.valoff[len] + code)
                    .ok()
                    .filter(|&i| i < t.syms.len())
                    .ok_or("bad Huffman code")?;
                self.consume(len as u32);
                return Ok(t.syms[idx]);
            }
        }
        Err("bad Huffman code".into())
    }

    /// Ends a restart interval: the next bytes must be `RSTn`.
    fn restart(&mut self, index: u8) -> Result<()> {
        if self.overrun {
            return Err("truncated JPEG scan".into());
        }
        let ok = self.hit_marker
            && self.d.get(self.pos) == Some(&0xFF)
            && self.d.get(self.pos + 1) == Some(&(0xD0 + (index & 7)));
        if !ok {
            return Err("missing or misplaced restart marker".into());
        }
        self.pos += 2;
        self.acc = 0;
        self.n = 0;
        self.fake = 0;
        self.hit_marker = false;
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pass {
    Sequential,
    DcFirst,
    DcRefine,
    AcFirst,
    AcRefine,
}

struct Scan<'a> {
    bits: Bits<'a>,
    pass: Pass,
    ss: usize,
    se: usize,
    al: u32,
    preds: [i32; 4],
    eobrun: u32,
    /// Per scan component (DC table, AC table).
    dc: [Option<HuffDec>; 4],
    ac: [Option<HuffDec>; 4],
}

fn narrow(v: i32) -> Result<i16> {
    i16::try_from(v).map_err(|_| "coefficient out of range".to_string())
}

impl Scan<'_> {
    fn dc_symbol(&mut self, si: usize) -> Result<u8> {
        let t = self.dc[si].as_ref().ok_or("missing DC table")?;
        self.bits.symbol(t)
    }

    fn ac_symbol(&mut self, si: usize) -> Result<u8> {
        let t = self.ac[si].as_ref().ok_or("missing AC table")?;
        self.bits.symbol(t)
    }

    fn block(&mut self, b: &mut [i16], si: usize) -> Result<()> {
        match self.pass {
            Pass::Sequential => self.sequential(b, si),
            Pass::DcFirst => {
                let s = u32::from(self.dc_symbol(si)?);
                if s > 15 {
                    return Err("bad DC category".into());
                }
                self.preds[si] += self.bits.signed(s);
                b[0] = narrow(self.preds[si].checked_mul(1 << self.al).ok_or("overflow")?)?;
                Ok(())
            }
            Pass::DcRefine => {
                if self.bits.bit() {
                    b[0] |= 1 << self.al;
                }
                Ok(())
            }
            Pass::AcFirst => self.ac_first(b, si),
            Pass::AcRefine => self.ac_refine(b, si),
        }
    }

    fn sequential(&mut self, b: &mut [i16], si: usize) -> Result<()> {
        let s = u32::from(self.dc_symbol(si)?);
        if s > 15 {
            return Err("bad DC category".into());
        }
        self.preds[si] += self.bits.signed(s);
        b[0] = narrow(self.preds[si])?;
        let mut k = 1;
        while k < 64 {
            let rs = self.ac_symbol(si)?;
            let (r, s) = (usize::from(rs >> 4), u32::from(rs & 15));
            if s == 0 {
                if r == 15 {
                    k += 16;
                    continue;
                }
                break;
            }
            k += r;
            if k > 63 {
                return Err("AC run past the end of a block".into());
            }
            b[usize::from(ZIGZAG[k])] = narrow(self.bits.signed(s))?;
            k += 1;
        }
        Ok(())
    }

    fn ac_first(&mut self, b: &mut [i16], si: usize) -> Result<()> {
        if self.eobrun > 0 {
            self.eobrun -= 1;
            return Ok(());
        }
        let mut k = self.ss;
        while k <= self.se {
            let rs = self.ac_symbol(si)?;
            let (r, s) = (usize::from(rs >> 4), u32::from(rs & 15));
            if s != 0 {
                k += r;
                if k > self.se {
                    return Err("AC run past the end of the band".into());
                }
                b[usize::from(ZIGZAG[k])] = narrow(self.bits.signed(s) << self.al)?;
            } else if r == 15 {
                k += 15;
            } else {
                self.eobrun = 1 << r;
                if r > 0 {
                    self.eobrun += self.bits.get(r as u32);
                }
                self.eobrun -= 1;
                break;
            }
            k += 1;
        }
        Ok(())
    }

    /// Refinement bit for an already non-zero coefficient.
    fn refine(&mut self, c: &mut i16) {
        if self.bits.bit() {
            let p1 = 1i16 << self.al;
            if *c & p1 == 0 {
                *c = if *c >= 0 {
                    c.wrapping_add(p1)
                } else {
                    c.wrapping_sub(p1)
                };
            }
        }
    }

    fn ac_refine(&mut self, b: &mut [i16], si: usize) -> Result<()> {
        let p1 = 1i16 << self.al;
        let mut k = self.ss;
        if self.eobrun == 0 {
            while k <= self.se {
                let rs = self.ac_symbol(si)?;
                let (mut r, s) = (i32::from(rs >> 4), rs & 15);
                let mut new = 0i16;
                if s != 0 {
                    if s != 1 {
                        return Err("bad refinement symbol".into());
                    }
                    new = if self.bits.bit() { p1 } else { -p1 };
                } else if r != 15 {
                    self.eobrun = 1 << r;
                    if r > 0 {
                        self.eobrun += self.bits.get(r as u32);
                    }
                    break;
                }
                // Skip `r` still-zero coefficients, refining the non-zero ones passed.
                while k <= self.se {
                    let pos = usize::from(ZIGZAG[k]);
                    if b[pos] != 0 {
                        let mut c = b[pos];
                        self.refine(&mut c);
                        b[pos] = c;
                    } else {
                        r -= 1;
                        if r < 0 {
                            break;
                        }
                    }
                    k += 1;
                }
                if new != 0 {
                    if k > self.se {
                        return Err("refinement past the end of the band".into());
                    }
                    b[usize::from(ZIGZAG[k])] = new;
                }
                k += 1;
            }
        }
        if self.eobrun > 0 {
            while k <= self.se {
                let pos = usize::from(ZIGZAG[k]);
                if b[pos] != 0 {
                    let mut c = b[pos];
                    self.refine(&mut c);
                    b[pos] = c;
                }
                k += 1;
            }
            self.eobrun -= 1;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------------------

/// Position after the next marker's `FF xx` bytes, and the marker.
fn next_marker(d: &[u8], pos: &mut usize) -> Option<u8> {
    loop {
        while *d.get(*pos)? != 0xFF {
            *pos += 1;
        }
        while *d.get(*pos + 1)? == 0xFF {
            *pos += 1;
        }
        let m = *d.get(*pos + 1)?;
        *pos += 2;
        if m != 0 {
            return Some(m);
        }
    }
}

fn parse_sof(seg: &[u8], progressive: bool) -> Result<Frame> {
    if seg.len() < 6 {
        return Err("truncated SOF".into());
    }
    if seg[0] != 8 {
        return Err(format!("{}-bit JPEG precision is not supported", seg[0]));
    }
    let (height, width) = (be16(seg, 1)?, be16(seg, 3)?);
    let n = usize::from(seg[5]);
    if height == 0 {
        return Err("JPEG height defined by DNL is not supported".into());
    }
    if !(1..=4).contains(&n) || seg.len() < 6 + 3 * n {
        return Err("unsupported component count".into());
    }
    check_dimensions(width as u32, height as u32).map_err(|e| e.to_string())?;
    let mut comps: Vec<Comp> = Vec::new();
    for i in 0..n {
        let c = &seg[6 + 3 * i..9 + 3 * i];
        let (mut h, mut v) = (usize::from(c[1] >> 4), usize::from(c[1] & 15));
        if !(1..=4).contains(&h) || !(1..=4).contains(&v) || c[2] > 3 {
            return Err("bad component parameters".into());
        }
        if comps.iter().any(|o| o.id == c[0]) {
            return Err("duplicate component id".into());
        }
        if n == 1 {
            // A lone component is one block per MCU whatever its declared factors.
            (h, v) = (1, 1);
        }
        comps.push(Comp {
            id: c[0],
            h,
            v,
            tq: usize::from(c[2]),
            q: None,
            blocks_w: 0,
            blocks_h: 0,
            coefs: Vec::new(),
        });
    }
    let hmax = comps.iter().map(|c| c.h).max().unwrap_or(1);
    let vmax = comps.iter().map(|c| c.v).max().unwrap_or(1);
    if comps.iter().any(|c| hmax % c.h != 0 || vmax % c.v != 0) {
        return Err("non-integral sampling ratios are not supported".into());
    }
    let mcus_x = ceil_div(width, 8 * hmax);
    let mcus_y = ceil_div(height, 8 * vmax);
    let mut bytes = 0u64;
    for c in &mut comps {
        c.blocks_w = mcus_x * c.h;
        c.blocks_h = mcus_y * c.v;
        bytes += (c.blocks_w * c.blocks_h * 128) as u64;
    }
    // The decoded coefficients and the transformed copy are alive together.
    check_alloc(bytes.saturating_mul(2)).map_err(|e| e.to_string())?;
    for c in &mut comps {
        c.coefs = vec![0; c.blocks_w * c.blocks_h * 64];
    }
    Ok(Frame {
        width,
        height,
        progressive,
        comps,
        hmax,
        vmax,
        mcus_x,
        mcus_y,
    })
}

fn parse_dqt(seg: &[u8], tables: &mut [Option<QTable>; 4]) -> Result<()> {
    let mut i = 0;
    while i < seg.len() {
        let (pq, tq) = (seg[i] >> 4, usize::from(seg[i] & 15));
        let wide = match pq {
            0 => false,
            1 => true,
            _ => return Err("bad quantisation table precision".into()),
        };
        let size = if wide { 128 } else { 64 };
        let body = seg
            .get(i + 1..i + 1 + size)
            .filter(|_| tq < 4)
            .ok_or("bad DQT")?;
        let mut v = [0u16; 64];
        for (k, &z) in ZIGZAG.iter().enumerate() {
            v[usize::from(z)] = if wide {
                u16::from_be_bytes([body[2 * k], body[2 * k + 1]])
            } else {
                u16::from(body[k])
            };
        }
        tables[tq] = Some(QTable { wide, v });
        i += 1 + size;
    }
    Ok(())
}

fn parse_dht(
    seg: &[u8],
    dc: &mut [Option<HuffSpec>; 4],
    ac: &mut [Option<HuffSpec>; 4],
) -> Result<()> {
    let mut i = 0;
    while i < seg.len() {
        let (tc, th) = (seg[i] >> 4, usize::from(seg[i] & 15));
        let counts: [u8; 16] = seg
            .get(i + 1..i + 17)
            .ok_or("bad DHT")?
            .try_into()
            .map_err(|_| "bad DHT")?;
        let total: usize = counts.iter().map(|&c| usize::from(c)).sum();
        let syms = seg.get(i + 17..i + 17 + total).ok_or("bad DHT")?.to_vec();
        if tc > 1 || th > 3 {
            return Err("bad DHT class or id".into());
        }
        let spec = HuffSpec { counts, syms };
        HuffDec::new(&spec)?;
        if tc == 0 {
            dc[th] = Some(spec)
        } else {
            ac[th] = Some(spec)
        }
        i += 17 + total;
    }
    Ok(())
}

struct ScanHeader {
    comps: Vec<usize>,
    td: Vec<usize>,
    ta: Vec<usize>,
    ss: usize,
    se: usize,
    ah: u8,
    al: u8,
}

fn parse_sos(seg: &[u8], frame: &Frame) -> Result<ScanHeader> {
    let ns = usize::from(*seg.first().ok_or("truncated SOS")?);
    if !(1..=4).contains(&ns) || seg.len() != 4 + 2 * ns {
        return Err("bad SOS".into());
    }
    let mut h = ScanHeader {
        comps: Vec::new(),
        td: Vec::new(),
        ta: Vec::new(),
        ss: usize::from(seg[1 + 2 * ns]),
        se: usize::from(seg[2 + 2 * ns]),
        ah: seg[3 + 2 * ns] >> 4,
        al: seg[3 + 2 * ns] & 15,
    };
    for i in 0..ns {
        let ci = frame
            .comps
            .iter()
            .position(|c| c.id == seg[1 + 2 * i])
            .ok_or("scan names an unknown component")?;
        if h.comps.contains(&ci) {
            return Err("component repeated in scan".into());
        }
        let t = seg[2 + 2 * i];
        if t >> 4 > 3 || t & 15 > 3 {
            return Err("bad table selector".into());
        }
        h.comps.push(ci);
        h.td.push(usize::from(t >> 4));
        h.ta.push(usize::from(t & 15));
    }
    Ok(h)
}

/// Entropy decodes one scan into `frame`; returns the position after the scan data.
fn decode_scan(
    d: &[u8],
    pos: usize,
    h: &ScanHeader,
    frame: &mut Frame,
    restart_interval: usize,
    tables: (&[Option<HuffSpec>; 4], &[Option<HuffSpec>; 4]),
    deadline: Instant,
) -> Result<usize> {
    let pass = if !frame.progressive {
        if (h.ss, h.se, h.ah, h.al) != (0, 63, 0, 0) {
            return Err("unexpected spectral selection in a sequential scan".into());
        }
        Pass::Sequential
    } else if h.ss == 0 {
        if h.se != 0 || h.al > 13 || (h.ah != 0 && h.ah != h.al + 1) {
            return Err("bad progressive DC scan parameters".into());
        }
        if h.ah == 0 {
            Pass::DcFirst
        } else {
            Pass::DcRefine
        }
    } else {
        if h.se < h.ss
            || h.se > 63
            || h.comps.len() != 1
            || h.al > 13
            || (h.ah != 0 && h.ah != h.al + 1)
        {
            return Err("bad progressive AC scan parameters".into());
        }
        if h.ah == 0 {
            Pass::AcFirst
        } else {
            Pass::AcRefine
        }
    };
    let need_dc = matches!(pass, Pass::Sequential | Pass::DcFirst);
    let need_ac = matches!(pass, Pass::Sequential | Pass::AcFirst | Pass::AcRefine);
    let mut scan = Scan {
        bits: Bits::new(d, pos),
        pass,
        ss: h.ss,
        se: h.se,
        al: u32::from(h.al),
        preds: [0; 4],
        eobrun: 0,
        dc: [None, None, None, None],
        ac: [None, None, None, None],
    };
    for si in 0..h.comps.len() {
        if need_dc {
            let spec = tables.0[h.td[si]]
                .as_ref()
                .ok_or("scan uses an undefined DC table")?;
            scan.dc[si] = Some(HuffDec::new(spec)?);
        }
        if need_ac {
            let spec = tables.1[h.ta[si]]
                .as_ref()
                .ok_or("scan uses an undefined AC table")?;
            scan.ac[si] = Some(HuffDec::new(spec)?);
        }
    }
    let interleaved = h.comps.len() > 1;
    let (scan_w, scan_h) = if interleaved {
        (frame.mcus_x, frame.mcus_y)
    } else {
        let c = &frame.comps[h.comps[0]];
        (
            ceil_div(ceil_div(frame.width * c.h, frame.hmax), 8),
            ceil_div(ceil_div(frame.height * c.v, frame.vmax), 8),
        )
    };
    let mut rst = 0u8;
    for m in 0..scan_w * scan_h {
        if m % scan_w == 0 {
            check_time(deadline)?;
        }
        if restart_interval > 0 && m > 0 && m % restart_interval == 0 {
            scan.bits.restart(rst)?;
            rst = rst.wrapping_add(1);
            scan.preds = [0; 4];
            scan.eobrun = 0;
        }
        let (mx, my) = (m % scan_w, m / scan_w);
        for (si, &ci) in h.comps.iter().enumerate() {
            let c = &mut frame.comps[ci];
            let (bw, bh) = if interleaved { (c.h, c.v) } else { (1, 1) };
            for by in 0..bh {
                for bx in 0..bw {
                    let blk = (my * bh + by) * c.blocks_w + mx * bw + bx;
                    scan.block(&mut c.coefs[blk * 64..blk * 64 + 64], si)?;
                }
            }
        }
    }
    if scan.bits.overrun {
        return Err("truncated JPEG scan".into());
    }
    Ok(scan.bits.pos)
}

fn parse(d: &[u8], deadline: Instant) -> Result<Parsed> {
    if !d.starts_with(&[0xFF, 0xD8]) {
        return Err("not a JPEG".into());
    }
    let mut pos = 2;
    let mut qt: [Option<QTable>; 4] = [None, None, None, None];
    let mut dc: [Option<HuffSpec>; 4] = [None, None, None, None];
    let mut ac: [Option<HuffSpec>; 4] = [None, None, None, None];
    let mut restart_interval = 0;
    let mut frame: Option<Frame> = None;
    let mut segments = Vec::new();
    let mut scans = 0usize;
    let mut trailer = 0;
    let mut orig = None;
    let mut covered_dc: Vec<bool> = Vec::new();
    let mut covered: Vec<bool> = Vec::new();
    while let Some(m) = next_marker(d, &mut pos) {
        match m {
            0xD9 => {
                let rest = &d[pos..];
                if rest.iter().any(|&b| b != 0 && !b.is_ascii_whitespace()) {
                    trailer = rest.len();
                }
                break;
            }
            0x01 | 0xD0..=0xD8 => continue,
            _ => {}
        }
        let len = be16(d, pos)?;
        let seg = d
            .get(pos + 2..pos + len)
            .filter(|_| len >= 2)
            .ok_or("truncated JPEG segment")?;
        pos += len;
        match m {
            0xC0..=0xC2 => {
                if frame.is_some() {
                    return Err("multiple frames".into());
                }
                let f = parse_sof(seg, m == 0xC2)?;
                covered = vec![false; f.comps.len()];
                covered_dc = vec![false; f.comps.len()];
                frame = Some(f);
            }
            0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF => {
                return Err("arithmetic, lossless and hierarchical JPEG are not supported".into());
            }
            0xC4 => parse_dht(seg, &mut dc, &mut ac)?,
            0xDB => parse_dqt(seg, &mut qt)?,
            0xDC => return Err("DNL marker is not supported".into()),
            0xDD => restart_interval = be16(seg, 0)?,
            0xDA => {
                let f = frame.as_mut().ok_or("scan before frame header")?;
                let h = parse_sos(seg, f)?;
                for &ci in &h.comps {
                    let c = &mut f.comps[ci];
                    if c.q.is_none() {
                        c.q = Some(
                            qt[c.tq]
                                .clone()
                                .ok_or("scan uses an undefined quantisation table")?,
                        );
                    }
                }
                pos = decode_scan(d, pos, &h, f, restart_interval, (&dc, &ac), deadline)?;
                for &ci in &h.comps {
                    covered[ci] = true;
                    if h.ss == 0 {
                        covered_dc[ci] = true;
                    }
                }
                scans += 1;
                if scans == 1 && !f.progressive && h.comps.len() == f.comps.len() {
                    orig = Some(OrigTables {
                        comp_tables: h.td.iter().copied().zip(h.ta.iter().copied()).collect(),
                        dc: dc.clone(),
                        ac: ac.clone(),
                    });
                }
            }
            0xE0..=0xEF | 0xFE => segments.push((m, seg.to_vec())),
            _ => {}
        }
    }
    let frame = frame.ok_or("no JPEG frame")?;
    let complete = if frame.progressive {
        &covered_dc
    } else {
        &covered
    };
    if scans == 0 || complete.iter().any(|c| !c) {
        return Err("incomplete JPEG scans".into());
    }
    let single_scan = scans == 1 && orig.is_some();
    if !single_scan {
        orig = None;
    }
    Ok(Parsed {
        frame,
        meta: Meta {
            segments,
            restart_interval: if single_scan { restart_interval } else { 0 },
            orig,
            trailer,
        },
    })
}

// ---------------------------------------------------------------------------------------
// Transform
// ---------------------------------------------------------------------------------------

fn transform_frame(f: Frame, xf: Transform) -> Result<Frame> {
    let dim = [f.width, f.height];
    let mcu = [8 * f.hmax, 8 * f.vmax];
    for i in 0..2 {
        let s = usize::from(xf.src[i]);
        if xf.neg[i] && !dim[s].is_multiple_of(mcu[s]) {
            return Err(format!(
                "{}x{} is not a multiple of the {}x{} MCU size, so this rotation or flip cannot be done losslessly",
                f.width, f.height, mcu[0], mcu[1]
            ));
        }
    }
    let swap = xf.swaps_axes();
    let map = xf.coefficient_map();
    let comps = f
        .comps
        .iter()
        .map(|c| {
            let d = [c.blocks_w, c.blocks_h];
            let od = [d[usize::from(xf.src[0])], d[usize::from(xf.src[1])]];
            let mut coefs = vec![0i16; c.coefs.len()];
            for by in 0..c.blocks_h {
                for bx in 0..c.blocks_w {
                    let p = [bx, by];
                    let q: [usize; 2] = std::array::from_fn(|i| {
                        let s = usize::from(xf.src[i]);
                        if xf.neg[i] { d[s] - 1 - p[s] } else { p[s] }
                    });
                    let (src, dst) = ((by * d[0] + bx) * 64, (q[1] * od[0] + q[0]) * 64);
                    for (k, &(to, flip)) in map.iter().enumerate() {
                        let v = c.coefs[src + k];
                        coefs[dst + to] = if flip { v.wrapping_neg() } else { v };
                    }
                }
            }
            let q = c.q.as_ref().map(|q| {
                let mut v = [0u16; 64];
                for (k, &(to, _)) in map.iter().enumerate() {
                    v[to] = q.v[k];
                }
                QTable { wide: q.wide, v }
            });
            Comp {
                id: c.id,
                h: if swap { c.v } else { c.h },
                v: if swap { c.h } else { c.v },
                tq: c.tq,
                q,
                blocks_w: od[0],
                blocks_h: od[1],
                coefs,
            }
        })
        .collect();
    Ok(Frame {
        width: dim[usize::from(xf.src[0])],
        height: dim[usize::from(xf.src[1])],
        progressive: false,
        comps,
        hmax: if swap { f.vmax } else { f.hmax },
        vmax: if swap { f.hmax } else { f.vmax },
        mcus_x: if swap { f.mcus_y } else { f.mcus_x },
        mcus_y: if swap { f.mcus_x } else { f.mcus_y },
    })
}

// ---------------------------------------------------------------------------------------
// Entropy encoding
// ---------------------------------------------------------------------------------------

trait Sink {
    /// Emits a Huffman symbol of `class` (0 = DC, 1 = AC) for component `ci`.
    fn symbol(&mut self, class: usize, ci: usize, sym: u8) -> Result<()>;
    fn bits(&mut self, v: u32, n: u32);
    fn restart(&mut self, index: u8);
}

struct Counter {
    /// `freq[class][component][symbol]`
    freq: [[[u32; 256]; 4]; 2],
}

impl Sink for Counter {
    fn symbol(&mut self, class: usize, ci: usize, sym: u8) -> Result<()> {
        self.freq[class][ci][usize::from(sym)] += 1;
        Ok(())
    }
    fn bits(&mut self, _: u32, _: u32) {}
    fn restart(&mut self, _: u8) {}
}

struct HuffEnc {
    code: [u16; 256],
    len: [u8; 256],
}

impl HuffEnc {
    fn new(spec: &HuffSpec) -> Result<Self> {
        HuffDec::new(spec)?;
        let mut t = Self {
            code: [0; 256],
            len: [0; 256],
        };
        let (mut code, mut k) = (0u32, 0usize);
        for len in 1..=16usize {
            for _ in 0..spec.counts[len - 1] {
                let s = usize::from(spec.syms[k]);
                t.code[s] = code as u16;
                t.len[s] = len as u8;
                code += 1;
                k += 1;
            }
            code <<= 1;
        }
        Ok(t)
    }
}

struct Writer<'a> {
    out: &'a mut Vec<u8>,
    acc: u64,
    nb: u32,
    /// `tables[class][id]`, and each component's (DC id, AC id).
    tables: [[Option<HuffEnc>; 4]; 2],
    comp_tables: &'a [(usize, usize)],
}

impl Writer<'_> {
    fn put(&mut self, v: u32, n: u32) {
        if n == 0 {
            return;
        }
        self.acc = (self.acc << n) | (u64::from(v) & ((1 << n) - 1));
        self.nb += n;
        while self.nb >= 8 {
            let b = (self.acc >> (self.nb - 8)) as u8;
            self.out.push(b);
            if b == 0xFF {
                self.out.push(0);
            }
            self.nb -= 8;
        }
    }

    fn flush(&mut self) {
        if self.nb > 0 {
            let pad = 8 - self.nb;
            self.put((1 << pad) - 1, pad);
        }
    }
}

impl Sink for Writer<'_> {
    fn symbol(&mut self, class: usize, ci: usize, sym: u8) -> Result<()> {
        let id = if class == 0 {
            self.comp_tables[ci].0
        } else {
            self.comp_tables[ci].1
        };
        let t = self.tables[class][id]
            .as_ref()
            .ok_or("missing Huffman table")?;
        let (code, len) = (t.code[usize::from(sym)], t.len[usize::from(sym)]);
        if len == 0 {
            return Err("symbol missing from Huffman table".into());
        }
        self.put(u32::from(code), u32::from(len));
        Ok(())
    }

    fn bits(&mut self, v: u32, n: u32) {
        self.put(v, n);
    }

    fn restart(&mut self, index: u8) {
        self.flush();
        self.out.extend_from_slice(&[0xFF, 0xD0 + (index & 7)]);
    }
}

/// Magnitude category (number of bits) of `v`.
fn category(v: i32) -> u32 {
    32 - v.unsigned_abs().leading_zeros()
}

fn encode_block<S: Sink>(b: &[i16], pred: &mut i32, ci: usize, sink: &mut S) -> Result<()> {
    let diff = i32::from(b[0]) - *pred;
    *pred = i32::from(b[0]);
    let cat = category(diff);
    if cat > 15 {
        return Err("DC difference out of range".into());
    }
    sink.symbol(0, ci, cat as u8)?;
    sink.bits(
        if diff < 0 {
            (diff - 1) as u32
        } else {
            diff as u32
        },
        cat,
    );
    let mut run = 0u32;
    for &z in &ZIGZAG[1..] {
        let v = i32::from(b[usize::from(z)]);
        if v == 0 {
            run += 1;
            continue;
        }
        while run > 15 {
            sink.symbol(1, ci, 0xF0)?;
            run -= 16;
        }
        let cat = category(v);
        sink.symbol(1, ci, ((run << 4) | cat) as u8)?;
        sink.bits(if v < 0 { (v - 1) as u32 } else { v as u32 }, cat);
        run = 0;
    }
    if run > 0 {
        sink.symbol(1, ci, 0x00)?;
    }
    Ok(())
}

fn encode_scan<S: Sink>(
    f: &Frame,
    restart_interval: usize,
    sink: &mut S,
    deadline: Instant,
) -> Result<()> {
    let mut preds = [0i32; 4];
    let mut rst = 0u8;
    for m in 0..f.mcus_x * f.mcus_y {
        if m % f.mcus_x == 0 {
            check_time(deadline)?;
        }
        if restart_interval > 0 && m > 0 && m % restart_interval == 0 {
            sink.restart(rst);
            rst = rst.wrapping_add(1);
            preds = [0; 4];
        }
        let (mx, my) = (m % f.mcus_x, m / f.mcus_x);
        for (ci, c) in f.comps.iter().enumerate() {
            for by in 0..c.v {
                for bx in 0..c.h {
                    let blk = (my * c.v + by) * c.blocks_w + mx * c.h + bx;
                    encode_block(&c.coefs[blk * 64..blk * 64 + 64], &mut preds[ci], ci, sink)?;
                }
            }
        }
    }
    Ok(())
}

/// Optimal Huffman table for `freq` (JPEG Annex K.2, as libjpeg's `jpeg_gen_optimal_table`),
/// with code lengths limited to 16 and the all-ones code reserved.
fn optimal_table(freq: &[u32; 256]) -> HuffSpec {
    let mut f = [0u64; 257];
    for (d, &s) in f.iter_mut().zip(freq) {
        *d = u64::from(s);
    }
    f[256] = 1;
    let mut size = [0usize; 257];
    let mut others = [-1i32; 257];
    loop {
        let (mut c1, mut c2) = (-1i32, -1i32);
        let mut v = u64::MAX;
        for (i, &fi) in f.iter().enumerate() {
            if fi != 0 && fi <= v {
                v = fi;
                c1 = i as i32;
            }
        }
        v = u64::MAX;
        for (i, &fi) in f.iter().enumerate() {
            if fi != 0 && fi <= v && i as i32 != c1 {
                v = fi;
                c2 = i as i32;
            }
        }
        if c2 < 0 {
            break;
        }
        let (mut a, mut b) = (c1 as usize, c2 as usize);
        f[a] += f[b];
        f[b] = 0;
        size[a] += 1;
        while others[a] >= 0 {
            a = others[a] as usize;
            size[a] += 1;
        }
        others[a] = b as i32;
        size[b] += 1;
        while others[b] >= 0 {
            b = others[b] as usize;
            size[b] += 1;
        }
    }
    let mut bits = [0i32; 64];
    for &s in &size {
        if s > 0 {
            bits[s] += 1;
        }
    }
    for i in (17..bits.len()).rev() {
        while bits[i] > 0 {
            let mut j = i - 2;
            while bits[j] == 0 {
                j -= 1;
            }
            bits[i] -= 2;
            bits[i - 1] += 1;
            bits[j + 1] += 2;
            bits[j] -= 1;
        }
    }
    let mut i = 16;
    while bits[i] == 0 {
        i -= 1;
    }
    bits[i] -= 1;
    let mut counts = [0u8; 16];
    for (c, &b) in counts.iter_mut().zip(&bits[1..17]) {
        *c = b as u8;
    }
    let mut syms = Vec::new();
    for len in 1..64 {
        for (s, &sz) in size.iter().enumerate().take(256) {
            if sz == len {
                syms.push(s as u8);
            }
        }
    }
    HuffSpec { counts, syms }
}

fn push_segment(out: &mut Vec<u8>, marker: u8, payload: &[u8]) -> Result<()> {
    let len = u16::try_from(payload.len() + 2).map_err(|_| "JPEG segment too long".to_string())?;
    out.extend_from_slice(&[0xFF, marker]);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(payload);
    Ok(())
}

/// Sets a one-digit XMP `tiff:Orientation` to 1 in place (same length).
fn reset_xmp_orientation(xmp: &mut [u8]) {
    for pat in [&b"tiff:Orientation=\""[..], &b"<tiff:Orientation>"[..]] {
        if let Some(i) = xmp.windows(pat.len()).position(|w| w == pat)
            && let Some(digit) = xmp.get_mut(i + pat.len())
            && digit.is_ascii_digit()
        {
            *digit = b'1';
        }
    }
}

fn encode(meta: Meta, out_frame: &Frame, deadline: Instant) -> Result<Vec<u8>> {
    let Meta {
        segments,
        restart_interval,
        orig,
        ..
    } = meta;
    let f = out_frame;
    let n = f.comps.len();

    // Pass 1: symbol statistics per component.
    let mut counter = Counter {
        freq: [[[0; 256]; 4]; 2],
    };
    encode_scan(f, restart_interval, &mut counter, deadline)?;
    let by_table = |assign: &[(usize, usize)]| {
        let mut t = [[[0u32; 256]; 4]; 2];
        for (ci, &(d, a)) in assign.iter().enumerate() {
            for (dst, n) in t[0][d].iter_mut().zip(&counter.freq[0][ci]) {
                *dst += n;
            }
            for (dst, n) in t[1][a].iter_mut().zip(&counter.freq[1][ci]) {
                *dst += n;
            }
        }
        t
    };

    // Prefer the input's own tables when they cover everything needed.
    let reuse = orig.and_then(|o| {
        let freq = by_table(&o.comp_tables);
        for (class, specs) in [&o.dc, &o.ac].into_iter().enumerate() {
            for (id, spec) in specs.iter().enumerate() {
                if freq[class][id].iter().all(|&c| c == 0) {
                    continue;
                }
                let enc = HuffEnc::new(spec.as_ref()?).ok()?;
                if (0..256).any(|s| freq[class][id][s] > 0 && enc.len[s] == 0) {
                    return None;
                }
            }
        }
        Some((o.comp_tables, o.dc, o.ac))
    });
    let (comp_tables, dc_specs, ac_specs) = match reuse {
        Some(r) => r,
        None => {
            let assign: Vec<(usize, usize)> = (0..n)
                .map(|ci| (usize::from(ci > 0), usize::from(ci > 0)))
                .collect();
            let freq = by_table(&assign);
            let spec = |class: usize, id: usize| {
                freq[class][id]
                    .iter()
                    .any(|&c| c > 0)
                    .then(|| optimal_table(&freq[class][id]))
            };
            (
                assign,
                [spec(0, 0), spec(0, 1), None, None],
                [spec(1, 0), spec(1, 1), None, None],
            )
        }
    };

    // Quantisation tables, deduplicated in component order.
    let mut qtables: Vec<QTable> = Vec::new();
    let mut qids = Vec::new();
    for c in &f.comps {
        let q =
            c.q.clone()
                .ok_or("component without a quantisation table")?;
        let id = qtables.iter().position(|t| *t == q).unwrap_or_else(|| {
            qtables.push(q);
            qtables.len() - 1
        });
        qids.push(id);
    }

    let mut out = vec![0xFF, 0xD8];
    for (marker, payload) in segments {
        let mut payload = payload;
        match marker {
            0xE1 if payload.starts_with(b"Exif\0\0") => {
                let Some(fixed) = exif::normalise(&payload[6..], (f.width as u32, f.height as u32))
                else {
                    continue;
                };
                payload.truncate(6);
                payload.extend_from_slice(&fixed);
            }
            0xE1 if payload.starts_with(b"http://ns.adobe.com/xap/1.0/\0") => {
                reset_xmp_orientation(&mut payload);
            }
            0xE2 if payload.starts_with(b"MPF\0") => continue,
            _ => {}
        }
        push_segment(&mut out, marker, &payload)?;
    }
    for (id, q) in qtables.iter().enumerate() {
        let mut p = vec![(u8::from(q.wide) << 4) | id as u8];
        for &z in &ZIGZAG {
            let v = q.v[usize::from(z)];
            if q.wide {
                p.extend_from_slice(&v.to_be_bytes());
            } else {
                p.push(u8::try_from(v).map_err(|_| "quantisation value out of range")?);
            }
        }
        push_segment(&mut out, 0xDB, &p)?;
    }
    let baseline =
        !qtables.iter().any(|q| q.wide) && comp_tables.iter().all(|&(d, a)| d <= 1 && a <= 1);
    let mut sof = vec![8];
    sof.extend_from_slice(&u16::try_from(f.height).map_err(|_| "height")?.to_be_bytes());
    sof.extend_from_slice(&u16::try_from(f.width).map_err(|_| "width")?.to_be_bytes());
    sof.push(n as u8);
    for (c, &qid) in f.comps.iter().zip(&qids) {
        sof.extend_from_slice(&[c.id, ((c.h as u8) << 4) | c.v as u8, qid as u8]);
    }
    push_segment(&mut out, if baseline { 0xC0 } else { 0xC1 }, &sof)?;
    let mut tables: [[Option<HuffEnc>; 4]; 2] = Default::default();
    for (class, specs) in [(0usize, &dc_specs), (1, &ac_specs)] {
        for (id, spec) in specs.iter().enumerate() {
            let Some(spec) = spec else { continue };
            let used = comp_tables
                .iter()
                .any(|&(d, a)| if class == 0 { d == id } else { a == id });
            if !used {
                continue;
            }
            let mut p = vec![((class as u8) << 4) | id as u8];
            p.extend_from_slice(&spec.counts);
            p.extend_from_slice(&spec.syms);
            push_segment(&mut out, 0xC4, &p)?;
            tables[class][id] = Some(HuffEnc::new(spec)?);
        }
    }
    if restart_interval > 0 {
        push_segment(&mut out, 0xDD, &(restart_interval as u16).to_be_bytes())?;
    }
    let mut sos = vec![n as u8];
    for (c, &(d, a)) in f.comps.iter().zip(&comp_tables) {
        sos.extend_from_slice(&[c.id, ((d as u8) << 4) | a as u8]);
    }
    sos.extend_from_slice(&[0, 63, 0]);
    push_segment(&mut out, 0xDA, &sos)?;
    {
        let mut w = Writer {
            out: &mut out,
            acc: 0,
            nb: 0,
            tables,
            comp_tables: &comp_tables,
        };
        encode_scan(f, restart_interval, &mut w, deadline)?;
        w.flush();
    }
    out.extend_from_slice(&[0xFF, 0xD9]);
    Ok(out)
}

/// Applies `xf` to the JPEG `data` without recompressing it. See the module docs for what
/// is supported, what is preserved and when this refuses. Also returns the number of
/// bytes after the end-of-image marker that were not carried over.
pub fn transform_reporting(data: &[u8], xf: Transform) -> Result<(Vec<u8>, usize)> {
    let deadline = Instant::now() + BUDGET;
    let Parsed { frame, meta } = parse(data, deadline)?;
    let blocks_per_mcu: usize = frame.comps.iter().map(|c| c.h * c.v).sum();
    if frame.comps.len() > 1 && blocks_per_mcu > 10 {
        return Err("more than 10 blocks per MCU".into());
    }
    let out_frame = transform_frame(frame, xf)?;
    let trailer = meta.trailer;
    encode(meta, &out_frame, deadline).map(|bytes| (bytes, trailer))
}

#[cfg(test)]
fn transform(data: &[u8], xf: Transform) -> Result<Vec<u8>> {
    transform_reporting(data, xf).map(|(bytes, _)| bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exif::testutil::{camera_exif, inject_into_jpeg};
    use image::RgbImage;
    use image::codecs::jpeg::JpegEncoder;

    const ALL: [&str; 5] = ["right", "left", "180", "fliph", "flipv"];

    fn deadline() -> Instant {
        Instant::now() + BUDGET
    }

    /// Deterministic picture with enough detail to exercise every coefficient class.
    fn picture(w: u32, h: u32) -> RgbImage {
        let mut s = 0x9E37_79B9u32;
        RgbImage::from_fn(w, h, |x, y| {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            let n = (s & 15) as i32;
            image::Rgb([
                ((x * 255 / w.max(1)) as i32 + n).clamp(0, 255) as u8,
                ((y * 255 / h.max(1)) as i32 + n).clamp(0, 255) as u8,
                (((x ^ y) * 4) as i32 + n).clamp(0, 255) as u8,
            ])
        })
    }

    fn jpeg(img: &RgbImage, quality: u8) -> Vec<u8> {
        let mut out = Vec::new();
        JpegEncoder::new_with_quality(&mut out, quality)
            .encode_image(img)
            .unwrap();
        out
    }

    fn decode(d: &[u8]) -> RgbImage {
        image::load_from_memory_with_format(d, image::ImageFormat::Jpeg)
            .unwrap()
            .to_rgb8()
    }

    /// Start of the entropy-coded data (just after the SOS header).
    fn scan_data(d: &[u8]) -> &[u8] {
        let mut i = 2;
        loop {
            let len = u16::from_be_bytes([d[i + 2], d[i + 3]]) as usize;
            if d[i + 1] == 0xDA {
                return &d[i + 2 + len..];
            }
            i += 2 + len;
        }
    }

    /// `xf` applied to a `w` x `h` plane of anything copyable.
    fn apply_plane<T: Copy>(p: &[T], w: usize, h: usize, xf: Transform) -> (Vec<T>, usize, usize) {
        let d = [w, h];
        let od = [d[usize::from(xf.src[0])], d[usize::from(xf.src[1])]];
        let mut out = Vec::with_capacity(w * h);
        for y in 0..od[1] {
            for x in 0..od[0] {
                let po = [x, y];
                let mut q = [0usize; 2];
                for i in 0..2 {
                    q[usize::from(xf.src[i])] = if xf.neg[i] { od[i] - 1 - po[i] } else { po[i] };
                }
                out.push(p[q[1] * w + q[0]]);
            }
        }
        (out, od[0], od[1])
    }

    /// Dequantised, exact (f64) inverse DCT of one component at block resolution.
    fn plane(f: &Frame, ci: usize) -> (Vec<f64>, usize, usize) {
        let c = &f.comps[ci];
        let q = c.q.as_ref().unwrap();
        let (w, h) = (c.blocks_w * 8, c.blocks_h * 8);
        let mut basis = [[0f64; 8]; 8];
        for (u, row) in basis.iter_mut().enumerate() {
            for (x, b) in row.iter_mut().enumerate() {
                let a = if u == 0 { 0.5 / 2f64.sqrt() } else { 0.5 };
                *b = a * ((2 * x + 1) as f64 * u as f64 * std::f64::consts::PI / 16.0).cos();
            }
        }
        let mut out = vec![0.0; w * h];
        for by in 0..c.blocks_h {
            for bx in 0..c.blocks_w {
                let blk = &c.coefs[(by * c.blocks_w + bx) * 64..][..64];
                for y in 0..8 {
                    for x in 0..8 {
                        let mut s = 0.0;
                        for v in 0..8 {
                            for u in 0..8 {
                                let k = v * 8 + u;
                                s += f64::from(blk[k])
                                    * f64::from(q.v[k])
                                    * basis[u][x]
                                    * basis[v][y];
                            }
                        }
                        out[(by * 8 + y) * w + bx * 8 + x] = s;
                    }
                }
            }
        }
        (out, w, h)
    }

    /// Mathematically exact check that `out` is `data` with `xf` applied: the dequantised
    /// exact (f64) IDCT of every output component equals `xf` applied to the same IDCT of
    /// the input, to 1e-6. This is independent of any decoder's rounding.
    fn assert_planes_exact(data: &[u8], out: &[u8], xf: Transform, what: &str) {
        let fi = parse(data, deadline()).unwrap().frame;
        let fo = parse(out, deadline()).unwrap().frame;
        assert_eq!(fi.comps.len(), fo.comps.len(), "{what}");
        for ci in 0..fi.comps.len() {
            let (p, w, h) = plane(&fi, ci);
            let (e, ew, eh) = apply_plane(&p, w, h, xf);
            let (g, gw, gh) = plane(&fo, ci);
            assert_eq!((ew, eh), (gw, gh), "{what}: component {ci} grid");
            let worst = e
                .iter()
                .zip(&g)
                .map(|(a, b)| (a - b).abs())
                .fold(0.0, f64::max);
            assert!(
                worst < 1e-6,
                "{what}: component {ci} IDCT differs by {worst}"
            );
        }
    }

    /// As the `image` crate (a fixed-point decoder) decodes the files: the output has the
    /// transformed size and its pixels match the transformed input decode up to decoder
    /// rounding. The decoder's IDCT rounds between its row and column passes, so it is not
    /// symmetric under transposition and the two decodes can differ by a few levels (quarter
    /// turns reached 3, flips 2 in these fixtures). The exact guarantee is the one above.
    fn assert_decodes_alike(data: &[u8], out: &[u8], xf: Transform, what: &str) {
        let (a, b) = (decode(data), decode(out));
        let pixels: Vec<[u8; 3]> = a.pixels().map(|p| p.0).collect();
        let (want, ww, wh) = apply_plane(&pixels, a.width() as usize, a.height() as usize, xf);
        assert_eq!(
            (b.width() as usize, b.height() as usize),
            (ww, wh),
            "{what}: size"
        );
        let tolerance = 3;
        let worst = want
            .iter()
            .zip(b.pixels())
            .flat_map(|(p, q)| {
                p.iter()
                    .zip(q.0)
                    .map(|(x, y)| (i32::from(*x) - i32::from(y)).abs())
            })
            .max()
            .unwrap_or(0);
        assert!(
            worst <= tolerance,
            "{what}: decoded pixels differ by {worst}"
        );
    }

    fn assert_transformed(data: &[u8], out: &[u8], xf: Transform, what: &str) {
        assert_planes_exact(data, out, xf, what);
        assert_decodes_alike(data, out, xf, what);
    }

    /// A sequential JPEG with chroma subsampled `hs` x `vs` (the `image` encoder only writes
    /// 4:4:4): forward DCT and quantisation of the test picture, entropy coded by this
    /// module's own writer.
    fn subsampled_jpeg(w: u32, h: u32, hs: usize, vs: usize) -> Vec<u8> {
        let img = picture(w, h);
        let (w, h) = (w as usize, h as usize);
        let mut ycc = vec![[0f64; 3]; w * h];
        for (i, p) in img.pixels().enumerate() {
            let [r, g, b] = p.0.map(f64::from);
            ycc[i] = [
                0.299 * r + 0.587 * g + 0.114 * b,
                -0.168_736 * r - 0.331_264 * g + 0.5 * b + 128.0,
                0.5 * r - 0.418_688 * g - 0.081_312 * b + 128.0,
            ];
        }
        let (mcus_x, mcus_y) = (w.div_ceil(8 * hs), h.div_ceil(8 * vs));
        let mut basis = [[0f64; 8]; 8];
        for (u, row) in basis.iter_mut().enumerate() {
            for (x, b) in row.iter_mut().enumerate() {
                let a = if u == 0 { 0.5 / 2f64.sqrt() } else { 0.5 };
                *b = a * ((2 * x + 1) as f64 * u as f64 * std::f64::consts::PI / 16.0).cos();
            }
        }
        let comps = (0..3)
            .map(|ci| {
                let (ch, cv) = if ci == 0 { (hs, vs) } else { (1, 1) };
                let (bw, bh) = (mcus_x * ch, mcus_y * cv);
                let q = if ci == 0 { 6u16 } else { 9 };
                // Box-filter the plane down by hs/ch x vs/cv, edge-clamped.
                let (fx, fy) = (hs / ch, vs / cv);
                let sample = |x: usize, y: usize| {
                    let mut s = 0.0;
                    for dy in 0..fy {
                        for dx in 0..fx {
                            let (sx, sy) = (((x * fx) + dx).min(w - 1), ((y * fy) + dy).min(h - 1));
                            s += ycc[sy * w + sx][ci];
                        }
                    }
                    s / (fx * fy) as f64 - 128.0
                };
                let mut coefs = vec![0i16; bw * bh * 64];
                for by in 0..bh {
                    for bx in 0..bw {
                        for v in 0..8 {
                            for u in 0..8 {
                                let mut s = 0.0;
                                for y in 0..8 {
                                    for x in 0..8 {
                                        s += sample(bx * 8 + x, by * 8 + y)
                                            * basis[u][x]
                                            * basis[v][y];
                                    }
                                }
                                coefs[(by * bw + bx) * 64 + v * 8 + u] =
                                    (s / f64::from(q)).round() as i16;
                            }
                        }
                    }
                }
                Comp {
                    id: ci as u8 + 1,
                    h: ch,
                    v: cv,
                    tq: usize::from(ci > 0),
                    q: Some(QTable {
                        wide: false,
                        v: [q; 64],
                    }),
                    blocks_w: bw,
                    blocks_h: bh,
                    coefs,
                }
            })
            .collect();
        let frame = Frame {
            width: w,
            height: h,
            progressive: false,
            comps,
            hmax: hs,
            vmax: vs,
            mcus_x,
            mcus_y,
        };
        let meta = Meta {
            segments: Vec::new(),
            restart_interval: 0,
            orig: None,
            trailer: 0,
        };
        encode(meta, &frame, deadline()).unwrap()
    }

    fn sampling(d: &[u8]) -> Vec<(usize, usize)> {
        let f = parse(d, deadline()).unwrap().frame;
        f.comps.iter().map(|c| (c.h, c.v)).collect()
    }

    #[test]
    fn transform_algebra() {
        use Transform as T;
        assert_eq!(T::RIGHT.then(T::RIGHT), T::ROTATE_180);
        assert_eq!(T::RIGHT.then(T::LEFT), T::IDENTITY);
        assert_eq!(T::FLIP_H.then(T::FLIP_V), T::ROTATE_180);
        assert_eq!(T::from_exif(6), T::RIGHT);
        assert_eq!(T::from_exif(8), T::LEFT);
        assert_eq!(T::from_exif(5).then(T::from_exif(5)), T::IDENTITY);
        assert_eq!(T::from_exif(0), T::IDENTITY);
        let r4 = (0..4).fold(T::IDENTITY, |t, _| t.then(T::RIGHT));
        assert_eq!(r4, T::IDENTITY);
        // EXIF 5 (transpose) is the main-diagonal mirror: (x, y) becomes (y, x).
        let px: Vec<u32> = (0..6).collect(); // 3 wide, 2 tall
        assert_eq!(
            apply_plane(&px, 3, 2, T::from_exif(5)).0,
            [0, 3, 1, 4, 2, 5]
        );
        // Rotate right: the left column becomes the top row, read bottom to top.
        assert_eq!(apply_plane(&px, 3, 2, T::RIGHT).0, [3, 0, 4, 1, 5, 2]);
    }

    #[test]
    fn every_transform_is_exact() {
        // 64x48 is a whole number of the 16x16 MCUs of 4:2:0, so all five are lossless.
        let data = jpeg(&picture(64, 48), 85);
        for name in ALL {
            let xf = Transform::by_name(name).unwrap();
            let out = transform(&data, xf).unwrap();
            assert_transformed(&data, &out, xf, name);
        }
    }

    #[test]
    fn four_right_turns_are_byte_identical_scan_data() {
        let data = jpeg(&picture(80, 48), 90);
        let mut cur = data.clone();
        for _ in 0..4 {
            cur = transform(&cur, Transform::RIGHT).unwrap();
        }
        assert_eq!(scan_data(&cur), scan_data(&data), "scan data");
        assert_eq!(decode(&cur).as_raw(), decode(&data).as_raw());
    }

    #[test]
    fn grayscale_images_transform() {
        let gray = image::DynamicImage::ImageRgb8(picture(40, 24)).to_luma8();
        let mut data = Vec::new();
        JpegEncoder::new_with_quality(&mut data, 80)
            .encode_image(&gray)
            .unwrap();
        // 40x24 gray is 5x3 whole 8x8 MCUs.
        for name in ALL {
            let xf = Transform::by_name(name).unwrap();
            let out = transform(&data, xf).unwrap();
            assert_transformed(&data, &out, xf, name);
        }
    }

    #[test]
    fn restart_intervals_are_kept_and_decoded() {
        let data = jpeg(&picture(64, 64), 80);
        let identity = || {
            transform_frame(parse(&data, deadline()).unwrap().frame, Transform::IDENTITY).unwrap()
        };
        let source = parse(&data, deadline()).unwrap();
        let with_dri = encode(
            Meta {
                segments: source.meta.segments,
                restart_interval: 3,
                orig: None,
                trailer: 0,
            },
            &identity(),
            deadline(),
        )
        .unwrap();
        assert!(
            with_dri.windows(2).any(|w| w == [0xFF, 0xD0]),
            "RST0 written"
        );
        assert_eq!(decode(&with_dri).as_raw(), decode(&data).as_raw());
        for name in ALL {
            let xf = Transform::by_name(name).unwrap();
            let out = transform(&with_dri, xf).unwrap();
            assert!(
                out.windows(2).any(|w| w == [0xFF, 0xDD]),
                "{name}: DRI kept"
            );
            assert_transformed(&with_dri, &out, xf, name);
        }
    }

    #[test]
    fn subsampled_images_transform_exactly_and_swap_sampling_factors() {
        for (hs, vs) in [(2, 2), (2, 1), (1, 2)] {
            let data = subsampled_jpeg(64, 48, hs, vs);
            assert_eq!(sampling(&data), [(hs, vs), (1, 1), (1, 1)]);
            for name in ALL {
                let xf = Transform::by_name(name).unwrap();
                let out = transform(&data, xf).unwrap();
                let what = format!("{hs}x{vs} {name}");
                assert_planes_exact(&data, &out, xf, &what);
                let swapped = if xf.swaps_axes() { (vs, hs) } else { (hs, vs) };
                assert_eq!(sampling(&out), [swapped, (1, 1), (1, 1)], "{what}");
                // Still a valid file for an independent decoder, with the right size.
                let size = decode(&out).dimensions();
                assert_eq!(
                    size,
                    if xf.swaps_axes() { (48, 64) } else { (64, 48) },
                    "{what}"
                );
            }
        }
    }

    #[test]
    fn partial_edge_mcus_are_refused_not_trimmed() {
        // 4:4:4 has 8x8 MCUs: 50x50 is partial on both axes, 64x40 is not partial at all.
        let data = jpeg(&picture(50, 50), 85);
        for name in ALL {
            let err = transform(&data, Transform::by_name(name).unwrap()).unwrap_err();
            assert!(err.contains("cannot be done losslessly"), "{name}: {err}");
        }
        // A plain transpose never moves the partial edge, so it stays lossless.
        let transpose = Transform::from_exif(5);
        let out = transform(&data, transpose).unwrap();
        assert_eq!(decode(&out).dimensions(), (50, 50));
        assert_transformed(&data, &out, transpose, "transpose");
        let data = jpeg(&picture(64, 40), 85);
        for name in ALL {
            let xf = Transform::by_name(name).unwrap();
            assert_transformed(&data, &transform(&data, xf).unwrap(), xf, name);
        }
        // 4:2:0 has 16x16 MCUs: with 64x40 the height is partial. A right turn mirrors the
        // input's y axis (needs the height aligned), a left turn mirrors x (the width).
        let data = subsampled_jpeg(64, 40, 2, 2);
        assert!(transform(&data, Transform::RIGHT).is_err());
        assert!(transform(&data, Transform::FLIP_V).is_err());
        assert!(transform(&data, Transform::ROTATE_180).is_err());
        for xf in [Transform::LEFT, Transform::FLIP_H, transpose] {
            let out = transform(&data, xf).unwrap();
            assert_planes_exact(&data, &out, xf, "aligned axis");
        }
        let data = subsampled_jpeg(50, 50, 2, 2);
        for name in ALL {
            assert!(transform(&data, Transform::by_name(name).unwrap()).is_err());
        }
        let out = transform(&data, transpose).unwrap();
        assert_planes_exact(&data, &out, transpose, "partial transpose");
    }

    #[test]
    fn data_after_the_image_is_reported_and_dropped() {
        let plain = jpeg(&picture(32, 32), 80);
        let mut with_trailer = plain.clone();
        with_trailer.extend_from_slice(b"SEFHvendor-extension-data....SEFT");
        let (out, dropped) = transform_reporting(&with_trailer, Transform::RIGHT).unwrap();
        assert_eq!(dropped, 33);
        assert!(out.ends_with(&[0xFF, 0xD9]), "output ends at EOI");
        // Padding is not data.
        let mut padded = plain;
        padded.extend_from_slice(&[0, 0, b'\n']);
        assert_eq!(transform_reporting(&padded, Transform::RIGHT).unwrap().1, 0);
    }

    #[test]
    fn exif_is_normalised_and_pixels_stay_exact() {
        let src = inject_into_jpeg(&jpeg(&picture(64, 48), 85), &camera_exif(6, (64, 48)));
        let out = transform(&src, Transform::RIGHT).unwrap();
        let block = exif::from_container(&out).expect("EXIF kept");
        assert_eq!(exif::orientation(block), Some(1));
        let s = exif::summarize(block);
        assert_eq!(s.make.as_deref(), Some("Thumbcam"));
        assert!(s.gps.is_some());
        assert!(block.len() < 2000, "thumbnail dropped: {}", block.len());
        assert_eq!(decode(&out).dimensions(), (48, 64));
        assert_transformed(&src, &out, Transform::RIGHT, "with EXIF");
    }

    /// The fixture is a progressive re-encoding of a baseline file's coefficients written by
    /// the test-side encoder below (successive approximation, EOB runs), so the decoder's
    /// progressive paths run against an independent writer.
    #[test]
    fn progressive_input_becomes_equivalent_sequential_output() {
        let data = jpeg(&picture(64, 48), 85);
        let prog = progressive::to_progressive(&data);
        assert!(prog.windows(2).any(|w| w == [0xFF, 0xC2]), "SOF2 present");
        assert_eq!(
            decode(&prog).as_raw(),
            decode(&data).as_raw(),
            "progressive fixture decodes like the baseline source"
        );
        for name in ALL {
            let xf = Transform::by_name(name).unwrap();
            let out = transform(&prog, xf).unwrap();
            assert!(
                out.windows(2).any(|w| w == [0xFF, 0xC0]),
                "{name}: sequential output"
            );
            // The very coefficients of the baseline source came through.
            assert_transformed(&data, &out, xf, name);
        }
    }

    #[test]
    fn files_without_reusable_tables_get_generated_ones() {
        // A progressive source has no reusable single-scan tables, so optimal ones are
        // generated; the output must be complete and exact.
        let data = jpeg(&picture(48, 48), 60);
        let prog = progressive::to_progressive(&data);
        let out = transform(&prog, Transform::RIGHT).unwrap();
        assert_transformed(&data, &out, Transform::RIGHT, "optimal tables");
    }

    #[test]
    fn hostile_input_is_rejected_without_panicking() {
        let data = jpeg(&picture(32, 32), 80);
        // Truncations and single-byte corruptions are handled or rejected cleanly.
        for cut in (0..data.len()).step_by(7) {
            let _ = transform(&data[..cut], Transform::RIGHT);
        }
        for i in (0..data.len()).step_by(5) {
            let mut bad = data.clone();
            bad[i] ^= 0xA5;
            let _ = transform(&bad, Transform::RIGHT);
        }
        // Declared 65535x65535 (beyond the pixel budget) is refused before allocating.
        let mut huge = data.clone();
        let sof = huge.windows(2).position(|w| w == [0xFF, 0xC0]).unwrap();
        huge[sof + 5..sof + 9].copy_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]);
        assert!(transform(&huge, Transform::RIGHT).is_err());
        assert!(transform(b"not a jpeg", Transform::RIGHT).is_err());
        // A scan that stops early is an error, not a silently gray image.
        let cut = data.len() - 40;
        assert!(transform(&data[..cut], Transform::RIGHT).is_err());
    }

    mod progressive {
        //! Test-only progressive encoder: DC first (Al=1) and refine, then AC bands with
        //! successive approximation, EOB runs included, using generated Huffman tables.
        use super::super::*;

        struct BitOut(Vec<u8>, u64, u32);

        impl BitOut {
            fn put(&mut self, v: u32, n: u32) {
                if n == 0 {
                    return;
                }
                self.1 = (self.1 << n) | (u64::from(v) & ((1 << n) - 1));
                self.2 += n;
                while self.2 >= 8 {
                    let b = (self.1 >> (self.2 - 8)) as u8;
                    self.0.push(b);
                    if b == 0xFF {
                        self.0.push(0);
                    }
                    self.2 -= 8;
                }
            }
            fn flush(&mut self) {
                if self.2 > 0 {
                    let pad = 8 - self.2;
                    self.put((1 << pad) - 1, pad);
                }
            }
        }

        /// DC: categories 0-11 at 4 bits. AC: every symbol whose low nibble (the magnitude
        /// category) is at most 10, at 8 bits; that covers run/size pairs, ZRL (0xF0) and the
        /// EOBn symbols (0x00, 0x10 .. 0xE0).
        fn tables() -> (HuffSpec, HuffEnc, HuffSpec, HuffEnc) {
            let mut dc_counts = [0u8; 16];
            dc_counts[3] = 12;
            let dc = HuffSpec {
                counts: dc_counts,
                syms: (0..12u8).collect(),
            };
            let ac_syms: Vec<u8> = (0..=255u8).filter(|b| b & 15 <= 10).collect();
            let mut ac_counts = [0u8; 16];
            ac_counts[7] = ac_syms.len() as u8;
            let ac = HuffSpec {
                counts: ac_counts,
                syms: ac_syms,
            };
            let (dce, ace) = (HuffEnc::new(&dc).unwrap(), HuffEnc::new(&ac).unwrap());
            (dc, dce, ac, ace)
        }

        fn amag(v: i32, n: u32, out: &mut BitOut) {
            out.put(if v < 0 { (v - 1) as u32 } else { v as u32 }, n);
        }

        pub fn to_progressive(jpeg: &[u8]) -> Vec<u8> {
            let parsed = parse(jpeg, Instant::now() + BUDGET).unwrap();
            let f = &parsed.frame;
            let (dc_spec, dce, ac_spec, enc) = tables();
            let mut out = vec![0xFF, 0xD8];
            // Quantisation tables and frame header as in the source, but SOF2.
            let mut qt: Vec<QTable> = Vec::new();
            let mut qids = Vec::new();
            for c in &f.comps {
                let q = c.q.clone().unwrap();
                let id = qt.iter().position(|t| *t == q).unwrap_or_else(|| {
                    qt.push(q);
                    qt.len() - 1
                });
                qids.push(id);
            }
            for (id, q) in qt.iter().enumerate() {
                let mut p = vec![id as u8];
                p.extend(ZIGZAG.iter().map(|&z| q.v[usize::from(z)] as u8));
                push_segment(&mut out, 0xDB, &p).unwrap();
            }
            let mut sof = vec![8];
            sof.extend_from_slice(&(f.height as u16).to_be_bytes());
            sof.extend_from_slice(&(f.width as u16).to_be_bytes());
            sof.push(f.comps.len() as u8);
            for (c, &q) in f.comps.iter().zip(&qids) {
                sof.extend_from_slice(&[c.id, ((c.h as u8) << 4) | c.v as u8, q as u8]);
            }
            push_segment(&mut out, 0xC2, &sof).unwrap();
            let mut dht = vec![0x00];
            dht.extend_from_slice(&dc_spec.counts);
            dht.extend_from_slice(&dc_spec.syms);
            push_segment(&mut out, 0xC4, &dht).unwrap();
            let mut dht = vec![0x10];
            dht.extend_from_slice(&ac_spec.counts);
            dht.extend_from_slice(&ac_spec.syms);
            push_segment(&mut out, 0xC4, &dht).unwrap();

            let n = f.comps.len();
            let sos = |out: &mut Vec<u8>, comps: &[usize], ss: u8, se: u8, ah: u8, al: u8| {
                let mut p = vec![comps.len() as u8];
                for &ci in comps {
                    p.extend_from_slice(&[f.comps[ci].id, 0x00]);
                }
                p.extend_from_slice(&[ss, se, (ah << 4) | al]);
                push_segment(out, 0xDA, &p).unwrap();
            };
            let all: Vec<usize> = (0..n).collect();

            // DC first, Al = 1, interleaved.
            sos(&mut out, &all, 0, 0, 0, 1);
            let mut b = BitOut(Vec::new(), 0, 0);
            let mut pred = [0i32; 4];
            for m in 0..f.mcus_x * f.mcus_y {
                let (mx, my) = (m % f.mcus_x, m / f.mcus_x);
                for (ci, c) in f.comps.iter().enumerate() {
                    for by in 0..c.v {
                        for bx in 0..c.h {
                            let blk = (my * c.v + by) * c.blocks_w + mx * c.h + bx;
                            let dc = i32::from(c.coefs[blk * 64]) >> 1;
                            let diff = dc - pred[ci];
                            pred[ci] = dc;
                            let cat = category(diff);
                            b.put(
                                u32::from(dce.code[cat as usize]),
                                u32::from(dce.len[cat as usize]),
                            );
                            amag(diff, cat, &mut b);
                        }
                    }
                }
            }
            b.flush();
            out.extend(b.0);

            // AC bands per component, non-interleaved: 1-5 and 6-63 with Al = 1, then
            // refinement of both bands, then the DC refinement bit.
            let grid = |ci: usize| {
                let c = &f.comps[ci];
                (
                    ceil_div(ceil_div(f.width * c.h, f.hmax), 8),
                    ceil_div(ceil_div(f.height * c.v, f.vmax), 8),
                )
            };
            for ci in 0..n {
                let (gw, gh) = grid(ci);
                let c = &f.comps[ci];
                for (ss, se) in [(1usize, 5usize), (6, 63)] {
                    sos(&mut out, &[ci], ss as u8, se as u8, 0, 1);
                    let mut b = BitOut(Vec::new(), 0, 0);
                    let mut eobrun = 0u32;
                    let flush_eob = |b: &mut BitOut, run: &mut u32| {
                        if *run > 0 {
                            let r = 31 - run.leading_zeros();
                            b.put(
                                u32::from(enc.code[(r << 4) as usize]),
                                u32::from(enc.len[(r << 4) as usize]),
                            );
                            b.put(*run - (1 << r), r);
                            *run = 0;
                        }
                    };
                    for y in 0..gh {
                        for x in 0..gw {
                            let blk = y * c.blocks_w + x;
                            let blk = &c.coefs[blk * 64..blk * 64 + 64];
                            let mut run = 0u32;
                            for k in ss..=se {
                                let v = i32::from(blk[usize::from(ZIGZAG[k])]);
                                // Point transform: toward zero, like libjpeg.
                                let t = if v < 0 { -((-v) >> 1) } else { v >> 1 };
                                if t == 0 {
                                    run += 1;
                                    continue;
                                }
                                flush_eob(&mut b, &mut eobrun);
                                while run > 15 {
                                    b.put(u32::from(enc.code[0xF0]), u32::from(enc.len[0xF0]));
                                    run -= 16;
                                }
                                let cat = category(t);
                                let s = ((run << 4) | cat) as usize;
                                b.put(u32::from(enc.code[s]), u32::from(enc.len[s]));
                                amag(t, cat, &mut b);
                                run = 0;
                            }
                            if run > 0 {
                                eobrun += 1;
                                if eobrun == 0x7FFF {
                                    flush_eob(&mut b, &mut eobrun);
                                }
                            }
                        }
                    }
                    flush_eob(&mut b, &mut eobrun);
                    b.flush();
                    out.extend(b.0);
                }
                // Refinement scans for both bands (Ah = 1, Al = 0).
                for (ss, se) in [(1usize, 5usize), (6, 63)] {
                    sos(&mut out, &[ci], ss as u8, se as u8, 1, 0);
                    let mut b = BitOut(Vec::new(), 0, 0);
                    // Pending EOB run with the correction bits it must carry.
                    let mut eobrun = 0u32;
                    let mut pending: Vec<u8> = Vec::new();
                    let flush_eob = |b: &mut BitOut, run: &mut u32, bits: &mut Vec<u8>| {
                        if *run > 0 {
                            let r = 31 - run.leading_zeros();
                            b.put(
                                u32::from(enc.code[(r << 4) as usize]),
                                u32::from(enc.len[(r << 4) as usize]),
                            );
                            b.put(*run - (1 << r), r);
                            *run = 0;
                        }
                        for &bit in bits.iter() {
                            b.put(u32::from(bit), 1);
                        }
                        bits.clear();
                    };
                    for y in 0..gh {
                        for x in 0..gw {
                            let blk = y * c.blocks_w + x;
                            let blk = &c.coefs[blk * 64..blk * 64 + 64];
                            // Absolute values after the first-pass point transform.
                            let abs: Vec<i32> = (ss..=se)
                                .map(|k| {
                                    let v = i32::from(blk[usize::from(ZIGZAG[k])]);
                                    v.abs()
                                })
                                .collect();
                            let first: Vec<i32> = abs.iter().map(|a| a >> 1).collect();
                            let last_new = abs
                                .iter()
                                .zip(&first)
                                .rposition(|(a, f)| *f == 0 && *a == 1);
                            let mut run = 0u32;
                            let mut corr: Vec<u8> = Vec::new();
                            for (i, k) in (ss..=se).enumerate() {
                                let v = i32::from(blk[usize::from(ZIGZAG[k])]);
                                if first[i] != 0 {
                                    corr.push((abs[i] & 1) as u8);
                                    continue;
                                }
                                if abs[i] == 1 {
                                    // Newly non-zero coefficient (magnitude 1 at Al = 0).
                                    while run > 15 && last_new.is_some_and(|l| i <= l) {
                                        flush_eob(&mut b, &mut eobrun, &mut pending);
                                        b.put(u32::from(enc.code[0xF0]), u32::from(enc.len[0xF0]));
                                        for &bit in &corr {
                                            b.put(u32::from(bit), 1);
                                        }
                                        corr.clear();
                                        run -= 16;
                                    }
                                    flush_eob(&mut b, &mut eobrun, &mut pending);
                                    let s = ((run << 4) | 1) as usize;
                                    b.put(u32::from(enc.code[s]), u32::from(enc.len[s]));
                                    b.put(u32::from(v > 0), 1);
                                    for &bit in &corr {
                                        b.put(u32::from(bit), 1);
                                    }
                                    corr.clear();
                                    run = 0;
                                } else {
                                    run += 1;
                                }
                            }
                            if run > 0 || !corr.is_empty() {
                                eobrun += 1;
                                pending.extend_from_slice(&corr);
                                if eobrun == 0x7FFF || pending.len() > 900 {
                                    flush_eob(&mut b, &mut eobrun, &mut pending);
                                }
                            }
                        }
                    }
                    flush_eob(&mut b, &mut eobrun, &mut pending);
                    b.flush();
                    out.extend(b.0);
                }
            }
            // DC refinement, interleaved.
            sos(&mut out, &all, 0, 0, 1, 0);
            let mut b = BitOut(Vec::new(), 0, 0);
            for m in 0..f.mcus_x * f.mcus_y {
                let (mx, my) = (m % f.mcus_x, m / f.mcus_x);
                for c in &f.comps {
                    for by in 0..c.v {
                        for bx in 0..c.h {
                            let blk = (my * c.v + by) * c.blocks_w + mx * c.h + bx;
                            b.put(u32::from(c.coefs[blk * 64] as u16 & 1), 1);
                        }
                    }
                }
            }
            b.flush();
            out.extend(b.0);
            out.extend_from_slice(&[0xFF, 0xD9]);
            out
        }
    }
}
