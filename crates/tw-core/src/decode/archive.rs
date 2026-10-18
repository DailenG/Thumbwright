//! 7-Zip and RAR archives: a contact sheet of contained images (`.7z`, `.rar`) or the first
//! image of a comic book (`.cb7`, `.cbr`), with the same image selection as ZIP archives.
//!
//! Both formats need real decompression (LZMA/LZMA2/PPMd/Deflate/BCJ for 7z; the RAR 1.x to
//! 7 coders), so this decoder runs in the worker process (`Placement::Worker`), which also
//! contains an allocation failure on a hostile dictionary size. On top of that, in-process:
//!
//! - the archive structure is read first (names and declared sizes only); candidates are
//!   chosen from it and only entries that can matter are decoded: a 7z block or RAR member
//!   that holds none is skipped, and decoding stops as soon as the result is settled;
//! - every entry is read through `take(limit + 1)` against
//!   [`limits::ARCHIVE_ENTRY_MAX_BYTES`], and everything decompressed, including data that
//!   only has to be passed over to reach a later entry of a solid stream, counts against
//!   [`limits::ARCHIVE_TOTAL_MAX_BYTES`];
//! - declared LZMA/LZMA2/PPMd dictionary sizes and RAR 5 dictionaries are capped before any
//!   decoder allocates them; header counts are capped.
//!
//! Encrypted archives and compression methods not compiled in (BZip2, Brotli, LZ4, Zstd)
//! yield `Unsupported`. A comic named `.cbr`/`.cb7`/`.rar`/`.7z` that is really a ZIP (very
//! common) is routed to the ZIP reader by its signature.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{self, Cursor, Read, Write};
use std::sync::{Arc, Mutex, PoisonError};

use image::DynamicImage;
use rars::{
    Archive as RarArchive, ArchiveMember, ArchiveReadOptions, ArchiveReader, ExtractionDecision,
    ExtractionErrorAction,
};
use sevenz_rust2::{Archive as SevenArchive, BlockDecoder, Password};

use super::zip_preview::{
    MAX_DECODE_ATTEMPTS, MAX_ENTRIES, SHEET_CELL, SHEET_MAX_TILES, compose_sheet, normalize,
    rank_images, shrink_to,
};
use super::{DecodeError, Result, decode_bytes};
use crate::limits;

pub const EXTENSIONS: &[&str] = &["7z", "cb7", "rar", "cbr"];

/// RAR headers parsed per archive (the parser copies the archive and keeps per-member
/// metadata in memory).
const RAR_MAX_HEADERS: u64 = 100_000;
const RAR_MAX_HEADER_BYTES: u64 = 64 * 1024 * 1024;
/// Largest RAR accepted: the RAR reader copies its input.
const RAR_MAX_INPUT_BYTES: u64 = 512 * 1024 * 1024;

const SEVENZ_SIGNATURE: &[u8] = b"7z\xBC\xAF\x27\x1C";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    /// Contact sheet of up to [`SHEET_MAX_TILES`] images.
    Sheet,
    /// First image (a "cover" first, then natural order), like `.cbz`.
    Comic,
}

/// Decompression budget (REQ-SEC-1); tests shrink it.
#[derive(Clone, Copy, Debug)]
struct Budget {
    entry_max: u64,
    total_max: u64,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            entry_max: limits::ARCHIVE_ENTRY_MAX_BYTES,
            total_max: limits::ARCHIVE_TOTAL_MAX_BYTES,
        }
    }
}

pub fn extract(data: &[u8], ext: &str) -> Result<DynamicImage> {
    extract_with(data, ext, Budget::default())
}

fn extract_with(data: &[u8], ext: &str, budget: Budget) -> Result<DynamicImage> {
    let mode = match ext.to_ascii_lowercase().as_str() {
        "7z" | "rar" => Mode::Sheet,
        "cb7" | "cbr" => Mode::Comic,
        _ => return Err(DecodeError::Unsupported),
    };
    // The signature, not the extension, picks the reader (".cbr" files that are ZIPs are
    // everywhere).
    if data.starts_with(SEVENZ_SIGNATURE) {
        sevenz(data, mode, budget)
    } else if data.starts_with(b"PK\x03\x04") || data.starts_with(b"PK\x05\x06") {
        super::zip_preview::extract(data, if mode == Mode::Comic { "cbz" } else { "zip" })
    } else if rars::detect_archive_family(data).is_some() {
        rar(data, mode, budget)
    } else {
        Err(DecodeError::Unsupported)
    }
}

// ---------------------------------------------------------------------------------------
// Candidate selection shared by both readers

enum Slot {
    /// Not seen in the stream yet.
    Pending,
    /// Being extracted.
    InFlight,
    Failed,
    Done(DynamicImage),
}

/// The images an archive is read for, in preference order, and what was decoded so far.
/// The archive's own order (which a solid stream forces on the reader) is independent of
/// the preference order: results are collected per rank and [`Picker::done`] says when the
/// lowest ranks are settled.
struct Picker {
    mode: Mode,
    ranks: HashMap<String, usize>,
    slots: Vec<Slot>,
}

