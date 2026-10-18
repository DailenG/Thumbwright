//! Property handler: a read-only `IPropertyStore` + `IInitializeWithStream` that exposes
//! image dimensions, bit depth, resolution and EXIF values to the Details pane, the
//! property dialog and Windows Search (installer registration, spec 4.2).
//!
//! The file is parsed lazily on the first property access, header-only
//! (`tw_core::decode::probe`), on a budgeted, panic-contained thread. A file that cannot be
//! parsed has no properties; that is not an error, so the shell keeps showing its own.

use std::cell::RefCell;
use std::mem::ManuallyDrop;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use windows::Win32::Foundation::{
    E_FAIL, E_INVALIDARG, E_POINTER, ERROR_ALREADY_INITIALIZED, FILETIME, PROPERTYKEY,
    STG_E_ACCESSDENIED,
};
use windows::Win32::System::Com::StructuredStorage::{
    InitPropVariantFromFileTime, PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0,
};
use windows::Win32::System::Com::{IStream, STGM_READWRITE, STGM_WRITE};
use windows::Win32::System::Variant::VT_LPWSTR;
use windows::Win32::UI::Shell::PropertiesSystem::{
    IInitializeWithStream, IInitializeWithStream_Impl, IPropertyStore, IPropertyStore_Impl,
};
use windows::Win32::UI::Shell::SHStrDupW;
use windows::core::{GUID, HRESULT, HSTRING, Ref, Result, implement};

use tw_core::config::Settings;
use tw_core::decode::probe::ImageInfo;
use tw_core::{limits, log, pipeline};

use crate::ModuleRef;

/// `System.Image.*` (`6444048F-4C8B-11D1-8B70-080036B11A03`) and `System.Photo.*`
/// (`14B81DA1-0135-4D31-96D9-6CBFC9671A99`) keys, as defined in the Windows SDK `propkey.h`.
const FMT_IMAGE: u128 = 0x6444048F_4C8B_11D1_8B70_080036B11A03;
const FMT_PHOTO: u128 = 0x14B81DA1_0135_4D31_96D9_6CBFC9671A99;

const fn key(fmtid: u128, pid: u32) -> PROPERTYKEY {
    PROPERTYKEY {
        fmtid: GUID::from_u128(fmtid),
        pid,
    }
}

pub(crate) const HORIZONTAL_SIZE: PROPERTYKEY = key(FMT_IMAGE, 3);
pub(crate) const VERTICAL_SIZE: PROPERTYKEY = key(FMT_IMAGE, 4);
pub(crate) const HORIZONTAL_RESOLUTION: PROPERTYKEY = key(FMT_IMAGE, 5);
pub(crate) const VERTICAL_RESOLUTION: PROPERTYKEY = key(FMT_IMAGE, 6);
pub(crate) const BIT_DEPTH: PROPERTYKEY = key(FMT_IMAGE, 7);
pub(crate) const DIMENSIONS: PROPERTYKEY = key(FMT_IMAGE, 13);
pub(crate) const CAMERA_MANUFACTURER: PROPERTYKEY = key(FMT_PHOTO, 271);
pub(crate) const CAMERA_MODEL: PROPERTYKEY = key(FMT_PHOTO, 272);
pub(crate) const ORIENTATION: PROPERTYKEY = key(FMT_PHOTO, 274);
pub(crate) const DATE_TAKEN: PROPERTYKEY = key(FMT_PHOTO, 36867);
pub(crate) const EXPOSURE_TIME: PROPERTYKEY = key(FMT_PHOTO, 33434);
pub(crate) const F_NUMBER: PROPERTYKEY = key(FMT_PHOTO, 33437);
pub(crate) const ISO_SPEED: PROPERTYKEY = key(FMT_PHOTO, 34855);
pub(crate) const FOCAL_LENGTH: PROPERTYKEY = key(FMT_PHOTO, 37386);

/// A `VT_LPWSTR` variant (`PROPVARIANT::from(&str)` would make a `VT_BSTR`).
fn lpwstr(s: &str) -> Result<PROPVARIANT> {
    // SAFETY: SHStrDupW returns a CoTaskMem copy whose ownership moves into the variant;
    // the variant frees it with PropVariantClear.
    unsafe {
        let p = SHStrDupW(&HSTRING::from(s))?;
        Ok(PROPVARIANT {
            Anonymous: PROPVARIANT_0 {
                Anonymous: ManuallyDrop::new(PROPVARIANT_0_0 {
                    vt: VT_LPWSTR,
                    wReserved1: 0,
                    wReserved2: 0,
                    wReserved3: 0,
                    Anonymous: PROPVARIANT_0_0_0 { pwszVal: p },
                }),
            },
        })
    }
}

