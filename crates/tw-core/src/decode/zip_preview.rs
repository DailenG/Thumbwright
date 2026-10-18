//! ZIP-based containers: embedded previews of packages and documents, ebook/comic
//! covers, and a contact sheet for plain `.zip` archives.
//!
//! REQ-SEC-1: every entry is checked against [`limits::ARCHIVE_ENTRY_MAX_BYTES`] by its
//! declared size and read through `take(limit + 1)` so a lying header is caught; the
//! bytes read across all entries are capped by [`limits::ARCHIVE_TOTAL_MAX_BYTES`];
//! at most [`MAX_ENTRIES`] entries are inspected.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::io::{Cursor, Read};

use image::{DynamicImage, RgbaImage, imageops};
use zip::ZipArchive;
use zip::result::ZipError;

use super::android::{self, Ref};
use super::{DecodeError, Result, decode_bytes};
use crate::limits;

pub const EXTENSIONS: &[&str] = &[
    // Image / design / CAD / package files (embedded preview).
    "ora",
    "kra",
    "3mf",
    "fcstd",
    "fcbak",
    "fcstd1",
    "mcworld",
    "mctemplate",
    "mcpack",
    "mcaddon",
    "sketch",
    "procreate",
    "xd",
    "pxo",
    "f3d",
    "nupkg",
    "vsix",
    // Documents.
    "odt",
    "ods",
    "odp",
    "odg",
    "odf",
    "ott",
    "ots",
    "otp",
    "docx",
    "docm",
    "dotx",
    "dotm",
    "xlsx",
    "xlsm",
    "xlsb",
    "xltx",
    "xltm",
    "pptx",
    "pptm",
    "potx",
    "potm",
    "ppsx",
    "ppsm",
    "vsdx",
    "vsdm",
    "vstx",
    "vssx",
    "key",
    "pages",
    "numbers",
    "xmind",
    "ggb",
    // Ebooks and comics.
    "epub",
    "cbz",
    "phz",
    "fbz",
    // Android packages.
    "apk",
    "apks",
    "xapk",
    "apkm",
    // Archives.
    "zip",
];

/// Central-directory entries considered per archive.
pub(super) const MAX_ENTRIES: usize = 4096;
/// Images tried before giving up on "first decodable image" searches.
pub(super) const MAX_DECODE_ATTEMPTS: usize = 16;
/// EPUB spine documents scanned for a first image.
const MAX_SPINE_DOCS: usize = 8;
/// Contact sheet: 2x2 cells of this edge (sheet at most 512x512).
pub(super) const SHEET_CELL: u32 = 256;
pub(super) const SHEET_MAX_TILES: usize = 4;

const OPC_THUMBNAIL_REL: &str = "/metadata/thumbnail";

pub fn extract(data: &[u8], ext: &str) -> Result<DynamicImage> {
    extract_with(data, ext, Budget::default())
}

/// Image entry named (any directory) one of `names` in a ZIP package, under the standard
/// budget. Lets other decoders reuse the bounded reader for ZIP-wrapped formats.
pub(super) fn extract_thumbnail(data: &[u8], names: &[&str]) -> Result<DynamicImage> {
    Archive::open(data, Budget::default())?.by_basename(names)
}

/// Still-image extensions of an Apple Live Photo bundle (`.livp`: the still plus a QuickTime
/// movie).
const LIVP_STILLS: &[&str] = &["heic", "heif", "jpg", "jpeg", "png"];

/// The still image bytes of a `.livp` bundle (shallowest HEIC/HEIF/JPEG/PNG entry), read
/// under the entry and total budgets, with its lower-case extension. The caller decodes it.
pub(super) fn livp_still(data: &[u8]) -> Result<(Vec<u8>, &'static str)> {
    let mut a = Archive::open(data, Budget::default())?;
    let pick = (0..a.entries.len())
        .filter_map(|pos| {
            let name = &a.entries[pos].name;
            let lower = name.to_lowercase();
            if lower.starts_with("__macosx/") || basename(name).starts_with("._") {
                return None;
            }
            let ext = *LIVP_STILLS.iter().find(|e| **e == extension(&lower))?;
            Some((name.matches('/').count(), pos, ext))
        })
        .min();
    let (_, pos, ext) = pick.ok_or(DecodeError::Unsupported)?;
    Ok((a.read(pos)?, ext))
}

/// Decompression budget (REQ-SEC-1); tests shrink it.
#[derive(Clone, Copy, Debug)]
struct Budget {
    entry_max: u64,
    total_max: u64,
    max_entries: usize,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            entry_max: limits::ARCHIVE_ENTRY_MAX_BYTES,
            total_max: limits::ARCHIVE_TOTAL_MAX_BYTES,
            max_entries: MAX_ENTRIES,
        }
    }
}

fn extract_with(data: &[u8], ext: &str, budget: Budget) -> Result<DynamicImage> {
    let ext = ext.to_ascii_lowercase();
    if !EXTENSIONS.contains(&ext.as_str()) {
        return Err(DecodeError::Unsupported);
    }
    // Application packages routinely hold tens of thousands of files, and the entries that
    // matter (`resources.arsc`, `res/...`) sit anywhere in the directory.
    let budget = if matches!(ext.as_str(), "apk" | "apks" | "xapk" | "apkm") {
        Budget {
            max_entries: budget.max_entries.max(APK_MAX_ENTRIES),
            ..budget
        }
    } else {
        budget
    };
    let mut a = Archive::open(data, budget)?;
    match ext.as_str() {
        "ora" => a.fixed(&["mergedimage.png", "Thumbnails/thumbnail.png"]),
        "kra" => a.fixed(&["mergedimage.png", "preview.png"]),
        "fcstd" | "fcbak" | "fcstd1" => a.fixed(&["thumbnails/Thumbnail.png"]),
        "odt" | "ods" | "odp" | "odg" | "odf" | "ott" | "ots" | "otp" => {
            a.fixed(&["Thumbnails/thumbnail.png"])
        }
        "3mf" => a.opc(&["Metadata/thumbnail.png", "3D/Metadata/thumbnail.png"]),
        "docx" | "docm" | "dotx" | "dotm" | "xlsx" | "xlsm" | "xlsb" | "xltx" | "xltm" | "pptx"
        | "pptm" | "potx" | "potm" | "ppsx" | "ppsm" | "vsdx" | "vsdm" | "vstx" | "vssx" => {
            a.opc(&[
                "docProps/thumbnail.jpeg",
                "docProps/thumbnail.jpg",
                "docProps/thumbnail.png",
            ])
        }
        "key" | "pages" | "numbers" => a.fixed(&[
            "preview.jpg",
            "QuickLook/Thumbnail.jpg",
            "preview-web.jpg",
            "preview-micro.jpg",
        ]),
        "xmind" => a.fixed(&["Thumbnails/thumbnail.png", "Thumbnails/thumbnail.jpg"]),
        "ggb" => a.fixed(&["geogebra_thumbnail.png"]),
        "mcworld" | "mctemplate" => {
            a.by_basename(&["world_icon.jpeg", "world_icon.jpg", "world_icon.png"])
        }
        "mcpack" | "mcaddon" => a.by_basename(&["pack_icon.png"]),
        "sketch" => a.fixed(&["previews/preview.png"]),
        "procreate" => a.fixed(&["QuickLook/Thumbnail.png"]),
        "xd" => a.fixed(&["preview.png", "thumbnail.png"]),
        "pxo" => a.fixed(&["preview.png"]),
        "f3d" => a.by_name_containing(&["thumbnail", "preview"]),
        "nupkg" => a.nupkg(),
        "vsix" => a.vsix(),
        "epub" => a.epub(),
        "cbz" | "phz" => a.comic(),
        "fbz" => a.fbz(),
        "apk" => a.apk(),
        "apks" | "xapk" | "apkm" => a.bundle(),
        _ => a.contact_sheet(),
    }
}

struct EntryInfo {
    index: usize,
    /// Normalised path (forward slashes, no leading `/` or `./`), original case.
    name: String,
}

struct Archive<'a> {
    zip: ZipArchive<Cursor<&'a [u8]>>,
    entries: Vec<EntryInfo>,
    by_name: HashMap<String, usize>,
    budget: Budget,
    used: u64,
}

fn zip_error(e: ZipError) -> DecodeError {
    match e {
        ZipError::UnsupportedArchive(_) | ZipError::CompressionMethodNotSupported(_) => {
            DecodeError::Unsupported
        }
        other => DecodeError::malformed(format!("ZIP: {other}")),
    }
}

impl<'a> Archive<'a> {
    fn open(data: &'a [u8], budget: Budget) -> Result<Self> {
        let zip = ZipArchive::new(Cursor::new(data)).map_err(|e| {
            if data.starts_with(b"PK") {
                zip_error(e)
            } else {
                DecodeError::Unsupported
            }
        })?;
        let mut entries = Vec::new();
        let mut by_name = HashMap::new();
        for index in 0..zip.len().min(budget.max_entries) {
            let Ok(entry) = zip.by_index_data(index) else {
                continue;
            };
            if entry.is_dir() {
                continue;
            }
            let Ok(raw) = entry.name() else { continue };
            let name = normalize(&raw);
            if name.is_empty() {
                continue;
            }
            by_name.entry(name.to_lowercase()).or_insert(entries.len());
            entries.push(EntryInfo { index, name });
        }
        Ok(Self {
            zip,
            entries,
            by_name,
            budget,
            used: 0,
        })
    }

