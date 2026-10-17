//! Product identity. Every product GUID below is a freshly generated UUIDv4 (REQ-COM-5).
//! Microsoft-defined identifiers live with their consumers, not here.

pub const PRODUCT_NAME: &str = "Thumbwright";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// File names of the shipped binaries; they must sit in one directory.
pub const SHELL_DLL_NAME: &str = "thumbwright_shell.dll";
pub const WORKER_EXE_NAME: &str = "tw-worker.exe";
pub const CTL_EXE_NAME: &str = "twctl.exe";

/// A product class: CLSID plus the registry display name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClassId {
    pub guid: u128,
    pub display_name: &'static str,
}

impl ClassId {
    /// Registry form, `{XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX}`.
    pub fn braced(&self) -> String {
        braced(self.guid)
    }
}

pub fn braced(g: u128) -> String {
    format!(
        "{{{:08X}-{:04X}-{:04X}-{:04X}-{:012X}}}",
        (g >> 96) as u32,
        (g >> 80) as u16,
        (g >> 64) as u16,
        (g >> 48) as u16,
        g & 0xFFFF_FFFF_FFFF
    )
}

pub const CLSID_THUMBNAIL: ClassId = ClassId {
    guid: 0x99A01621_C48D_402A_A898_3D3E6D46C32F,
    display_name: "Thumbwright Thumbnail Provider",
};
pub const CLSID_CONTEXT_MENU: ClassId = ClassId {
    guid: 0x39A04F07_5D10_46F8_8AD0_729B94CF03FE,
    display_name: "Thumbwright Context Menu",
};
pub const CLSID_CLOUD_THUMBNAIL: ClassId = ClassId {
    guid: 0x74C2AD8E_3870_4DAB_8DCF_77F3469A7211,
    display_name: "Thumbwright Cloud Folder Thumbnail Provider",
};
pub const CLSID_PREVIEW: ClassId = ClassId {
    guid: 0xB9CB2C7F_A4E5_4666_B828_48A7A3C8DD53,
    display_name: "Thumbwright Preview Handler",
};
pub const CLSID_PROPERTY: ClassId = ClassId {
    guid: 0xEF0378DC_6537_4046_B925_61634B08D376,
    display_name: "Thumbwright Property Handler",
};
pub const CLSID_EXPLORER_COMMAND: ClassId = ClassId {
    guid: 0x3911672C_3AEC_4736_90AA_CB0B489B7127,
    display_name: "Thumbwright Explorer Command",
};

/// AppID for the cloud class; empty `DllSurrogate` selects the system default surrogate.
pub const APPID_CLOUD: u128 = 0x217B6D50_AA2D_4FB9_9714_0B6DA8D290A4;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn braced_matches_registry_layout() {
        assert_eq!(
            CLSID_THUMBNAIL.braced(),
            "{99A01621-C48D-402A-A898-3D3E6D46C32F}"
        );
        assert_eq!(
            braced(APPID_CLOUD),
            "{217B6D50-AA2D-4FB9-9714-0B6DA8D290A4}"
        );
    }

    #[test]
    fn product_guids_are_v4_and_distinct() {
        let all = [
            CLSID_THUMBNAIL.guid,
            CLSID_CONTEXT_MENU.guid,
            CLSID_CLOUD_THUMBNAIL.guid,
            CLSID_PREVIEW.guid,
            CLSID_PROPERTY.guid,
            CLSID_EXPLORER_COMMAND.guid,
            APPID_CLOUD,
        ];
        for (i, g) in all.iter().enumerate() {
            assert_eq!((g >> 76) & 0xF, 4, "version nibble");
            assert_eq!((g >> 62) & 0b11, 0b10, "RFC 4122 variant");
            assert!(!all[i + 1..].contains(g), "duplicate GUID");
        }
    }
}