fn filetime(ticks: u64) -> Result<PROPVARIANT> {
    let ft = FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    // SAFETY: `ft` is a valid FILETIME for the duration of the call.
    unsafe { InitPropVariantFromFileTime(&ft) }
}

/// The properties of `info` in display order.
pub(crate) fn properties(info: &ImageInfo) -> Vec<(PROPERTYKEY, PROPVARIANT)> {
    let mut out = vec![
        (HORIZONTAL_SIZE, PROPVARIANT::from(info.width)),
        (VERTICAL_SIZE, PROPVARIANT::from(info.height)),
        (BIT_DEPTH, PROPVARIANT::from(info.bits_per_pixel)),
    ];
    let mut push = |k: PROPERTYKEY, v: Result<PROPVARIANT>| {
        if let Ok(v) = v {
            out.push((k, v));
        }
    };
    push(
        DIMENSIONS,
        lpwstr(&format!("{} x {}", info.width, info.height)),
    );
    let e = &info.exif;
    if let Some(r) = e.x_resolution {
        push(HORIZONTAL_RESOLUTION, Ok(PROPVARIANT::from(r)));
    }
    if let Some(r) = e.y_resolution {
        push(VERTICAL_RESOLUTION, Ok(PROPVARIANT::from(r)));
    }
    if let Some(s) = &e.make {
        push(CAMERA_MANUFACTURER, lpwstr(s));
    }
    if let Some(s) = &e.model {
        push(CAMERA_MODEL, lpwstr(s));
    }
    if let Some(o) = e.orientation {
        push(ORIENTATION, Ok(PROPVARIANT::from(o)));
    }
    if let Some(t) = e.date_taken {
        push(DATE_TAKEN, filetime(t));
    }
    if let Some(v) = e.exposure_time {
        push(EXPOSURE_TIME, Ok(PROPVARIANT::from(v)));
    }
    if let Some(v) = e.f_number {
        push(F_NUMBER, Ok(PROPVARIANT::from(v)));
    }
    if let Some(v) = e.iso.and_then(|v| u16::try_from(v).ok()) {
        push(ISO_SPEED, Ok(PROPVARIANT::from(v)));
    }
    if let Some(v) = e.focal_length {
        push(FOCAL_LENGTH, Ok(PROPVARIANT::from(v)));
    }
    out
}

#[implement(IPropertyStore, IInitializeWithStream)]
pub(crate) struct PropertyHandler {
    stream: RefCell<Option<IStream>>,
    /// Filled on first access; `Some(empty)` for a file with no readable properties.
    props: RefCell<Option<Vec<(PROPERTYKEY, PROPVARIANT)>>>,
    _module: ModuleRef,
}

impl PropertyHandler {
    pub(crate) fn new() -> Self {
        log::init("dll");
        Self {
            stream: RefCell::new(None),
            props: RefCell::new(None),
            _module: ModuleRef::new(),
        }
    }
}

impl IInitializeWithStream_Impl for PropertyHandler_Impl {
    fn Initialize(&self, stream: Ref<IStream>, mode: u32) -> Result<()> {
        // A read-only handler cannot honour a write request (property handler contract).
        if mode & (STGM_WRITE.0 | STGM_READWRITE.0) != 0 {
            return Err(STG_E_ACCESSDENIED.into());
        }
        let stream = stream.ok()?.clone();
        let mut slot = self.stream.borrow_mut();
        if slot.is_some() {
            return Err(HRESULT::from_win32(ERROR_ALREADY_INITIALIZED.0).into());
        }
        *slot = Some(stream);
        Ok(())
    }
}

/// What a successful probe reports to the `VerboseLog` line.
struct Probed {
    info: ImageInfo,
    ext: Option<String>,
    size: u64,
    verbose: bool,
}