    fn find(&self, path: &str) -> Option<usize> {
        self.by_name.get(&normalize(path).to_lowercase()).copied()
    }

    /// Reads entry `pos` (index into `entries`) under the decompression budget.
    fn read(&mut self, pos: usize) -> Result<Vec<u8>> {
        let index = self.entries[pos].index;
        // Declared size first (central directory), before any decompression starts.
        let declared = self.zip.by_index_data(index).map_err(zip_error)?.size();
        if declared > self.budget.entry_max
            || self.used.saturating_add(declared) > self.budget.total_max
        {
            return Err(DecodeError::TooLarge);
        }
        let entry = self.zip.by_index(index).map_err(zip_error)?;
        let mut buf = Vec::with_capacity(usize::try_from(declared.min(1 << 20)).unwrap_or(0));
        // One byte past the remaining allowance distinguishes "fits" from "over budget".
        let allowance = self
            .budget
            .entry_max
            .min(self.budget.total_max.saturating_sub(self.used));
        let read = entry.take(allowance + 1).read_to_end(&mut buf);
        // A failed read (e.g. CRC mismatch) still decompressed `buf`: charge it first.
        let got = buf.len() as u64;
        self.used = self.used.saturating_add(got);
        read.map_err(|e| DecodeError::malformed(format!("ZIP entry: {e}")))?;
        if got > self.budget.entry_max || self.used > self.budget.total_max {
            return Err(DecodeError::TooLarge);
        }
        Ok(buf)
    }

    fn read_text(&mut self, pos: usize) -> Result<String> {
        Ok(bytes_to_text(&self.read(pos)?))
    }

    fn decode(&mut self, pos: usize) -> Result<DynamicImage> {
        decode_bytes(&self.read(pos)?)
    }

    /// First candidate (entry positions, in order) that decodes. Budget violations stop
    /// the search; plain decode failures move on.
    fn first_decodable(
        &mut self,
        candidates: impl IntoIterator<Item = usize>,
    ) -> Result<DynamicImage> {
        let mut last = DecodeError::Unsupported;
        let mut seen = Vec::new();
        for pos in candidates {
            if seen.contains(&pos) {
                continue;
            }
            if seen.len() >= MAX_DECODE_ATTEMPTS {
                break;
            }
            seen.push(pos);
            match self.decode(pos) {
                Ok(img) => return Ok(img),
                Err(e) if e.falls_through() => last = e,
                Err(e) => return Err(e),
            }
        }
        Err(last)
    }

    fn fixed(&mut self, names: &[&str]) -> Result<DynamicImage> {
        let found: Vec<usize> = names.iter().filter_map(|n| self.find(n)).collect();
        self.first_decodable(found)
    }

