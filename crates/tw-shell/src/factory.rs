//! `IClassFactory` for the product classes.

use std::ffi::c_void;

use windows::Win32::Foundation::{CLASS_E_NOAGGREGATION, E_POINTER};
use windows::Win32::System::Com::{IClassFactory, IClassFactory_Impl};
use windows::core::{BOOL, GUID, IUnknown, Interface, Ref, Result, implement};

use tw_core::ids::{self, ClassId};

use crate::ModuleRef;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Thumbnail,
    ContextMenu,
    CloudThumbnail,
    Preview,
    Property,
    ExplorerCommand,
}

impl Kind {
    const ALL: [(ClassId, Kind); 6] = [
        (ids::CLSID_THUMBNAIL, Kind::Thumbnail),
        (ids::CLSID_CONTEXT_MENU, Kind::ContextMenu),
        (ids::CLSID_CLOUD_THUMBNAIL, Kind::CloudThumbnail),
        (ids::CLSID_PREVIEW, Kind::Preview),
        (ids::CLSID_PROPERTY, Kind::Property),
        (ids::CLSID_EXPLORER_COMMAND, Kind::ExplorerCommand),
    ];

    pub(crate) fn from_clsid(clsid: &GUID) -> Option<Self> {
        Self::ALL
            .iter()
            .find(|(class, _)| *clsid == GUID::from_u128(class.guid))
            .map(|&(_, kind)| kind)
    }
}

#[implement(IClassFactory)]
pub(crate) struct ClassFactory {
    kind: Kind,
    _module: ModuleRef,
}

impl ClassFactory {
    pub(crate) fn new(kind: Kind) -> Self {
        Self {
            kind,
            _module: ModuleRef::new(),
        }
    }
}

impl IClassFactory_Impl for ClassFactory_Impl {
    fn CreateInstance(
        &self,
        outer: Ref<IUnknown>,
        riid: *const GUID,
        ppv: *mut *mut c_void,
    ) -> Result<()> {
        if ppv.is_null() || riid.is_null() {
            return Err(E_POINTER.into());
        }
        // SAFETY: checked non-null.
        unsafe { *ppv = std::ptr::null_mut() };
        if !outer.is_null() {
            return Err(CLASS_E_NOAGGREGATION.into());
        }
        let object: IUnknown = match self.kind {
            Kind::Thumbnail => crate::thumbnail::ThumbnailProvider::new().into(),
            Kind::ContextMenu => crate::menu::ContextMenu::new().into(),
            Kind::CloudThumbnail => crate::cloud::CloudThumbnailProvider::new().into(),
            Kind::Preview => crate::preview::PreviewHandler::new().into(),
            Kind::Property => crate::property::PropertyHandler::new().into(),
            Kind::ExplorerCommand => crate::explorer_command::ExplorerCommand::new().into(),
        };
        // SAFETY: riid/ppv valid per the COM contract.
        unsafe { object.query(riid, ppv).ok() }
    }

    fn LockServer(&self, lock: BOOL) -> Result<()> {
        crate::lock_server(lock.as_bool());
        Ok(())
    }
}
