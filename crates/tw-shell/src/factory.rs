//! `IClassFactory` for the product classes.

use std::ffi::c_void;

use windows::Win32::Foundation::{CLASS_E_NOAGGREGATION, E_POINTER};
use windows::Win32::System::Com::{IClassFactory, IClassFactory_Impl};
use windows::core::{BOOL, GUID, IUnknown, Interface, Ref, Result, implement};

use tw_core::ids;

use crate::ModuleRef;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Thumbnail,
    ContextMenu,
}

impl Kind {
    pub(crate) fn from_clsid(clsid: &GUID) -> Option<Self> {
        if *clsid == GUID::from_u128(ids::CLSID_THUMBNAIL.guid) {
            Some(Self::Thumbnail)
        } else if *clsid == GUID::from_u128(ids::CLSID_CONTEXT_MENU.guid) {
            Some(Self::ContextMenu)
        } else {
            None
        }
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
        };
        // SAFETY: riid/ppv valid per the COM contract.
        unsafe { object.query(riid, ppv).ok() }
    }

    fn LockServer(&self, lock: BOOL) -> Result<()> {
        crate::lock_server(lock.as_bool());
        Ok(())
    }
}