    /// Entries whose file name matches one of `names` at any depth, shallowest first.
    fn by_basename(&mut self, names: &[&str]) -> Result<DynamicImage> {
        let mut found: Vec<(usize, usize, usize)> = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(pos, e)| {
                let base = basename(&e.name).to_lowercase();
                let rank = names.iter().position(|n| *n == base)?;
                Some((e.name.matches('/').count(), rank, pos))
            })
            .collect();
        found.sort();
        self.first_decodable(found.into_iter().map(|f| f.2))
    }

    /// Image entries whose file name contains one of `needles` (lower-case), shallowest
    /// first.
    fn by_name_containing(&mut self, needles: &[&str]) -> Result<DynamicImage> {
        let mut found: Vec<(usize, usize)> = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(pos, e)| {
                let lower = e.name.to_lowercase();
                let base = basename(&lower);
                let wanted = is_image_name(&lower)
                    && !lower.starts_with("__macosx/")
                    && !base.starts_with("._")
                    && needles.iter().any(|n| base.contains(n));
                wanted.then(|| (e.name.matches('/').count(), pos))
            })
            .collect();
        found.sort();
        self.first_decodable(found.into_iter().map(|f| f.1))
    }

    /// Image entries (by extension), skipping macOS metadata, in natural order.
    fn images(&self) -> Vec<usize> {
        rank_images(self.entries.iter().map(|e| e.name.as_str()), false)
    }

    /// OPC packages (OOXML, 3MF): the package thumbnail relationship, then `fallbacks`.
    fn opc(&mut self, fallbacks: &[&str]) -> Result<DynamicImage> {
        let mut candidates = Vec::new();
        if let Some(rels) = self.find("_rels/.rels") {
            let xml = self.read_text(rels)?;
            for tag in tags(&xml).filter(|t| t.is("Relationship") && !t.end) {
                let is_thumb = tag
                    .attr("Type")
                    .is_some_and(|t| t.trim_end_matches('/').ends_with(OPC_THUMBNAIL_REL));
                let external = tag
                    .attr("TargetMode")
                    .is_some_and(|m| m.eq_ignore_ascii_case("External"));
                if let (true, false, Some(target)) = (is_thumb, external, tag.attr("Target")) {
                    let path = resolve("", &target);
                    if !is_metafile(&path) {
                        candidates.extend(self.find(&path));
                    }
                }
            }
        }
        candidates.extend(fallbacks.iter().filter_map(|n| self.find(n)));
        self.first_decodable(candidates)
    }

    fn nupkg(&mut self) -> Result<DynamicImage> {
        let mut candidates = Vec::new();
        let nuspec = (0..self.entries.len())
            .filter(|&p| self.entries[p].name.to_lowercase().ends_with(".nuspec"))
            .min_by_key(|&p| self.entries[p].name.matches('/').count());
        if let Some(pos) = nuspec {
            let xml = self.read_text(pos)?;
            if let Some(icon) = element_text(&xml, "icon") {
                candidates.extend(self.find(&resolve("", &icon)));
            }
        }
        candidates.extend(self.images());
        self.first_decodable(candidates)
    }

    fn vsix(&mut self) -> Result<DynamicImage> {
        let mut candidates = Vec::new();
        if let Some(pos) = self.find("extension.vsixmanifest") {
            let xml = self.read_text(pos)?;
            for name in ["Icon", "PreviewImage"] {
                if let Some(path) = element_text(&xml, name) {
                    candidates.extend(self.find(&resolve("", &path)));
                }
            }
        }
        candidates.extend(self.images());
        self.first_decodable(candidates)
    }

    fn epub(&mut self) -> Result<DynamicImage> {
        let mut candidates: Vec<usize> = Vec::new();
        let opf_path = match self.find("META-INF/container.xml") {
            Some(pos) => {
                let xml = self.read_text(pos)?;
                tags(&xml)
                    .find(|t| t.is("rootfile") && !t.end)
                    .and_then(|t| t.attr("full-path"))
                    .map(|p| resolve("", &p))
            }
            None => None,
        }
        .or_else(|| {
            self.entries
                .iter()
                .find(|e| e.name.to_lowercase().ends_with(".opf"))
                .map(|e| e.name.clone())
        });
        if let Some(opf_pos) = opf_path.as_deref().and_then(|p| self.find(p)) {
            let opf_path = self.entries[opf_pos].name.clone();
            let dir = parent(&opf_path).to_string();
            let xml = self.read_text(opf_pos)?;
            let opf = Opf::parse(&xml, &dir);
            for href in opf.cover_images() {
                candidates.extend(self.find(&href));
            }
            // Cover page documents from the guide, then the spine in reading order.
            let docs: Vec<String> = opf
                .guide_cover
                .iter()
                .filter(|h| !is_image_name(&h.to_lowercase()))
                .cloned()
                .chain(opf.spine_docs())
                .take(MAX_SPINE_DOCS)
                .collect();
            for doc in docs {
                let Some(pos) = self.find(&doc) else { continue };
                let html = self.read_text(pos)?;
                let doc_dir = parent(&doc).to_string();
                let first =
                    tags(&html).find_map(|t| match t.local().to_ascii_lowercase().as_str() {
                        "img" if !t.end => t.attr("src"),
                        "image" if !t.end => t.attr("href"),
                        _ => None,
                    });
                if let Some(src) = first {
                    candidates.extend(self.find(&resolve(&doc_dir, &src)));
                }
            }
            for href in opf.manifest_images() {
                candidates.extend(self.find(&href));
            }
        }
        candidates.extend(self.images());
        self.first_decodable(candidates)
    }

    fn comic(&mut self) -> Result<DynamicImage> {
        let order = rank_images(self.entries.iter().map(|e| e.name.as_str()), true);
        self.first_decodable(order)
    }

    fn fbz(&mut self) -> Result<DynamicImage> {
        let book = (0..self.entries.len())
            .find(|&p| self.entries[p].name.to_lowercase().ends_with(".fb2"))
            .ok_or(DecodeError::Unsupported)?;
        let xml = self.read_text(book)?;
        let bytes = fb2_cover(&xml)?;
        decode_bytes(&bytes)
    }

    // ---- Android packages ----

    /// Launcher icon of an APK: `android:icon` of `<application>` resolved through
    /// `resources.arsc` (highest-density raster, else an adaptive icon's layers composed),
    /// else the documented heuristic ([`Archive::apk_heuristic`]).
    fn apk(&mut self) -> Result<DynamicImage> {
        let manifest = self.find("AndroidManifest.xml");
        let table = self.find("resources.arsc");
        if let Some(m) = manifest {
            let manifest = self.read(m)?;
            let table_bytes = match table {
                Some(t) => self.read(t)?,
                None => Vec::new(),
            };
            let icons = android::manifest_icons(&manifest);
            if let Some(table) = android::Table::parse(&table_bytes) {
                let mut ctx = ApkCtx {
                    table: &table,
                    steps: APK_MAX_STEPS,
                };
                for icon in &icons {
                    if let Some(img) = self.apk_drawable(&mut ctx, icon, 0)? {
                        return Ok(img);
                    }
                }
            } else {
                // No resource table: only a literal path can be followed.
                for icon in &icons {
                    if let Ref::Path(p) = icon
                        && let Some(img) = self.apk_raster(p)?
                    {
                        return Ok(img);
                    }
                }
            }
        }
        self.apk_heuristic()
    }

    fn apk_raster(&mut self, path: &str) -> Result<Option<DynamicImage>> {
        let Some(pos) = self.find(path) else {
            return Ok(None);
        };
        match self.decode(pos) {
            Ok(img) => Ok(Some(img)),
            Err(e) if e.falls_through() => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// The best image for a drawable reference. Each resolution step is counted so that a
    /// hostile resource table (reference cycles, wide fan-out) ends in `None`.
    fn apk_drawable(
        &mut self,
        ctx: &mut ApkCtx<'_, '_>,
        r: &Ref,
        depth: u8,
    ) -> Result<Option<DynamicImage>> {
        if depth > APK_MAX_DEPTH || ctx.steps == 0 {
            return Ok(None);
        }
        ctx.steps -= 1;
        match r {
            Ref::Color(_) => Ok(None),
            Ref::Path(p) if p.to_ascii_lowercase().ends_with(".xml") => {
                self.apk_adaptive(ctx, p, depth)
            }
            Ref::Path(p) => self.apk_raster(p),
            Ref::Res(id) => {
                let mut values = ctx.table.lookup(*id);
                values.truncate(APK_MAX_CONFIGS);
                // nodpi counts as the most detailed, anydpi (vector / adaptive) as the least.
                values.sort_by_key(|v| {
                    std::cmp::Reverse(match v.density {
                        0xFFFE => 0,
                        0xFFFF => 480,
                        0 => 160,
                        d => d,
                    })
                });
                // Plain rasters first (a legacy launcher icon is the finished artwork), then
                // references, then XML (adaptive icons).
                for v in &values {
                    if let android::Value::Path(p) = &v.value
                        && is_image_name(&p.to_ascii_lowercase())
                        && let Some(img) = self.apk_raster(p)?
                    {
                        return Ok(Some(img));
                    }
                }
                for v in &values {
                    if let android::Value::Reference(next) = v.value
                        && let Some(img) = self.apk_drawable(ctx, &Ref::Res(next), depth + 1)?
                    {
                        return Ok(Some(img));
                    }
                }
                for v in &values {
                    if let android::Value::Path(p) = &v.value
                        && p.to_ascii_lowercase().ends_with(".xml")
                        && let Some(img) = self.apk_adaptive(ctx, p, depth + 1)?
                    {
                        return Ok(Some(img));
                    }
                }
                Ok(None)
            }
        }
    }

    /// An `<adaptive-icon>` document: the foreground over the background, cropped to the
    /// 72dp-of-108dp area a launcher shows. Vector layers cannot be rendered, so an icon
    /// without a raster foreground yields `None`.
    fn apk_adaptive(
        &mut self,
        ctx: &mut ApkCtx<'_, '_>,
        path: &str,
        depth: u8,
    ) -> Result<Option<DynamicImage>> {
        let Some(pos) = self.find(path) else {
            return Ok(None);
        };
        let xml = self.read(pos)?;
        let Some((fg, bg)) = android::adaptive_layers(&xml) else {
            return Ok(None);
        };
        let Some(fg) = fg else { return Ok(None) };
        let Some(fg) = self.apk_drawable(ctx, &fg, depth + 1)? else {
            return Ok(None);
        };
        let background = match bg {
            Some(bg) => self.apk_layer(ctx, &bg, depth + 1)?,
            None => None,
        };
        Ok(Some(compose_adaptive(fg, background)))
    }

    /// A background layer: a colour (inline or resource) or a raster.
    fn apk_layer(&mut self, ctx: &mut ApkCtx<'_, '_>, r: &Ref, depth: u8) -> Result<Option<Layer>> {
        match r {
            Ref::Color(c) => return Ok(Some(Layer::Color(*c))),
            Ref::Res(id) => {
                for v in ctx.table.lookup(*id).into_iter().take(APK_MAX_CONFIGS) {
                    match v.value {
                        android::Value::Color(c) => return Ok(Some(Layer::Color(c))),
                        android::Value::Reference(next)
                            if depth < APK_MAX_DEPTH && ctx.steps > 0 =>
                        {
                            ctx.steps -= 1;
                            if let Some(l) = self.apk_layer(ctx, &Ref::Res(next), depth + 1)? {
                                return Ok(Some(l));
                            }
                        }
                        _ => {}
                    }
                }
            }
            Ref::Path(_) => {}
        }
        Ok(self.apk_drawable(ctx, r, depth)?.map(Layer::Image))
    }

    /// Without a usable manifest/resource table: the best `res/{mipmap,drawable}*/` PNG or
    /// WebP named `ic_launcher*`, `icon*` or `app_icon*`. Full-colour files beat round and
    /// foreground-layer variants, higher density (from the directory qualifier) beats lower;
    /// background and monochrome layers are never used.
    fn apk_heuristic(&mut self) -> Result<DynamicImage> {
        let mut found: Vec<(u8, std::cmp::Reverse<u32>, usize)> = Vec::new();
        for (pos, e) in self.entries.iter().enumerate() {
            let lower = e.name.to_lowercase();
            let mut parts = lower.split('/');
            let (Some("res"), Some(dir), Some(file), None) =
                (parts.next(), parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            if !(dir.starts_with("mipmap") || dir.starts_with("drawable"))
                || !matches!(extension(file), "png" | "webp")
            {
                continue;
            }
            let stem = file.rsplit_once('.').map_or(file, |s| s.0);
            if !(stem.starts_with("ic_launcher")
                || stem.starts_with("icon")
                || stem.starts_with("app_icon"))
                || stem.contains("background")
                || stem.contains("monochrome")
            {
                continue;
            }
            let class = if stem.contains("foreground") {
                2
            } else {
                u8::from(stem.contains("round"))
            };
            found.push((class, std::cmp::Reverse(dir_density(dir)), pos));
        }
        found.sort();
        self.first_decodable(found.into_iter().map(|f| f.2))
    }

    /// `.apks` / `.xapk` / `.apkm` bundles: a root icon (`manifest.json` `icon`, `icon.png`),
    /// else the launcher icon of the base APK nested inside (one level only; the nested
    /// archive is read under the same entry cap and what is left of the total budget).
    fn bundle(&mut self) -> Result<DynamicImage> {
        let mut candidates = Vec::new();
        if let Some(m) = self.find("manifest.json") {
            let text = self.read_text(m)?;
            if let Some(icon) = json_string(&text, "icon") {
                candidates.extend(self.find(&resolve("", &icon)));
            }
        }
        candidates.extend(self.find("icon.png"));
        match self.first_decodable(candidates) {
            Ok(img) => return Ok(img),
            Err(e) if e.falls_through() => {}
            Err(e) => return Err(e),
        }
        if self.find("AndroidManifest.xml").is_some() {
            return self.apk();
        }
        let mut nested: Vec<(u8, usize, usize)> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.name.to_lowercase().ends_with(".apk"))
            .map(|(pos, e)| {
                let lower = e.name.to_lowercase();
                let rank = if lower.contains("base") || lower.contains("master") {
                    0
                } else if lower.contains("standalone") {
                    1
                } else {
                    2
                };
                (rank, e.name.matches('/').count(), pos)
            })
            .collect();
        nested.sort();
        let mut last = DecodeError::Unsupported;
        for (_, _, pos) in nested.into_iter().take(APK_MAX_NESTED) {
            let bytes = self.read(pos)?;
            let budget = Budget {
                total_max: self.budget.total_max.saturating_sub(self.used),
                ..self.budget
            };
            let mut inner = Archive::open(&bytes, budget)?;
            let result = inner.apk();
            self.used = self.used.saturating_add(inner.used);
            match result {
                Ok(img) => return Ok(img),
                Err(e) if e.falls_through() => last = e,
                Err(e) => return Err(e),
            }
        }
        Err(last)
    }

    fn contact_sheet(&mut self) -> Result<DynamicImage> {
        let edge = SHEET_CELL * 2;
        let mut tiles: Vec<DynamicImage> = Vec::new();
        for pos in self.images().into_iter().take(MAX_DECODE_ATTEMPTS) {
            if tiles.len() == SHEET_MAX_TILES {
                break;
            }
            match self.decode(pos) {
                Ok(img) => tiles.push(shrink_to(img, edge)),
                Err(e) if e.falls_through() => {}
                Err(e) => return Err(e),
            }
        }
        compose_sheet(tiles)
    }
}

/// Image entries (by extension), skipping macOS metadata, in natural order. With `comic`,
/// images whose file name contains "cover" come first. Returns indices into `names`.
pub(super) fn rank_images<'n>(names: impl IntoIterator<Item = &'n str>, comic: bool) -> Vec<usize> {
    let names: Vec<&str> = names.into_iter().collect();
    let mut v: Vec<usize> = (0..names.len())
        .filter(|&i| {
            let lower = names[i].to_lowercase();
            !lower.starts_with("__macosx/")
                && !basename(names[i]).starts_with("._")
                && is_image_name(&lower)
        })
        .collect();
    v.sort_by(|&a, &b| natural_cmp(names[a], names[b]));
    if comic {
        let (covers, pages): (Vec<usize>, Vec<usize>) = v
            .into_iter()
            .partition(|&i| basename(names[i]).to_lowercase().contains("cover"));
        return covers.into_iter().chain(pages).collect();
    }
    v
}