/// Reads and probes the stream; any failure means "no properties".
fn load(stream: &IStream) -> Vec<(PROPERTYKEY, PROPVARIANT)> {
    let started = Instant::now();
    let probe = || -> std::result::Result<Probed, String> {
        let settings = Settings::load(&crate::install_dir());
        let info = crate::thumbnail::stat(stream).map_err(|e| format!("stat: {e}"))?;
        if info.size > settings.max_file_bytes() || info.size > limits::MAX_INPUT_BYTES {
            return Err("file exceeds max file size".into());
        }
        let ext = Path::new(&info.name)
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase());
        let data =
            crate::thumbnail::read_all(stream, info.size).map_err(|e| format!("read: {e}"))?;
        let image = pipeline::run_probe(
            Arc::new(data),
            ext.as_deref(),
            limits::INPROC_DECODE_TIMEOUT,
        )
        .map_err(|e| e.to_string())?;
        Ok(Probed {
            info: image,
            ext,
            size: info.size,
            verbose: settings.verbose_log,
        })
    };
    match probe() {
        Ok(p) => {
            let props = properties(&p.info);
            log::verbose(p.verbose, || {
                format!(
                    "property handler: ok class=property ext={} size={} decoder=probe ms={} props={} dims={}x{}",
                    p.ext.as_deref().unwrap_or("?"),
                    p.size,
                    started.elapsed().as_millis(),
                    props.len(),
                    p.info.width,
                    p.info.height
                )
            });
            props
        }
        Err(cause) => {
            log::info(&format!("property handler: no properties ({cause})"));
            Vec::new()
        }
    }
}

impl PropertyHandler_Impl {
    fn with_props<T>(&self, f: impl FnOnce(&[(PROPERTYKEY, PROPVARIANT)]) -> T) -> Result<T> {
        if self.props.borrow().is_none() {
            let stream = self.stream.borrow().clone().ok_or(E_FAIL)?;
            let loaded = load(&stream);
            *self.props.borrow_mut() = Some(loaded);
        }
        Ok(f(self.props.borrow().as_deref().unwrap_or_default()))
    }
}

impl IPropertyStore_Impl for PropertyHandler_Impl {
    fn GetCount(&self) -> Result<u32> {
        self.with_props(|p| p.len() as u32)
    }

    fn GetAt(&self, index: u32, out: *mut PROPERTYKEY) -> Result<()> {
        if out.is_null() {
            return Err(E_POINTER.into());
        }
        let k = self.with_props(|p| p.get(index as usize).map(|(k, _)| *k))?;
        let k = k.ok_or(E_INVALIDARG)?;
        // SAFETY: checked non-null.
        unsafe { *out = k };
        Ok(())
    }

    fn GetValue(&self, key: *const PROPERTYKEY) -> Result<PROPVARIANT> {
        if key.is_null() {
            return Err(E_POINTER.into());
        }
        // SAFETY: checked non-null.
        let key = unsafe { *key };
        // An unknown property is S_OK with an empty variant, not an error.
        self.with_props(|p| {
            p.iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        })
    }

    fn SetValue(&self, _key: *const PROPERTYKEY, _value: *const PROPVARIANT) -> Result<()> {
        Err(STG_E_ACCESSDENIED.into())
    }

    fn Commit(&self) -> Result<()> {
        Err(STG_E_ACCESSDENIED.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tw_core::decode::exif::Exif;
    use windows::Win32::System::Variant::{VT_R8, VT_UI2, VT_UI4};

    fn info() -> ImageInfo {
        ImageInfo {
            width: 640,
            height: 480,
            bits_per_pixel: 24,
            exif: Exif {
                make: Some("Acme".into()),
                iso: Some(200),
                f_number: Some(2.8),
                x_resolution: Some(72.0),
                ..Exif::default()
            },
        }
    }

    #[test]
    fn exposes_dimensions_and_exif_with_the_documented_variant_types() {
        let props = properties(&info());
        let get = |k: PROPERTYKEY| props.iter().find(|(pk, _)| *pk == k).map(|(_, v)| v);
        assert_eq!(get(HORIZONTAL_SIZE).unwrap().vt(), VT_UI4);
        assert_eq!(u32::try_from(get(HORIZONTAL_SIZE).unwrap()).unwrap(), 640);
        assert_eq!(u32::try_from(get(VERTICAL_SIZE).unwrap()).unwrap(), 480);
        assert_eq!(u32::try_from(get(BIT_DEPTH).unwrap()).unwrap(), 24);
        assert_eq!(get(DIMENSIONS).unwrap().vt(), VT_LPWSTR);
        assert_eq!(get(DIMENSIONS).unwrap().to_string(), "640 x 480");
        assert_eq!(get(CAMERA_MANUFACTURER).unwrap().to_string(), "Acme");
        assert_eq!(get(ISO_SPEED).unwrap().vt(), VT_UI2);
        assert_eq!(get(F_NUMBER).unwrap().vt(), VT_R8);
        assert_eq!(
            f64::try_from(get(HORIZONTAL_RESOLUTION).unwrap()).unwrap(),
            72.0
        );
        assert!(get(CAMERA_MODEL).is_none(), "absent EXIF tags are omitted");
        assert!(get(DATE_TAKEN).is_none());
    }
}