impl Picker {
    /// `entries`: normalised path and declared size of every file entry, in archive order.
    fn new(mode: Mode, entries: Vec<(String, u64)>, entry_max: u64) -> Result<Self> {
        let order = rank_images(entries.iter().map(|(n, _)| n.as_str()), mode == Mode::Comic);
        let found = order.len();
        let usable: Vec<usize> = order
            .into_iter()
            .filter(|&i| entries[i].1 <= entry_max)
            .take(MAX_DECODE_ATTEMPTS)
            .collect();
        if usable.is_empty() {
            return Err(if found > 0 {
                DecodeError::TooLarge
            } else {
                DecodeError::Unsupported
            });
        }
        let mut ranks = HashMap::new();
        for (rank, &i) in usable.iter().enumerate() {
            ranks.entry(entries[i].0.clone()).or_insert(rank);
        }
        let slots = usable.iter().map(|_| Slot::Pending).collect();
        Ok(Self { mode, ranks, slots })
    }

    fn needed(&self) -> usize {
        match self.mode {
            Mode::Sheet => SHEET_MAX_TILES,
            Mode::Comic => 1,
        }
    }

    /// Claims the rank of `name` if it is a candidate that has not been seen yet.
    fn claim(&mut self, name: &str) -> Option<usize> {
        let rank = *self.ranks.get(name)?;
        if matches!(self.slots[rank], Slot::Pending) {
            self.slots[rank] = Slot::InFlight;
            return Some(rank);
        }
        None
    }

    fn is_candidate(&self, name: &str) -> bool {
        self.ranks
            .get(name)
            .is_some_and(|&r| matches!(self.slots[r], Slot::Pending))
    }

    fn fail(&mut self, rank: usize) {
        self.slots[rank] = Slot::Failed;
    }