/// Lays up to [`SHEET_MAX_TILES`] tiles out on a 2-column grid of [`SHEET_CELL`] cells (a
/// single tile is returned as is).
pub(super) fn compose_sheet(mut tiles: Vec<DynamicImage>) -> Result<DynamicImage> {
    tiles.truncate(SHEET_MAX_TILES);
    match tiles.len() {
        0 => Err(DecodeError::Unsupported),
        1 => Ok(tiles.remove(0)),
        n => {
            let cols = n.min(2) as u32;
            let rows = n.div_ceil(2) as u32;
            let mut sheet = RgbaImage::new(cols * SHEET_CELL, rows * SHEET_CELL);
            for (i, tile) in tiles.into_iter().enumerate() {
                let tile = shrink_to(tile, SHEET_CELL).to_rgba8();
                let (col, row) = ((i % 2) as u32, (i / 2) as u32);
                let x = col * SHEET_CELL + (SHEET_CELL - tile.width()) / 2;
                let y = row * SHEET_CELL + (SHEET_CELL - tile.height()) / 2;
                imageops::overlay(&mut sheet, &tile, i64::from(x), i64::from(y));
            }
            Ok(DynamicImage::ImageRgba8(sheet))
        }
    }
}

// ---------------------------------------------------------------------------------------
// Android packages

/// Reference hops followed while resolving a drawable.
const APK_MAX_DEPTH: u8 = 4;
/// Resolution steps (lookups and file reads) per APK.
const APK_MAX_STEPS: u32 = 64;
/// Configurations considered per resource.
const APK_MAX_CONFIGS: usize = 32;
/// Nested APKs tried inside a bundle.
const APK_MAX_NESTED: usize = 2;
/// Central-directory entries considered in an application package.
const APK_MAX_ENTRIES: usize = 1 << 16;
/// Longest side of the layers an adaptive icon is composed from.
const APK_LAYER_EDGE: u32 = 1024;

struct ApkCtx<'t, 'd> {
    table: &'t android::Table<'d>,
    steps: u32,
}

/// One layer of an adaptive icon.
enum Layer {
    Color(u32),
    Image(DynamicImage),
}

/// Foreground over background (a colour, an image scaled to the foreground, or nothing),
/// cropped to the central 2/3 (the 72dp visible area of the 108dp canvas).
fn compose_adaptive(fg: DynamicImage, bg: Option<Layer>) -> DynamicImage {
    let fg = shrink_to(fg, APK_LAYER_EDGE).to_rgba8();
    let (w, h) = fg.dimensions();
    let mut canvas = match bg {
        Some(Layer::Color(argb)) => {
            let [a, r, g, b] = argb.to_be_bytes();
            RgbaImage::from_pixel(w, h, image::Rgba([r, g, b, a]))
        }
        Some(Layer::Image(img)) => {
            let img = shrink_to(img, APK_LAYER_EDGE).to_rgba8();
            imageops::resize(&img, w, h, imageops::FilterType::Triangle)
        }
        None => RgbaImage::new(w, h),
    };
    imageops::overlay(&mut canvas, &fg, 0, 0);
    let (cw, ch) = ((w * 2 / 3).max(1), (h * 2 / 3).max(1));
    DynamicImage::ImageRgba8(
        imageops::crop_imm(&canvas, (w - cw) / 2, (h - ch) / 2, cw, ch).to_image(),
    )
}

/// Approximate density (dpi) from a resource directory's qualifiers.
fn dir_density(dir: &str) -> u32 {
    [
        ("xxxhdpi", 640),
        ("xxhdpi", 480),
        ("xhdpi", 320),
        ("tvdpi", 213),
        ("hdpi", 240),
        ("mdpi", 160),
        ("ldpi", 120),
        ("nodpi", 320),
    ]
    .iter()
    .find(|(q, _)| dir.contains(q))
    .map_or(160, |&(_, d)| d)
}

/// The string value of the first `"key": "value"` pair in a JSON document (enough for the
/// flat `manifest.json` of an XAPK).
fn json_string(text: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let mut from = 0;
    while let Some(i) = text.get(from..)?.find(&needle) {
        let rest = text.get(from + i + needle.len()..)?.trim_start();
        from += i + needle.len();
        let Some(rest) = rest.strip_prefix(':') else {
            continue;
        };
        let Some(rest) = rest.trim_start().strip_prefix('"') else {
            continue;
        };
        let mut out = String::new();
        let mut chars = rest.chars();
        while let Some(c) = chars.next() {
            match c {
                '"' => return Some(out),
                '\\' => out.push(chars.next()?),
                c => out.push(c),
            }
            if out.len() > 1024 {
                return None;
            }
        }
        return None;
    }
    None
}

// ---------------------------------------------------------------------------------------
// EPUB package document

struct ManifestItem {
    id: String,
    href: String,
    media_type: String,
    properties: String,
}

struct Opf {
    items: Vec<ManifestItem>,
    meta_cover: Option<String>,
    guide_cover: Vec<String>,
    spine: Vec<String>,
}

impl Opf {
    fn parse(xml: &str, dir: &str) -> Self {
        let mut opf = Opf {
            items: Vec::new(),
            meta_cover: None,
            guide_cover: Vec::new(),
            spine: Vec::new(),
        };
        for t in tags(xml).filter(|t| !t.end) {
            match t.local().to_ascii_lowercase().as_str() {
                "item" => {
                    if let Some(href) = t.attr("href") {
                        opf.items.push(ManifestItem {
                            id: t.attr("id").unwrap_or_default(),
                            href: resolve(dir, &href),
                            media_type: t
                                .attr("media-type")
                                .unwrap_or_default()
                                .to_ascii_lowercase(),
                            properties: t.attr("properties").unwrap_or_default(),
                        });
                    }
                }
                "meta"
                    if t.attr("name")
                        .is_some_and(|n| n.eq_ignore_ascii_case("cover")) =>
                {
                    opf.meta_cover = t.attr("content");
                }
                "reference"
                    if t.attr("type")
                        .is_some_and(|n| n.eq_ignore_ascii_case("cover")) =>
                {
                    opf.guide_cover
                        .extend(t.attr("href").map(|h| resolve(dir, &h)));
                }
                "itemref" => opf.spine.extend(t.attr("idref")),
                _ => {}
            }
        }
        opf
    }

    fn item(&self, id: &str) -> Option<&ManifestItem> {
        self.items.iter().find(|i| i.id == id)
    }

    /// Declared cover images, most explicit first.
    fn cover_images(&self) -> Vec<String> {
        let mut out = Vec::new();
        out.extend(
            self.items
                .iter()
                .filter(|i| i.properties.split_whitespace().any(|p| p == "cover-image"))
                .map(|i| i.href.clone()),
        );
        if let Some(cover) = &self.meta_cover {
            match self.item(cover) {
                Some(item) => out.push(item.href.clone()),
                None => out.push(cover.clone()),
            }
        }
        out.extend(
            self.items
                .iter()
                .filter(|i| {
                    i.media_type.starts_with("image/") && i.id.to_lowercase().contains("cover")
                })
                .map(|i| i.href.clone()),
        );
        out.extend(
            self.guide_cover
                .iter()
                .filter(|h| is_image_name(&h.to_lowercase()))
                .cloned(),
        );
        out
    }

    fn spine_docs(&self) -> Vec<String> {
        self.spine
            .iter()
            .filter_map(|id| self.item(id))
            .map(|i| i.href.clone())
            .collect()
    }

    fn manifest_images(&self) -> Vec<String> {
        self.items
            .iter()
            .filter(|i| i.media_type.starts_with("image/"))
            .map(|i| i.href.clone())
            .collect()
    }
}

// ---------------------------------------------------------------------------------------
// FictionBook

/// The `<coverpage>` image's `<binary>` payload, else the first image binary.
fn fb2_cover(xml: &str) -> Result<Vec<u8>> {
    let mut cover_id = None;
    let mut in_coverpage = false;
    for t in tags(xml) {
        if t.is("coverpage") {
            in_coverpage = !t.end;
        } else if in_coverpage && t.is("image") && !t.end {
            cover_id = t
                .attr("href")
                .map(|h| h.trim_start_matches('#').to_string());
            break;
        }
    }
    let mut first_image = None;
    for t in tags(xml).filter(|t| t.is("binary") && !t.end) {
        let id = t.attr("id").unwrap_or_default();
        let is_image = t
            .attr("content-type")
            .is_some_and(|c| c.to_ascii_lowercase().starts_with("image/"));
        let body = &xml[t.after..];
        let body = &body[..body.find('<').unwrap_or(body.len())];
        if cover_id.as_deref() == Some(id.as_str()) {
            return base64_decode(body);
        }
        if is_image && first_image.is_none() {
            first_image = Some(body);
        }
    }
    match first_image {
        Some(body) => base64_decode(body),
        None => Err(DecodeError::Unsupported),
    }
}

fn base64_decode(text: &str) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let mut acc = 0u32;
    let mut bits = 0u32;
    for &c in text.as_bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            c if c.is_ascii_whitespace() => continue,
            _ => {
                return Err(DecodeError::malformed(
                    "invalid base64 in FictionBook binary",
                ));
            }
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------
// Minimal tolerant XML tag scanner (no DTDs, no entity definitions).

pub(super) struct Tag<'x> {
    /// Qualified name as written.
    name: &'x str,
    pub(super) attrs: &'x str,
    pub(super) end: bool,
    /// Byte index just after the closing `>`.
    pub(super) after: usize,
}

impl<'x> Tag<'x> {
    pub(super) fn local(&self) -> &'x str {
        self.name.rsplit(':').next().unwrap_or(self.name)
    }

    pub(super) fn is(&self, local: &str) -> bool {
        self.local().eq_ignore_ascii_case(local)
    }

    /// Attribute value by local name (prefix ignored, case-insensitive), unescaped.
    pub(super) fn attr(&self, key: &str) -> Option<String> {
        let s = self.attrs;
        let b = s.as_bytes();
        let mut i = 0;
        while i < b.len() {
            while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b'/') {
                i += 1;
            }
            let start = i;
            while i < b.len() && b[i] != b'=' && !b[i].is_ascii_whitespace() {
                i += 1;
            }
            let name = &s[start..i];
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            if i >= b.len() || b[i] != b'=' {
                if i == start {
                    i += 1;
                }
                continue;
            }
            i += 1;
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            let Some(&q) = b.get(i) else { break };
            let value = if q == b'"' || q == b'\'' {
                let rest = &s[i + 1..];
                let end = rest.find(q as char).unwrap_or(rest.len());
                i += end + 2;
                &rest[..end]
            } else {
                let rest = &s[i..];
                let end = rest
                    .find(|c: char| c.is_ascii_whitespace())
                    .unwrap_or(rest.len());
                i += end;
                &rest[..end]
            };
            let local = name.rsplit(':').next().unwrap_or(name);
            if local.eq_ignore_ascii_case(key) {
                return Some(unescape(value));
            }
        }
        None
    }
}

pub(super) fn tags(xml: &str) -> impl Iterator<Item = Tag<'_>> {
    let mut pos = 0usize;
    std::iter::from_fn(move || {
        loop {
            let rest = xml.get(pos..)?;
            let lt = rest.find('<')?;
            let start = pos + lt;
            let tail = &xml[start..];
            let skip_to = |pat: &str| tail.find(pat).map(|i| start + i + pat.len());
            if tail.starts_with("<!--") {
                pos = skip_to("-->")?;
                continue;
            }
            if tail.starts_with("<![CDATA[") {
                pos = skip_to("]]>")?;
                continue;
            }
            if tail.starts_with("<?") {
                pos = skip_to("?>")?;
                continue;
            }
            if tail.starts_with("<!") {
                pos = skip_to(">")?;
                continue;
            }
            // Find the closing '>' outside quotes.
            let bytes = tail.as_bytes();
            let mut quote = None;
            let mut close = None;
            for (i, &c) in bytes.iter().enumerate().skip(1) {
                match (quote, c) {
                    (Some(q), c) if c == q => quote = None,
                    (Some(_), _) => {}
                    (None, b'"' | b'\'') => quote = Some(c),
                    (None, b'>') => {
                        close = Some(i);
                        break;
                    }
                    _ => {}
                }
            }
            let close = close?;
            pos = start + close + 1;
            let inner = &tail[1..close];
            let (end, inner) = match inner.strip_prefix('/') {
                Some(r) => (true, r),
                None => (false, inner),
            };
            let inner = inner.strip_suffix('/').unwrap_or(inner);
            let name_len = inner
                .find(|c: char| c.is_ascii_whitespace())
                .unwrap_or(inner.len());
            if name_len == 0 {
                continue;
            }
            return Some(Tag {
                name: &inner[..name_len],
                attrs: &inner[name_len..],
                end,
                after: pos,
            });
        }
    })
}

/// Text content of the first `<local>` element, trimmed and unescaped.
fn element_text(xml: &str, local: &str) -> Option<String> {
    let t = tags(xml).find(|t| t.is(local) && !t.end)?;
    let body = &xml[t.after..];
    let text = unescape(body[..body.find('<').unwrap_or(body.len())].trim());
    (!text.is_empty()).then_some(text)
}

pub(super) fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(semi) = rest.find(';').filter(|&j| j <= 10) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..semi];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            e => e
                .strip_prefix("#x")
                .or_else(|| e.strip_prefix("#X"))
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| e.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

// ---------------------------------------------------------------------------------------
// Paths and names

fn bytes_to_text(b: &[u8]) -> String {
    let utf16 = |le: bool| {
        let units: Vec<u16> = b[2..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&c| {
                if le {
                    u16::from_le_bytes(c)
                } else {
                    u16::from_be_bytes(c)
                }
            })
            .collect();
        String::from_utf16_lossy(&units)
    };
    match b {
        [0xFF, 0xFE, ..] => utf16(true),
        [0xFE, 0xFF, ..] => utf16(false),
        [0xEF, 0xBB, 0xBF, rest @ ..] => String::from_utf8_lossy(rest).into_owned(),
        _ => String::from_utf8_lossy(b).into_owned(),
    }
}

pub(super) fn normalize(name: &str) -> String {
    let name = name.replace('\\', "/");
    let mut out: Vec<&str> = Vec::new();
    for seg in name.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    out.join("/")
}

/// Scales down (never up) so both edges fit in `edge`.
pub(super) fn shrink_to(img: DynamicImage, edge: u32) -> DynamicImage {
    if img.width() > edge || img.height() > edge {
        img.thumbnail(edge, edge)
    } else {
        img
    }
}

pub(super) fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn parent(path: &str) -> &str {
    path.rfind('/').map_or("", |i| &path[..i])
}

/// Resolves a package-relative reference (URL-escaped, maybe with a fragment) against
/// `dir`; a leading `/` means the package root.
fn resolve(dir: &str, href: &str) -> String {
    let href = href.trim();
    let href = &href[..href.find(['#', '?']).unwrap_or(href.len())];
    let href = percent_decode(href).replace('\\', "/");
    if href.starts_with('/') || dir.is_empty() {
        normalize(&href)
    } else {
        normalize(&format!("{dir}/{href}"))
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        if b[i] == b'%'
            && let (Some(h), Some(l)) = (
                b.get(i + 1).and_then(|&c| hex(c)),
                b.get(i + 2).and_then(|&c| hex(c)),
            )
        {
            out.push((h * 16 + l) as u8);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn extension(lower_name: &str) -> &str {
    let base = basename(lower_name);
    base.rfind('.').map_or("", |i| &base[i + 1..])
}

/// Raster formats decodable from bytes (the same table as standalone files).
pub(super) fn is_image_name(lower_name: &str) -> bool {
    super::raster::format_for_ext(extension(lower_name)).is_some()
}

fn is_metafile(path: &str) -> bool {
    matches!(
        extension(&path.to_lowercase()),
        "emf" | "wmf" | "emz" | "wmz"
    )
}

/// Case-insensitive natural order: digit runs compare by numeric value.
pub(super) fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i].is_ascii_digit() && b[j].is_ascii_digit() {
            let (si, sj) = (i, j);
            while i < a.len() && a[i].is_ascii_digit() {
                i += 1;
            }
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            let na = trim_zeros(&a[si..i]);
            let nb = trim_zeros(&b[sj..j]);
            let ord = na.len().cmp(&nb.len()).then_with(|| na.cmp(nb));
            if ord != Ordering::Equal {
                return ord;
            }
        } else {
            let ord = a[i].to_ascii_lowercase().cmp(&b[j].to_ascii_lowercase());
            if ord != Ordering::Equal {
                return ord;
            }
            i += 1;
            j += 1;
        }
    }
    (a.len() - i).cmp(&(b.len() - j))
}