    /// Decodes the bytes of the entry claimed as `rank`. Plain decode failures only fail
    /// the rank; budget and timeout errors end the search.
    fn offer(&mut self, rank: usize, bytes: &[u8]) -> Result<()> {
        match decode_bytes(bytes) {
            Ok(img) => {
                self.slots[rank] = Slot::Done(match self.mode {
                    Mode::Sheet => shrink_to(img, SHEET_CELL * 2),
                    Mode::Comic => img,
                });
                Ok(())
            }
            Err(e) if e.falls_through() => {
                self.fail(rank);
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// True once the result cannot change any more: enough images were decoded at ranks
    /// that no still-pending rank precedes, or nothing is left to try.
    fn done(&self) -> bool {
        let mut got = 0;
        for slot in &self.slots {
            match slot {
                Slot::Done(_) => {
                    got += 1;
                    if got == self.needed() {
                        return true;
                    }
                }
                Slot::Failed => {}
                Slot::Pending | Slot::InFlight => return false,
            }
        }
        true
    }

    /// Gives up on every entry whose extraction was interrupted.
    fn fail_in_flight(&mut self) {
        for slot in &mut self.slots {
            if matches!(slot, Slot::InFlight) {
                *slot = Slot::Failed;
            }
        }
    }

    fn any_done(&self) -> bool {
        self.slots.iter().any(|s| matches!(s, Slot::Done(_)))
    }

    fn finish(self) -> Result<DynamicImage> {
        let needed = self.needed();
        let mut images = self.slots.into_iter().filter_map(|s| match s {
            Slot::Done(img) => Some(img),
            _ => None,
        });
        match self.mode {
            Mode::Comic => images.next().ok_or(DecodeError::Unsupported),
            Mode::Sheet => compose_sheet(images.take(needed).collect()),
        }
    }
}

// ---------------------------------------------------------------------------------------
// 7-Zip

fn sevenz_error(e: sevenz_rust2::Error) -> DecodeError {
    use sevenz_rust2::Error as E;
    match e {
        E::BadSignature(_)
        | E::UnsupportedVersion { .. }
        | E::PasswordRequired
        | E::MaybeBadPassword(_)
        | E::UnsupportedCompressionMethod(_)
        | E::ExternalUnsupported
        | E::Unsupported(_) => DecodeError::Unsupported,
        E::MaxMemLimited { .. } => DecodeError::TooLarge,
        other => DecodeError::malformed(format!("7z: {other}")),
    }
}

/// Counts what it is given and keeps it when asked to.
struct Collector {
    keep: bool,
    buf: Vec<u8>,
    n: u64,
}

impl Write for Collector {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.n += data.len() as u64;
        if self.keep {
            self.buf.extend_from_slice(data);
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Refuses a block whose declared dictionary / model memory exceeds the per-entry cap.
fn check_block_memory(block: &sevenz_rust2::Block, entry_max: u64) -> Result<()> {
    for coder in &block.coders {
        let p = coder.properties();
        let need: u64 = match coder.encoder_method_id() {
            // LZMA2: one byte, dictionary size 2^(b/2 + 12) or 3 * 2^(b/2 + 11); 40 = 4 GiB - 1.
            [0x21] => match p.first() {
                Some(&40) => u64::from(u32::MAX),
                Some(&b) if b < 40 => (2 | u64::from(b & 1)) << (u32::from(b) / 2 + 11),
                _ => return Err(DecodeError::malformed("7z: bad LZMA2 properties")),
            },
            // LZMA: lc/lp/pb byte, then the little-endian dictionary size.
            [0x03, 0x01, 0x01] => p
                .get(1..5)
                .and_then(|d| d.try_into().ok())
                .map(u32::from_le_bytes)
                .map(u64::from)
                .ok_or_else(|| DecodeError::malformed("7z: bad LZMA properties"))?,
            // PPMd7: order byte, then the little-endian model memory size.
            [0x03, 0x04, 0x01] => p
                .get(1..5)
                .and_then(|d| d.try_into().ok())
                .map(u32::from_le_bytes)
                .map(u64::from)
                .ok_or_else(|| DecodeError::malformed("7z: bad PPMd properties"))?,
            _ => continue,
        };
        if need > entry_max {
            return Err(DecodeError::TooLarge);
        }
    }
    Ok(())
}

fn sevenz(data: &[u8], mode: Mode, budget: Budget) -> Result<DynamicImage> {
    let password = Password::empty();
    let mut source = Cursor::new(data);
    let archive = SevenArchive::read(&mut source, &password).map_err(sevenz_error)?;

    let names: Vec<Option<String>> = archive
        .files
        .iter()
        .take(MAX_ENTRIES)
        .map(|f| (f.has_stream() && !f.is_directory() && f.size() > 0).then(|| normalize(f.name())))
        .collect();
    let entries = archive
        .files
        .iter()
        .zip(&names)
        .filter_map(|(f, n)| Some((n.clone()?, f.size())))
        .collect();
    let mut picker = Picker::new(mode, entries, budget.entry_max)?;

    let mut used = 0u64;
    let mut failure: Option<DecodeError> = None;
    for block_index in 0..archive.blocks.len() {
        if picker.done() || failure.is_some() {
            break;
        }
        let decoder = BlockDecoder::new(1, block_index, &archive, &password, &mut source);
        // Position of the last entry of this block that still matters; a block without
        // one is not decompressed at all.
        let Some(last) = decoder
            .entries()
            .iter()
            .rposition(|e| e.has_stream() && picker.is_candidate(&normalize(e.name())))
        else {
            continue;
        };
        if let Err(e) = check_block_memory(&archive.blocks[block_index], budget.entry_max) {
            failure = Some(e);
            break;
        }
        let mut pos = 0usize;
        let result = decoder.for_each_entries(&mut |entry, reader| {
            let this = pos;
            pos += 1;
            if !entry.has_stream() || entry.size() == 0 {
                return Ok(this < last);
            }
            let claimed = picker.claim(&normalize(entry.name()));
            // A wanted entry is kept; anything else is only passed over, which a solid
            // stream needs to reach a later entry. Both count against the total budget.
            let allowance = budget.total_max.saturating_sub(used);
            let cap = if claimed.is_some() {
                allowance.min(budget.entry_max)
            } else {
                allowance
            };
            let mut sink = Collector {
                keep: claimed.is_some(),
                buf: Vec::new(),
                n: 0,
            };
            // The reader is limited to the declared size by the crate; a stream or checksum
            // error leaves what was read in `sink.n`.
            let read = io::copy(&mut (&mut *reader).take(cap + 1), &mut sink);
            used = used.saturating_add(sink.n);
            if sink.n > cap {
                failure = Some(DecodeError::TooLarge);
                return Ok(false);
            }
            if let Some(rank) = claimed {
                if read.is_err() {
                    // Damaged stream: nothing after this point of the block can be trusted.
                    picker.fail(rank);
                    return Ok(false);
                }
                if let Err(e) = picker.offer(rank, &sink.buf) {
                    failure = Some(e);
                    return Ok(false);
                }
            } else if read.is_err() {
                return Ok(false);
            }
            Ok(this < last && !picker.done())
        });
        if let Err(e) = result {
            // A block that cannot be decoded (unsupported method, damage) is not fatal while
            // another block may still provide an image.
            picker.fail_in_flight();
            let e = sevenz_error(e);
            if !e.falls_through() {
                failure = Some(e);
            }
        }
    }
    if let Some(e) = failure {
        return Err(e);
    }
    picker.finish()
}

// ---------------------------------------------------------------------------------------
// RAR

fn rar_error(e: rars::Error) -> DecodeError {
    use rars::ErrorKind as K;
    match e.kind() {
        K::UnsupportedFormat | K::UnsupportedFeature | K::PasswordRequired | K::BadPassword => {
            DecodeError::Unsupported
        }
        K::ResourceLimit => DecodeError::TooLarge,
        _ => DecodeError::malformed(format!("RAR: {e}")),
    }
}

/// Collects an extracted member in memory, refusing to grow past the per-entry cap.
struct CappedSink(Arc<Mutex<Vec<u8>>>, u64);

impl Write for CappedSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut v = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if (v.len() as u64).saturating_add(buf.len() as u64) > self.1 {
            return Err(io::Error::other("RAR entry exceeds the size cap"));
        }
        v.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn rar_is_solid(a: &RarArchive) -> bool {
    a.as_rar13().is_some_and(|a| a.main.is_solid())
        || a.as_rar15_40().is_some_and(|a| a.main.is_solid())
        || a.as_rar50().is_some_and(|a| a.main.is_solid())
}

fn rar_name(m: &ArchiveMember) -> String {
    normalize(&m.meta.name_lossy())
}

struct RarState {
    picker: Picker,
    /// The member being extracted into the buffer, by rank.
    pending: Option<(usize, Arc<Mutex<Vec<u8>>>)>,
    failure: Option<DecodeError>,
}

impl RarState {
    /// Decodes the member extracted since the previous call, if any.
    fn flush(&mut self) {
        let Some((rank, buf)) = self.pending.take() else {
            return;
        };
        let bytes = std::mem::take(&mut *buf.lock().unwrap_or_else(PoisonError::into_inner));
        if let Err(e) = self.picker.offer(rank, &bytes) {
            self.failure = Some(e);
        }
    }
}

fn rar(data: &[u8], mode: Mode, budget: Budget) -> Result<DynamicImage> {
    if data.len() as u64 > RAR_MAX_INPUT_BYTES {
        return Err(DecodeError::TooLarge);
    }
    let options = ArchiveReadOptions::new()
        .with_max_header_count(RAR_MAX_HEADERS)
        .with_max_header_bytes(RAR_MAX_HEADER_BYTES)
        .with_max_member_output_bytes(budget.entry_max)
        .with_max_total_output_bytes(budget.total_max)
        .with_rar50_dictionary_size_limit(budget.entry_max);
    let archive = ArchiveReader::read_with_options(data, options).map_err(rar_error)?;
    let solid = rar_is_solid(&archive);

    let usable = |m: &ArchiveMember| {
        let meta = &m.meta;
        !meta.is_directory
            && !meta.is_redirection
            && !meta.is_encrypted
            && !meta.is_split_before
            && !meta.is_split_after
    };
    let entries = archive
        .members()
        .filter(usable)
        .take(MAX_ENTRIES)
        .map(|m| (rar_name(&m), m.meta.unpacked_size))
        .collect();
    let state = RefCell::new(RarState {
        picker: Picker::new(mode, entries, budget.entry_max)?,
        pending: None,
        failure: None,
    });

    let outcome = archive.extract_with_control_and_errors(
        options,
        |m| {
            let mut st = state.borrow_mut();
            st.flush();
            if st.failure.is_some() || st.picker.done() {
                return Ok(ExtractionDecision::Stop);
            }
            if !usable(m) {
                return Ok(ExtractionDecision::Skip);
            }
            if let Some(rank) = st.picker.claim(&rar_name(m)) {
                let buf = Arc::new(Mutex::new(Vec::new()));
                st.pending = Some((rank, Arc::clone(&buf)));
                return Ok(ExtractionDecision::Extract(Box::new(CappedSink(
                    buf,
                    budget.entry_max,
                ))));
            }
            // A solid stream has to be decoded through to reach a later member.
            Ok(if solid {
                ExtractionDecision::Extract(Box::new(io::sink()))
            } else {
                ExtractionDecision::Skip
            })
        },
        |_, _| {
            // An independent member that fails (checksum, damage) only fails its rank.
            let mut st = state.borrow_mut();
            if let Some((rank, _)) = st.pending.take() {
                st.picker.fail(rank);
            }
            Ok(ExtractionErrorAction::Continue)
        },
    );
    let mut st = state.into_inner();
    match outcome {
        Ok(_) => st.flush(),
        Err(e) => {
            if let Some((rank, _)) = st.pending.take() {
                st.picker.fail(rank);
            }
            // Solid or resource failures end the walk; whatever was decoded before still
            // makes a thumbnail.
            if st.failure.is_none() && !st.picker.any_done() {
                return Err(rar_error(e));
            }
        }
    }
    if let Some(e) = st.failure {
        return Err(e);
    }
    st.picker.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{GenericImageView, ImageFormat, Rgba, RgbaImage};
    use rars::{ArchiveVersion, Builder};
    use sevenz_rust2::{ArchiveEntry, ArchiveWriter, EncoderMethod, SourceReader};
    use std::io::Cursor;

    const RED: [u8; 4] = [255, 0, 0, 255];
    const GREEN: [u8; 4] = [0, 255, 0, 255];
    const BLUE: [u8; 4] = [0, 0, 255, 255];
    const YELLOW: [u8; 4] = [255, 255, 0, 255];

    fn png(w: u32, h: u32, color: [u8; 4]) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        RgbaImage::from_pixel(w, h, Rgba(color))
            .write_to(&mut out, ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &b in data {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xEDB8_8320 & 0u32.wrapping_sub(crc & 1));
            }
        }
        !crc
    }

    fn png_header_only(w: u32, h: u32) -> Vec<u8> {
        let mut chunk = b"IHDR".to_vec();
        chunk.extend(w.to_be_bytes());
        chunk.extend(h.to_be_bytes());
        chunk.extend([8, 6, 0, 0, 0]);
        let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
        out.extend(13u32.to_be_bytes());
        out.extend(&chunk);
        out.extend(crc32(&chunk).to_be_bytes());
        out
    }

    fn px(img: &DynamicImage, x: u32, y: u32) -> [u8; 4] {
        img.get_pixel(x, y).0
    }

    fn four_images() -> Vec<(&'static str, Vec<u8>)> {
        vec![
            ("pics/img1.png", png(64, 64, RED)),
            ("pics/img2.png", png(64, 64, GREEN)),
            ("pics/img3.png", png(64, 64, BLUE)),
            ("pics/img4.png", png(64, 64, YELLOW)),
        ]
    }

    /// 2x2 contact sheet: tile `i` is centred in cell `i`.
    fn assert_sheet(img: &DynamicImage, colors: [[u8; 4]; 4]) {
        assert_eq!(img.dimensions(), (512, 512));
        let cell = SHEET_CELL;
        for (i, c) in colors.iter().enumerate() {
            let (x, y) = (
                (i as u32 % 2) * cell + cell / 2,
                (i as u32 / 2) * cell + cell / 2,
            );
            assert_eq!(&px(img, x, y), c, "tile {i}");
        }
    }

    fn is_clean_failure(r: Result<DynamicImage>) -> bool {
        matches!(
            r,
            Err(DecodeError::TooLarge | DecodeError::Malformed(_) | DecodeError::Unsupported)
        )
    }

    // ---- 7z -----------------------------------------------------------------------------

    fn sevenz_writer() -> ArchiveWriter<Cursor<Vec<u8>>> {
        let mut w = ArchiveWriter::new(Cursor::new(Vec::new())).unwrap();
        w.set_content_methods(vec![EncoderMethod::LZMA2.into()]);
        w
    }

    fn sevenz_non_solid(files: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut w = sevenz_writer();
        for (name, data) in files {
            w.push_archive_entry(ArchiveEntry::new_file(name), Some(&data[..]))
                .unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    fn sevenz_solid(files: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut w = sevenz_writer();
        let entries = files
            .iter()
            .map(|(n, _)| ArchiveEntry::new_file(n))
            .collect();
        let readers = files
            .iter()
            .map(|(_, d)| SourceReader::new(&d[..]))
            .collect();
        w.push_archive_entries(entries, readers).unwrap();
        w.finish().unwrap().into_inner()
    }

    /// Recomputes the 7z start-header and next-header CRCs after a patch.
    fn fix_7z_crcs(a: &mut [u8]) {
        if a.len() < 32 {
            return;
        }
        let off = u64::from_le_bytes(a[12..20].try_into().unwrap()) as usize;
        let len = u64::from_le_bytes(a[20..28].try_into().unwrap()) as usize;
        let Some(header) = off
            .checked_add(32)
            .and_then(|s| Some(s..s.checked_add(len)?))
            .and_then(|r| a.get(r))
        else {
            return;
        };
        let crc = crc32(header);
        a[28..32].copy_from_slice(&crc.to_le_bytes());
        let start = crc32(&a[12..32]);
        a[8..12].copy_from_slice(&start.to_le_bytes());
    }

    #[test]
    fn sevenz_contact_sheet_non_solid_and_solid() {
        let mut files = four_images();
        files.insert(0, ("pics/notes.txt", b"not an image".to_vec()));
        let expected = [RED, GREEN, BLUE, YELLOW];
        assert_sheet(&extract(&sevenz_non_solid(&files), "7z").unwrap(), expected);
        assert_sheet(&extract(&sevenz_solid(&files), "7z").unwrap(), expected);
        // Archive order differs from natural order; the sheet follows the latter.
        let mut shuffled = four_images();
        shuffled.reverse();
        shuffled.insert(1, ("pics/img10.png", png(64, 64, [0, 0, 0, 255])));
        let sheet = extract(&sevenz_solid(&shuffled), "7z").unwrap();
        assert_sheet(&sheet, expected);
        let sheet = extract(&sevenz_non_solid(&shuffled), "7z").unwrap();
        assert_sheet(&sheet, expected);
    }

    #[test]
    fn sevenz_single_image_is_not_tiled() {
        let a = sevenz_solid(&[("only.png", png(40, 30, BLUE))]);
        let img = extract(&a, "7z").unwrap();
        assert_eq!(img.dimensions(), (40, 30));
        assert_eq!(px(&img, 20, 15), BLUE);
    }

    #[test]
    fn cb7_first_page_prefers_cover_then_natural_order() {
        let files = vec![
            ("p10.png", png(10, 10, RED)),
            ("p2.png", png(20, 10, GREEN)),
            ("p1.png", png(30, 10, BLUE)),
        ];
        for a in [sevenz_solid(&files), sevenz_non_solid(&files)] {
            assert_eq!(extract(&a, "cb7").unwrap().dimensions(), (30, 10));
        }
        let mut with_cover = files.clone();
        with_cover.push(("zz_cover.png", png(5, 40, YELLOW)));
        for a in [sevenz_solid(&with_cover), sevenz_non_solid(&with_cover)] {
            let img = extract(&a, "cb7").unwrap();
            assert_eq!(img.dimensions(), (5, 40));
            assert_eq!(px(&img, 2, 20), YELLOW);
        }
    }

    #[test]
    fn sevenz_decoding_stops_once_the_result_is_settled() {
        // The last entry's packed bytes are damaged; it is never needed for a comic and only
        // costs its own tile in a sheet.
        let files = vec![("a.png", png(16, 12, RED)), ("z.png", png(8, 8, GREEN))];
        let mut a = sevenz_non_solid(&files);
        let next = u64::from_le_bytes(a[12..20].try_into().unwrap()) as usize;
        let last_packed = 32 + next - 3;
        a[last_packed] ^= 0xFF;
        assert_eq!(extract(&a, "cb7").unwrap().dimensions(), (16, 12));
        assert_eq!(extract(&a, "7z").unwrap().dimensions(), (16, 12));
    }

    #[test]
    fn sevenz_skips_blocks_without_candidates_and_passes_over_solid_data() {
        let mut files = vec![("big.bin", vec![0u8; 300_000])];
        files.extend(four_images());
        let a = sevenz_solid(&files);
        assert_sheet(&extract(&a, "7z").unwrap(), [RED, GREEN, BLUE, YELLOW]);
        // Solid data in front of the wanted entry counts against the total budget.
        let tight = Budget {
            entry_max: 1 << 26,
            total_max: 100_000,
        };
        assert_eq!(extract_with(&a, "7z", tight), Err(DecodeError::TooLarge));
        // Same archive, budget large enough.
        let ok = Budget {
            entry_max: 1 << 26,
            total_max: 1 << 21,
        };
        assert!(extract_with(&a, "7z", ok).is_ok());
        // An image larger than the per-entry cap is refused before anything is decoded.
        let big = sevenz_non_solid(&[("big.png", png(512, 512, RED))]);
        let small_entry = Budget {
            entry_max: 100,
            total_max: 1 << 20,
        };
        assert_eq!(
            extract_with(&big, "7z", small_entry),
            Err(DecodeError::TooLarge)
        );
    }

    #[test]
    fn sevenz_dictionary_bomb_is_refused_before_allocation() {
        let mut a = sevenz_non_solid(&[("a.png", png(8, 8, RED))]);
        // Coder record: flags (id size 1 | has properties), id 0x21 (LZMA2), props size 1, props.
        let at = a
            .windows(3)
            .position(|w| w == [0x21, 0x21, 0x01])
            .expect("LZMA2 coder record");
        a[at + 3] = 40; // 4 GiB - 1 dictionary
        fix_7z_crcs(&mut a);
        assert_eq!(extract(&a, "7z"), Err(DecodeError::TooLarge));
    }

    #[test]
    fn sevenz_unknown_method_is_unsupported() {
        let mut a = sevenz_non_solid(&[("a.png", png(8, 8, RED))]);
        let at = a.windows(3).position(|w| w == [0x21, 0x21, 0x01]).unwrap();
        a[at + 1] = 0x7E;
        fix_7z_crcs(&mut a);
        assert_eq!(extract(&a, "7z"), Err(DecodeError::Unsupported));
    }

    #[test]
    fn sevenz_hostile_inputs_fail_cleanly() {
        assert!(matches!(extract(b"", "7z"), Err(DecodeError::Unsupported)));
        assert!(matches!(
            extract(b"7z", "7z"),
            Err(DecodeError::Unsupported)
        ));
        let a = sevenz_solid(&four_images());
        for cut in 0..a.len() {
            assert!(is_clean_failure(extract(&a[..cut], "7z")), "cut {cut}");
        }
        // Every byte flipped (header CRCs catch most; whatever passes must still not panic).
        for i in 0..a.len() {
            let mut bad = a.clone();
            bad[i] ^= 0xFF;
            let _ = extract(&bad, "7z");
            fix_7z_crcs(&mut bad);
            let _ = extract(&bad, "7z");
        }
        // An image declaring 65535 x 65535 pixels.
        let bomb = sevenz_non_solid(&[("a.png", png_header_only(65535, 65535))]);
        assert_eq!(extract(&bomb, "7z"), Err(DecodeError::TooLarge));
        // No images at all.
        let none = sevenz_non_solid(&[("a.txt", b"hello".to_vec())]);
        assert!(matches!(
            extract(&none, "7z"),
            Err(DecodeError::Unsupported)
        ));
    }

    // Written by 7-Zip itself (`7z a`) from four 200x150 PNG pages (red, green, blue, yellow)
    // and a text file.
    const SOLID_7Z: &[u8] = include_bytes!("../../tests/fixtures/archives/sheet_solid.7z");
    const NONSOLID_7Z: &[u8] = include_bytes!("../../tests/fixtures/archives/sheet_nonsolid.7z");
    const PPMD_7Z: &[u8] = include_bytes!("../../tests/fixtures/archives/sheet_ppmd.7z");
    const DEFLATE_7Z: &[u8] = include_bytes!("../../tests/fixtures/archives/sheet_deflate.7z");
    const BZIP2_7Z: &[u8] = include_bytes!("../../tests/fixtures/archives/sheet_bzip2.7z");

    #[test]
    fn archives_written_by_7zip_itself() {
        let page_colors = [
            [220, 40, 40, 255],
            [40, 200, 60, 255],
            [40, 80, 220, 255],
            [240, 200, 30, 255],
        ];
        for (name, a) in [
            ("solid lzma2", SOLID_7Z),
            ("non-solid lzma2", NONSOLID_7Z),
            ("ppmd", PPMD_7Z),
            ("deflate", DEFLATE_7Z),
        ] {
            let sheet = extract(a, "7z").unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_sheet(&sheet, page_colors);
            let page = extract(a, "cb7").unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(page.dimensions(), (200, 150), "{name}");
            assert_eq!(px(&page, 100, 75), page_colors[0], "{name}");
        }
        // BZip2 is not compiled in (its Rust port is under a license outside the allow-list).
        assert_eq!(extract(BZIP2_7Z, "7z"), Err(DecodeError::Unsupported));
    }
    #[test]
    fn zip_bytes_behind_an_archive_extension_use_the_zip_reader() {
        use std::io::Write;
        let mut cur = Cursor::new(Vec::new());
        {
            let mut z = zip::ZipWriter::new(&mut cur);
            let opts = zip::write::SimpleFileOptions::default();
            z.start_file("002.png", opts).unwrap();
            z.write_all(&png(11, 7, GREEN)).unwrap();
            z.start_file("001.png", opts).unwrap();
            z.write_all(&png(9, 5, RED)).unwrap();
            z.finish().unwrap();
        }
        let zip = cur.into_inner();
        for ext in ["cbr", "cb7"] {
            assert_eq!(extract(&zip, ext).unwrap().dimensions(), (9, 5), "{ext}");
        }
        // Two images in a plain ".rar": the ZIP contact sheet (two cells side by side).
        assert_eq!(extract(&zip, "rar").unwrap().dimensions(), (512, 256));
    }

    // ---- RAR ----------------------------------------------------------------------------

    fn vint(mut v: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(b);
                return out;
            }
            out.push(b | 0x80);
        }
    }

    /// A RAR 5 block: CRC32 of everything after the CRC field, header size, then `body`.
    fn rar5_block(body: &[u8]) -> Vec<u8> {
        let size = vint(body.len() as u64);
        let mut covered = size.clone();
        covered.extend(body);
        let mut out = crc32(&covered).to_le_bytes().to_vec();
        out.extend(covered);
        out
    }

    /// A RAR 5 archive of stored (method 0) files, assembled by hand from the format layout.
    fn rar5_stored(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = b"Rar!\x1a\x07\x01\x00".to_vec();
        // Main archive header: type 1, no flags, no archive flags.
        let mut main = vint(1);
        main.extend(vint(0));
        main.extend(vint(0));
        out.extend(rar5_block(&main));
        for (name, data) in files {
            let mut h = vint(2); // file header
            h.extend(vint(0x02)); // header flags: data area follows
            h.extend(vint(data.len() as u64)); // packed size
            h.extend(vint(0x04)); // file flags: CRC32 present
            h.extend(vint(data.len() as u64)); // unpacked size
            h.extend(vint(0x20)); // attributes
            h.extend(crc32(data).to_le_bytes());
            h.extend(vint(0)); // compression info: version 0, stored
            h.extend(vint(0)); // host OS
            h.extend(vint(name.len() as u64));
            h.extend(name.as_bytes());
            out.extend(rar5_block(&h));
            out.extend(*data);
        }
        // End of archive header: type 5, no flags, no end flags.
        let mut end = vint(5);
        end.extend(vint(0));
        end.extend(vint(0));
        out.extend(rar5_block(&end));
        out
    }

    fn rar_built(
        version: ArchiveVersion,
        solid: bool,
        store: bool,
        files: &[(&str, Vec<u8>)],
    ) -> Vec<u8> {
        let mut b = Builder::new(version).solid(solid).store(store);
        for (name, data) in files {
            b.add_bytes(name.as_bytes().to_vec(), data.clone(), None, None)
                .unwrap();
        }
        b.to_bytes().unwrap()
    }

    #[test]
    fn rar5_stored_hand_assembled_contact_sheet() {
        let imgs = four_images();
        let notes: &[u8] = b"read me";
        let mut files: Vec<(&str, &[u8])> = vec![("pics/notes.txt", notes)];
        files.extend(imgs.iter().map(|(n, d)| (*n, d.as_slice())));
        let a = rar5_stored(&files);
        assert_sheet(&extract(&a, "rar").unwrap(), [RED, GREEN, BLUE, YELLOW]);
        // Comic: the first page.
        let img = extract(&a, "cbr").unwrap();
        assert_eq!(img.dimensions(), (64, 64));
        assert_eq!(px(&img, 10, 10), RED);
    }

    #[test]
    fn rar_every_family_solid_and_not() {
        let mut files = vec![("docs/readme.txt", vec![b'x'; 20_000])];
        files.extend(four_images());
        for version in [
            ArchiveVersion::Rar50,
            ArchiveVersion::Rar29,
            ArchiveVersion::Rar20,
        ] {
            let solids: &[bool] = if version == ArchiveVersion::Rar50 {
                &[false, true]
            } else {
                &[false]
            };
            for &solid in solids {
                for store in [false, true] {
                    let a = rar_built(version, solid, store, &files);
                    let img = extract(&a, "rar")
                        .unwrap_or_else(|e| panic!("{version:?} solid={solid} store={store}: {e}"));
                    assert_sheet(&img, [RED, GREEN, BLUE, YELLOW]);
                }
            }
        }
    }

    #[test]
    fn cbr_first_page_prefers_cover_then_natural_order() {
        let files = vec![
            ("p10.png", png(10, 10, RED)),
            ("p2.png", png(20, 10, GREEN)),
            ("p1.png", png(30, 10, BLUE)),
            ("zz_cover.png", png(5, 40, YELLOW)),
        ];
        for solid in [false, true] {
            let a = rar_built(ArchiveVersion::Rar50, solid, false, &files);
            let img = extract(&a, "cbr").unwrap();
            assert_eq!(img.dimensions(), (5, 40), "solid={solid}");
            assert_eq!(px(&img, 2, 20), YELLOW);
            let a = rar_built(ArchiveVersion::Rar50, solid, false, &files[..3]);
            assert_eq!(extract(&a, "cbr").unwrap().dimensions(), (30, 10));
        }
    }

    #[test]
    fn encrypted_rar_is_unsupported() {
        let mut b = Builder::new(ArchiveVersion::Rar50).password(Some(b"secret".to_vec()));
        b.add_bytes(b"a.png".to_vec(), png(8, 8, RED), None, None)
            .unwrap();
        assert_eq!(
            extract(&b.to_bytes().unwrap(), "rar"),
            Err(DecodeError::Unsupported)
        );
        let mut b = Builder::new(ArchiveVersion::Rar50)
            .password(Some(b"secret".to_vec()))
            .header_encryption(true);
        b.add_bytes(b"a.png".to_vec(), png(8, 8, RED), None, None)
            .unwrap();
        assert_eq!(
            extract(&b.to_bytes().unwrap(), "cbr"),
            Err(DecodeError::Unsupported)
        );
    }

    #[test]
    fn rar_budgets_and_hostile_inputs() {
        assert!(matches!(extract(b"", "rar"), Err(DecodeError::Unsupported)));
        assert!(extract(b"Rar!\x1a\x07\x01\x00", "rar").is_err());
        let imgs = four_images();
        let mut files: Vec<(&str, &[u8])> = Vec::new();
        files.extend(imgs.iter().map(|(n, d)| (*n, d.as_slice())));
        let a = rar5_stored(&files);
        for cut in 0..a.len() {
            // A cut after some members still yields the images before it; never a panic.
            let _ = extract(&a[..cut], "rar");
        }
        for i in 0..a.len() {
            let mut bad = a.clone();
            bad[i] ^= 0xFF;
            let _ = extract(&bad, "rar");
        }
        // Solid stream whose leading member expands past the total budget.
        let mut solid = vec![("a.txt", vec![0u8; 4 << 20])];
        solid.extend(four_images());
        let a = rar_built(ArchiveVersion::Rar50, true, false, &solid);
        let tight = Budget {
            entry_max: 256 << 20,
            total_max: 1 << 20,
        };
        assert_eq!(extract_with(&a, "rar", tight), Err(DecodeError::TooLarge));
        assert!(extract_with(&a, "rar", Budget::default()).is_ok());
        // A member larger than the per-entry cap.
        let big = rar_built(
            ArchiveVersion::Rar50,
            false,
            true,
            &[("a.png", png(256, 256, RED))],
        );
        let small_entry = Budget {
            entry_max: 100,
            total_max: 1 << 20,
        };
        assert_eq!(
            extract_with(&big, "rar", small_entry),
            Err(DecodeError::TooLarge)
        );
        // A member whose header lies about its size: truncated data still fails cleanly.
        let mut lying = rar5_stored(&[("a.png", &imgs[0].1)]);
        let at = lying.len() / 2;
        lying.truncate(at);
        assert!(is_clean_failure(extract(&lying, "rar")));
        // An image declaring 65535 x 65535 pixels.
        let header = png_header_only(65535, 65535);
        let bomb = rar5_stored(&[("a.png", &header)]);
        assert_eq!(extract(&bomb, "rar"), Err(DecodeError::TooLarge));
        // Only non-images inside.
        let none = rar5_stored(&[("a.txt", b"hi")]);
        assert!(matches!(
            extract(&none, "rar"),
            Err(DecodeError::Unsupported)
        ));
    }
}