fn trim_zeros(d: &[u8]) -> &[u8] {
    let n = d.iter().take_while(|&&c| c == b'0').count();
    &d[n.min(d.len().saturating_sub(1))..]
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat, Rgba};
    use std::io::Write;
    use zip::CompressionMethod;
    use zip::write::SimpleFileOptions;

    fn png(w: u32, h: u32, color: [u8; 4]) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        RgbaImage::from_pixel(w, h, Rgba(color))
            .write_to(&mut out, ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    fn jpeg(w: u32, h: u32) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        image::RgbImage::from_pixel(w, h, image::Rgb([200, 100, 50]))
            .write_to(&mut out, ImageFormat::Jpeg)
            .unwrap();
        out.into_inner()
    }

    fn zip_of(files: &[(&str, &[u8])], method: CompressionMethod) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let opts = SimpleFileOptions::default().compression_method(method);
        for (name, data) in files {
            w.start_file(*name, opts).unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    fn zip(files: &[(&str, &[u8])]) -> Vec<u8> {
        zip_of(files, CompressionMethod::Stored)
    }

    fn dims(img: &DynamicImage) -> (u32, u32) {
        (img.width(), img.height())
    }

    /// Overwrites the uncompressed size of every central-directory record.
    fn patch_declared_sizes(z: &mut [u8], size: u32) {
        let mut i = 0;
        while let Some(p) = z[i..].windows(4).position(|w| w == b"PK\x01\x02") {
            let at = i + p + 24;
            z[at..at + 4].copy_from_slice(&size.to_le_bytes());
            i = at;
        }
    }

    #[test]
    fn openraster_merged_image_case_insensitive() {
        let z = zip(&[
            ("stack.xml", b"<image/>"),
            ("MergedImage.PNG", &png(7, 5, [1, 2, 3, 255])),
        ]);
        assert_eq!(dims(&extract(&z, "ora").unwrap()), (7, 5));
        assert_eq!(dims(&extract(&z, "ORA").unwrap()), (7, 5));
    }

    #[test]
    fn ooxml_thumbnail_relationship_then_fallbacks() {
        let rels = br#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
            <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
            <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/package/2006/relationships/metadata/thumbnail" Target="/docProps/thumb%20nail.png"/>
            </Relationships>"#;
        let z = zip(&[
            ("_rels/.rels", rels),
            ("docProps/thumb nail.png", &png(9, 4, [0, 0, 0, 255])),
            ("docProps/thumbnail.jpeg", &jpeg(3, 3)),
        ]);
        assert_eq!(dims(&extract(&z, "docx").unwrap()), (9, 4));

        // EMF thumbnail relationship is skipped; the raster fallback is used.
        let rels = br#"<Relationships><Relationship Type="http://schemas.openxmlformats.org/package/2006/relationships/metadata/thumbnail" Target="docProps/thumbnail.emf"/></Relationships>"#;
        let z = zip(&[
            ("_rels/.rels", rels),
            ("docProps/thumbnail.emf", b"emf"),
            ("docProps/thumbnail.jpeg", &jpeg(6, 2)),
        ]);
        assert_eq!(dims(&extract(&z, "pptx").unwrap()), (6, 2));
    }

    #[test]
    fn three_mf_and_minecraft_and_iwork() {
        let z = zip(&[("Metadata/thumbnail.png", &png(4, 4, [0, 0, 0, 255]))]);
        assert_eq!(dims(&extract(&z, "3mf").unwrap()), (4, 4));
        let z = zip(&[
            ("deep/er/pack_icon.png", &png(2, 2, [0, 0, 0, 255])),
            ("rp/pack_icon.png", &png(3, 3, [0, 0, 0, 255])),
        ]);
        assert_eq!(dims(&extract(&z, "mcaddon").unwrap()), (3, 3));
        let z = zip(&[("Index/Document.iwa", b"x"), ("preview.jpg", &jpeg(5, 4))]);
        assert_eq!(dims(&extract(&z, "key").unwrap()), (5, 4));
    }

    #[test]
    fn xd_pixelorama_and_fusion_previews() {
        let z = zip(&[
            ("artwork/a/graphics/x.png", &png(9, 9, [0, 0, 0, 255])),
            ("thumbnail.png", &png(3, 3, [0, 0, 0, 255])),
            ("preview.png", &png(7, 5, [0, 0, 0, 255])),
        ]);
        assert_eq!(dims(&extract(&z, "xd").unwrap()), (7, 5));
        let z = zip(&[("thumbnail.png", &png(3, 3, [0, 0, 0, 255]))]);
        assert_eq!(dims(&extract(&z, "xd").unwrap()), (3, 3));
        let z = zip(&[
            ("data.json", b"{}"),
            ("preview.png", &png(6, 4, [0, 0, 0, 255])),
        ]);
        assert_eq!(dims(&extract(&z, "pxo").unwrap()), (6, 4));
        // Fusion archives: a preview or thumbnail image at any depth, textures ignored.
        let z = zip(&[
            ("Textures/wood.png", &png(9, 9, [0, 0, 0, 255])),
            ("Manifest.dat", b"x"),
            (
                "FusionAssetName[Active]/Thumbnail.png",
                &png(5, 5, [0, 0, 0, 255]),
            ),
        ]);
        assert_eq!(dims(&extract(&z, "f3d").unwrap()), (5, 5));
        let z = zip(&[("Textures/wood.png", &png(9, 9, [0, 0, 0, 255]))]);
        assert_eq!(extract(&z, "f3d").err(), Some(DecodeError::Unsupported));
    }

    #[test]
    fn epub_cover_from_meta() {
        let container = br#"<container><rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#;
        let opf = br#"<package><metadata><meta name="cover" content="cov"/></metadata>
            <manifest><item id="a" href="images/a.png" media-type="image/png"/>
            <item id="cov" href="images/my%20cover.png" media-type="image/png"/></manifest></package>"#;
        let z = zip(&[
            ("mimetype", b"application/epub+zip"),
            ("META-INF/container.xml", container),
            ("OEBPS/content.opf", opf),
            ("OEBPS/images/a.png", &png(2, 2, [0, 0, 0, 255])),
            ("OEBPS/images/my cover.png", &png(10, 15, [0, 0, 0, 255])),
        ]);
        assert_eq!(dims(&extract(&z, "epub").unwrap()), (10, 15));
    }

    #[test]
    fn epub_first_image_of_spine() {
        let container = br#"<container><rootfile full-path="content.opf"/></container>"#;
        let opf = br#"<package><manifest>
            <item id="c1" href="text/ch1.xhtml" media-type="application/xhtml+xml"/>
            <item id="z" href="img/z.png" media-type="image/png"/>
            <item id="p" href="img/p1.png" media-type="image/png"/></manifest>
            <spine><itemref idref="c1"/></spine></package>"#;
        let xhtml = br#"<html><body><p>Hi</p><svg:svg><svg:image xlink:href="../img/p1.png"/></svg:svg></body></html>"#;
        let z = zip(&[
            ("META-INF/container.xml", container),
            ("content.opf", opf),
            ("text/ch1.xhtml", xhtml),
            ("img/z.png", &png(2, 2, [0, 0, 0, 255])),
            ("img/p1.png", &png(8, 3, [0, 0, 0, 255])),
        ]);
        assert_eq!(dims(&extract(&z, "epub").unwrap()), (8, 3));
    }

    #[test]
    fn comic_prefers_cover_then_natural_order() {
        let z = zip(&[
            ("p10.png", &png(10, 10, [0, 0, 0, 255])),
            ("p2.png", &png(2, 2, [0, 0, 0, 255])),
            ("notes.txt", b"hi"),
        ]);
        assert_eq!(dims(&extract(&z, "cbz").unwrap()), (2, 2));
        let z = zip(&[
            ("p01.png", &png(1, 1, [0, 0, 0, 255])),
            ("__MACOSX/._Cover.png", b"junk"),
            ("extras/Cover.png", &png(5, 6, [0, 0, 0, 255])),
        ]);
        assert_eq!(dims(&extract(&z, "phz").unwrap()), (5, 6));
    }

    #[test]
    fn fictionbook_coverpage_binary() {
        let pic = png(3, 7, [0, 0, 0, 255]);
        let b64 = encode_base64(&pic);
        let fb2 = format!(
            r##"<?xml version="1.0"?><FictionBook xmlns:l="http://www.w3.org/1999/xlink"><description><title-info>
            <coverpage><image l:href="#cover.png"/></coverpage></title-info></description>
            <binary id="other.png" content-type="image/png">{other}</binary>
            <binary id="cover.png" content-type="image/png">
            {b64}
            </binary></FictionBook>"##,
            other = encode_base64(&png(1, 1, [0, 0, 0, 255]))
        );
        let z = zip_of(&[("book.fb2", fb2.as_bytes())], CompressionMethod::Deflated);
        assert_eq!(dims(&extract(&z, "fbz").unwrap()), (3, 7));
    }

    fn encode_base64(data: &[u8]) -> String {
        const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut s = String::new();
        for chunk in data.chunks(3) {
            let n = chunk
                .iter()
                .enumerate()
                .fold(0u32, |n, (i, &b)| n | u32::from(b) << (16 - 8 * i));
            for i in 0..4 {
                if i <= chunk.len() {
                    s.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
                } else {
                    s.push('=');
                }
            }
        }
        s
    }

    #[test]
    fn nupkg_and_vsix_manifest_icons() {
        let nuspec =
            br#"<package><metadata><id>X</id><icon>images\icon.png</icon></metadata></package>"#;
        let z = zip(&[
            ("a.png", &png(1, 1, [0, 0, 0, 255])),
            ("X.nuspec", nuspec),
            ("images/icon.png", &png(6, 6, [0, 0, 0, 255])),
        ]);
        assert_eq!(dims(&extract(&z, "nupkg").unwrap()), (6, 6));

        let manifest = br#"<PackageManifest><Metadata><Icon>res/icon.png</Icon><PreviewImage>res/preview.png</PreviewImage></Metadata></PackageManifest>"#;
        let z = zip(&[
            ("extension.vsixmanifest", manifest),
            ("res/icon.png", &png(4, 4, [0, 0, 0, 255])),
            ("res/preview.png", &png(9, 9, [0, 0, 0, 255])),
        ]);
        assert_eq!(dims(&extract(&z, "vsix").unwrap()), (4, 4));
    }

    #[test]
    fn contact_sheet_grid() {
        let z = zip_of(
            &[
                ("b/2.png", &png(100, 50, [0, 255, 0, 255])),
                ("b/10.png", &png(50, 50, [0, 0, 255, 255])),
                ("a.png", &png(600, 600, [255, 0, 0, 255])),
                ("broken.png", b"\x89PNG\r\n\x1a\nnope"),
                ("readme.txt", b"text"),
            ],
            CompressionMethod::Deflated,
        );
        let sheet = extract(&z, "zip").unwrap().to_rgba8();
        assert_eq!(sheet.dimensions(), (512, 512));
        assert_eq!(sheet.get_pixel(128, 128).0, [255, 0, 0, 255]); // a.png, cell 0
        assert_eq!(sheet.get_pixel(384, 128).0, [0, 255, 0, 255]); // b/2.png, cell 1
        assert_eq!(sheet.get_pixel(128, 384).0, [0, 0, 255, 255]); // b/10.png, cell 2
        assert_eq!(sheet.get_pixel(384, 384).0[3], 0); // empty cell stays transparent
        assert_eq!(sheet.get_pixel(384, 10).0[3], 0); // letterbox around the wide tile

        let single = zip(&[("only.png", &png(30, 20, [9, 9, 9, 255]))]);
        assert_eq!(dims(&extract(&single, "zip").unwrap()), (30, 20));
        assert_eq!(
            extract(&zip(&[("a.txt", b"x")]), "zip"),
            Err(DecodeError::Unsupported)
        );
    }

    #[test]
    fn declared_entry_size_over_budget_is_rejected() {
        let mut z = zip(&[("mergedimage.png", &png(2, 2, [0, 0, 0, 255]))]);
        patch_declared_sizes(&mut z, 0xFFFF_FFF0);
        assert_eq!(extract(&z, "ora"), Err(DecodeError::TooLarge));
    }

    #[test]
    fn lying_small_declared_size_is_caught_while_reading() {
        // Declares 10 bytes, inflates to 5000: the read stops and fails instead of
        // trusting the header (the `zip` reader flags the overrun before our
        // `take(entry_max + 1)` backstop is reached).
        let big = vec![7u8; 5000];
        let mut z = zip_of(&[("mergedimage.png", &big)], CompressionMethod::Deflated);
        patch_declared_sizes(&mut z, 10);
        let budget = Budget {
            entry_max: 1000,
            ..Budget::default()
        };
        match extract_with(&z, "ora", budget) {
            Err(DecodeError::Malformed(m)) => assert!(m.contains("declared"), "{m}"),
            other => panic!("unexpected result {other:?}"),
        }
    }

    #[test]
    fn total_budget_spans_entries() {
        let img = png(64, 64, [1, 2, 3, 255]);
        let z = zip(&[("1.png", &img), ("2.png", &img), ("3.png", &img)]);
        let budget = Budget {
            total_max: img.len() as u64 * 2 + 10,
            ..Budget::default()
        };
        assert_eq!(extract_with(&z, "zip", budget), Err(DecodeError::TooLarge));
    }

    #[test]
    fn zip_failed_reads_count_toward_total() {
        // p1 and p2 fail their CRC check after being fully inflated; that work must be
        // charged, leaving no budget for p3.
        let img = png(64, 64, [1, 2, 3, 255]);
        let mut z = zip(&[("p1.png", &img), ("p2.png", &img), ("p3.png", &img)]);
        let mut i = 0;
        for _ in 0..2 {
            let p = z[i..].windows(4).position(|w| w == b"PK\x01\x02").unwrap();
            let at = i + p + 16;
            z[at] ^= 0xFF;
            i = at;
        }
        let budget = Budget {
            total_max: img.len() as u64 * 2 + 10,
            ..Budget::default()
        };
        assert!(extract_with(&z, "cbz", Budget::default()).is_ok());
        assert_eq!(extract_with(&z, "cbz", budget), Err(DecodeError::TooLarge));
    }

    #[test]
    fn farbfeld_header_bomb_entry_is_too_large() {
        let mut ff = b"farbfeld".to_vec();
        ff.extend_from_slice(&20000u32.to_be_bytes());
        ff.extend_from_slice(&20000u32.to_be_bytes());
        let z = zip(&[("p1.ff", &ff), ("p2.png", &png(2, 2, [0, 0, 0, 255]))]);
        assert_eq!(extract(&z, "cbz"), Err(DecodeError::TooLarge));
    }

    #[test]
    fn entry_inspection_limit() {
        let z = zip(&[
            ("a.txt", b"1"),
            ("b.txt", b"2"),
            ("c.png", &png(2, 2, [0, 0, 0, 255])),
        ]);
        let budget = Budget {
            max_entries: 2,
            ..Budget::default()
        };
        assert_eq!(
            extract_with(&z, "cbz", budget),
            Err(DecodeError::Unsupported)
        );
        assert!(extract_with(&z, "cbz", Budget::default()).is_ok());
    }

    #[test]
    fn foreign_and_truncated_archives() {
        assert_eq!(
            extract(b"not a zip at all", "zip"),
            Err(DecodeError::Unsupported)
        );
        let z = zip(&[("mergedimage.png", &png(2, 2, [0, 0, 0, 255]))]);
        assert!(matches!(
            extract(&z[..z.len() / 2], "ora"),
            Err(DecodeError::Malformed(_))
        ));
        assert_eq!(extract(&z, "unknownext"), Err(DecodeError::Unsupported));
        // Damaged preview with no alternative.
        let z = zip(&[("mergedimage.png", b"\x89PNG\r\n\x1a\ngarbage")]);
        assert!(matches!(extract(&z, "ora"), Err(DecodeError::Malformed(_))));
    }

    #[test]
    fn helpers() {
        let mut names = vec!["p10.png", "P2.png", "p1.png", "p02b.png"];
        names.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(names, ["p1.png", "P2.png", "p02b.png", "p10.png"]);
        assert_eq!(
            resolve("OEBPS/text", "../img/a%20b.jpg#frag"),
            "OEBPS/img/a b.jpg"
        );
        assert_eq!(resolve("OEBPS", "/root.png"), "root.png");
        assert_eq!(unescape("a&amp;b&#x41;&#66;&bogus"), "a&bAB&bogus");
        assert_eq!(base64_decode("aGk=\n").unwrap(), b"hi");
    }

    // ---- Android packages ---------------------------------------------------------------

    use crate::decode::android::tests as fx;

    // Compiled by the Android SDK's `aapt2` from a manifest with
    // `android:icon="@mipmap/ic_launcher"` and resources: `mipmap/ic_launcher` = mdpi, hdpi
    // and xxhdpi PNGs plus an anydpi-v26 adaptive icon (`both`), or the adaptive icon alone
    // (`adaptive`); the adaptive icon is a foreground PNG over a colour (#112233).
    const MANIFEST_BOTH: &[u8] =
        include_bytes!("../../tests/fixtures/apk/both/AndroidManifest.xml");
    const ARSC_BOTH: &[u8] = include_bytes!("../../tests/fixtures/apk/both/resources.arsc");
    const MANIFEST_ADAPTIVE: &[u8] =
        include_bytes!("../../tests/fixtures/apk/adaptive/AndroidManifest.xml");
    const ARSC_ADAPTIVE: &[u8] = include_bytes!("../../tests/fixtures/apk/adaptive/resources.arsc");
    const ADAPTIVE_XML: &[u8] = include_bytes!("../../tests/fixtures/apk/adaptive_icon.xml");

    const RED: [u8; 4] = [255, 0, 0, 255];
    const GREEN: [u8; 4] = [0, 255, 0, 255];
    const BLUE: [u8; 4] = [0, 0, 255, 255];
    const YELLOW: [u8; 4] = [255, 255, 0, 255];

    fn px(img: &DynamicImage, x: u32, y: u32) -> [u8; 4] {
        image::GenericImageView::get_pixel(img, x, y).0
    }

    /// An APK (as aapt2 lays it out) with legacy launcher icons in three densities.
    fn apk_with_legacy_icons() -> Vec<u8> {
        zip_of(
            &[
                ("AndroidManifest.xml", MANIFEST_BOTH),
                ("res/mipmap-mdpi-v4/ic_launcher.png", &png(48, 48, RED)),
                ("res/mipmap-hdpi-v4/ic_launcher.png", &png(72, 72, GREEN)),
                ("res/mipmap-xxhdpi-v4/ic_launcher.png", &png(144, 144, BLUE)),
                ("res/mipmap-anydpi-v26/ic_launcher.xml", ADAPTIVE_XML),
                (
                    "res/mipmap-xxxhdpi-v4/ic_launcher_foreground.png",
                    &png(432, 432, YELLOW),
                ),
                ("resources.arsc", ARSC_BOTH),
            ],
            CompressionMethod::Deflated,
        )
    }

    /// Foreground: transparent with a yellow square in the middle.
    fn foreground() -> Vec<u8> {
        let mut img = RgbaImage::new(432, 432);
        for y in 166..266 {
            for x in 166..266 {
                img.put_pixel(x, y, Rgba(YELLOW));
            }
        }
        let mut out = Cursor::new(Vec::new());
        img.write_to(&mut out, ImageFormat::Png).unwrap();
        out.into_inner()
    }

    fn apk_with_adaptive_icon_only() -> Vec<u8> {
        zip_of(
            &[
                ("AndroidManifest.xml", MANIFEST_ADAPTIVE),
                ("res/mipmap-anydpi-v26/ic_launcher.xml", ADAPTIVE_XML),
                (
                    "res/mipmap-xxxhdpi-v4/ic_launcher_foreground.png",
                    &foreground(),
                ),
                ("resources.arsc", ARSC_ADAPTIVE),
            ],
            CompressionMethod::Deflated,
        )
    }

    #[test]
    fn apk_launcher_icon_resolved_through_manifest_and_resource_table() {
        // Highest-density legacy PNG wins over the adaptive icon and lower densities.
        let img = extract(&apk_with_legacy_icons(), "apk").unwrap();
        assert_eq!(dims(&img), (144, 144));
        assert_eq!(px(&img, 70, 70), BLUE);
    }

    #[test]
    fn apk_adaptive_icon_foreground_over_background_colour() {
        let img = extract(&apk_with_adaptive_icon_only(), "apk").unwrap();
        // 432 px canvas cropped to the central two thirds.
        assert_eq!(dims(&img), (288, 288));
        assert_eq!(px(&img, 144, 144), YELLOW);
        assert_eq!(px(&img, 2, 2), [0x11, 0x22, 0x33, 255]);
    }

    #[test]
    fn apk_without_resolvable_icon_falls_back_to_the_heuristic() {
        // Unreadable resource table, readable manifest.
        let z = zip(&[
            ("AndroidManifest.xml", MANIFEST_BOTH),
            ("resources.arsc", b"garbage that is not a table"),
            ("res/mipmap-hdpi/ic_launcher.png", &png(72, 72, GREEN)),
            ("res/mipmap-xhdpi/ic_launcher.png", &png(96, 96, BLUE)),
            (
                "res/mipmap-xxxhdpi/ic_launcher_round.png",
                &png(192, 192, RED),
            ),
            (
                "res/mipmap-xxxhdpi/ic_launcher_background.png",
                &png(300, 300, RED),
            ),
            ("res/drawable/splash.png", &png(500, 500, RED)),
        ]);
        assert_eq!(dims(&extract(&z, "apk").unwrap()), (96, 96));
        // Only a round icon and a foreground layer: round first.
        let z = zip(&[
            (
                "res/mipmap-xxxhdpi/ic_launcher_foreground.png",
                &png(200, 200, YELLOW),
            ),
            (
                "res/mipmap-hdpi/ic_launcher_round.webp",
                &png(100, 100, GREEN),
            ),
            (
                "res/mipmap-xhdpi/ic_launcher_round.png",
                &png(120, 120, RED),
            ),
        ]);
        assert_eq!(dims(&extract(&z, "apk").unwrap()), (120, 120));
        // Only a foreground layer.
        let z = zip(&[(
            "res/mipmap-xxxhdpi/ic_launcher_foreground.png",
            &png(200, 200, YELLOW),
        )]);
        assert_eq!(dims(&extract(&z, "apk").unwrap()), (200, 200));
        // Nothing usable.
        let z = zip(&[
            ("res/drawable/splash.png", &png(5, 5, RED)),
            ("classes.dex", b"dex"),
        ]);
        assert_eq!(extract(&z, "apk"), Err(DecodeError::Unsupported));
    }

    #[test]
    fn apk_resource_reference_cycles_and_fan_out_are_bounded() {
        // 0x7f020000 -> 0x7f020001 -> 0x7f020000 ...; the manifest points at it.
        let strings = ["x"];
        let cyclic = fx::table(
            &strings,
            0x7f,
            &[fx::type_chunk(
                2,
                0,
                &[(0, 0x01, 0x7f02_0001), (1, 0x01, 0x7f02_0000)],
                false,
            )],
        );
        let manifest = fx::xml(
            &["icon", "application"],
            &[],
            &[fx::element(
                1,
                &[fx::attr(0, 0xFFFF_FFFF, 0x01, 0x7f02_0000)],
            )],
            false,
        );
        let z = zip(&[
            ("AndroidManifest.xml", &manifest),
            ("resources.arsc", &cyclic),
            ("res/mipmap-hdpi/ic_launcher.png", &png(72, 72, GREEN)),
        ]);
        // The table yields nothing; the heuristic still finds the icon.
        assert_eq!(dims(&extract(&z, "apk").unwrap()), (72, 72));
        // 40 configurations of one entry, each referencing the next entry which again has 40.
        let wide: Vec<Vec<u8>> = (0..40u16)
            .map(|d| {
                fx::type_chunk(
                    2,
                    d + 1,
                    &[(0, 0x01, 0x7f02_0001), (1, 0x01, 0x7f02_0000)],
                    false,
                )
            })
            .collect();
        let wide = fx::table(&strings, 0x7f, &wide);
        let z = zip(&[
            ("AndroidManifest.xml", &manifest),
            ("resources.arsc", &wide),
        ]);
        assert_eq!(extract(&z, "apk"), Err(DecodeError::Unsupported));
    }

    #[test]
    fn xapk_prefers_manifest_icon_then_icon_png_then_nested_base_apk() {
        let base = apk_with_legacy_icons();
        let manifest = br#"{"package_name":"x","icon":"images\/app_icon.png","name":"X"}"#;
        let z = zip(&[
            ("manifest.json", manifest),
            ("images/app_icon.png", &png(33, 33, GREEN)),
            ("icon.png", &png(22, 22, RED)),
            ("com.example.apk", &base),
        ]);
        assert_eq!(dims(&extract(&z, "xapk").unwrap()), (33, 33));
        let z = zip(&[("icon.png", &png(22, 22, RED)), ("com.example.apk", &base)]);
        assert_eq!(dims(&extract(&z, "apkm").unwrap()), (22, 22));
        // No icon file: the base APK inside, whatever its entry method.
        for method in [CompressionMethod::Stored, CompressionMethod::Deflated] {
            let z = zip_of(
                &[
                    ("toc.pb", b"x"),
                    ("splits/config.en.apk", &zip(&[("res/x.txt", b"x")])),
                    ("splits/base-master.apk", &base),
                ],
                method,
            );
            let img = extract(&z, "apks").unwrap();
            assert_eq!(dims(&img), (144, 144), "{method:?}");
            assert_eq!(px(&img, 5, 5), BLUE);
        }
        // A bundle that is really an APK at its root.
        assert_eq!(dims(&extract(&base, "xapk").unwrap()), (144, 144));
    }

    #[test]
    fn bundles_follow_one_nested_level_only() {
        let inner = apk_with_legacy_icons();
        let middle = zip(&[("base.apk", &inner)]);
        let outer = zip(&[("base.apk", &middle)]);
        // outer/base.apk is a plain zip holding another apk: not an APK itself.
        assert_eq!(extract(&outer, "apks"), Err(DecodeError::Unsupported));
        assert_eq!(
            extract(&zip(&[("readme.txt", b"x")]), "apks"),
            Err(DecodeError::Unsupported)
        );
    }

    #[test]
    fn apk_hostile_archives_fail_cleanly() {
        let apk = apk_with_legacy_icons();
        // Truncated at every length.
        for cut in (0..apk.len()).step_by(7) {
            let r = extract(&apk[..cut], "apk");
            assert!(
                matches!(r, Err(DecodeError::Unsupported | DecodeError::Malformed(_))),
                "cut {cut}"
            );
        }
        // Lying uncompressed sizes in the central directory.
        let mut lying = apk.clone();
        patch_declared_sizes(&mut lying, u32::MAX);
        assert!(matches!(
            extract(&lying, "apk"),
            Err(DecodeError::TooLarge | DecodeError::Malformed(_))
        ));
        // A nested APK declared far beyond the entry cap is refused before it is read.
        let mut bundle = zip(&[("base.apk", &apk)]);
        patch_declared_sizes(&mut bundle, u32::MAX);
        assert_eq!(extract(&bundle, "apks"), Err(DecodeError::TooLarge));
        // Nested archive larger than the budget allows (nested bomb): the budget is shared
        // with what the nested archive itself reads.
        let bundle = zip(&[("base.apk", &apk)]);
        let tight = Budget {
            entry_max: apk.len() as u64 + 16,
            total_max: apk.len() as u64 + 500,
            max_entries: MAX_ENTRIES,
        };
        assert_eq!(
            extract_with(&bundle, "apks", tight),
            Err(DecodeError::TooLarge)
        );
        let tiny_entry = Budget {
            entry_max: 1000,
            ..tight
        };
        assert_eq!(
            extract_with(&bundle, "apks", tiny_entry),
            Err(DecodeError::TooLarge)
        );
        // An icon declaring 65535 x 65535 pixels.
        let z = zip(&[("res/mipmap-hdpi/ic_launcher.png", &{
            let mut hdr = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
            hdr.extend(65535u32.to_be_bytes());
            hdr.extend(65535u32.to_be_bytes());
            hdr.extend([8, 6, 0, 0, 0, 0, 0, 0, 0]);
            hdr
        })]);
        assert!(matches!(
            extract(&z, "apk"),
            Err(DecodeError::TooLarge | DecodeError::Malformed(_))
        ));
    }

    #[test]
    fn livp_still_prefers_the_shallowest_still_and_reads_under_budget() {
        let z = zip(&[
            ("IMG_0001.MOV", b"movie"),
            ("__MACOSX/._IMG_0001.HEIC", b"x"),
            ("sub/IMG_0002.jpg", &jpeg(4, 4)),
            ("IMG_0001.HEIC", b"heic bytes"),
        ]);
        let (bytes, ext) = livp_still(&z).unwrap();
        assert_eq!((ext, bytes.as_slice()), ("heic", b"heic bytes".as_slice()));
        assert_eq!(
            livp_still(&zip(&[("a.mov", b"m")])),
            Err(DecodeError::Unsupported)
        );
        assert_eq!(livp_still(b"nope"), Err(DecodeError::Unsupported));
    }
}
