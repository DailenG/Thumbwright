//! Settings-app behaviour that needs no window: saving settings, keeping the per-user
//! registration in step with the File types list, and turning Explorer thumbnails on or off.

use std::path::Path;

use tw_core::config::{self, Settings};
use tw_core::decode::route;
use tw_core::formats::{self, Format};
use tw_core::ids;
use tw_core::registration::{self, Layout, Plan};

/// The formats this product can register a thumbnail handler for: those with a decoder
/// route. Everything else stays with Windows' own handlers (see `twctl register`).
pub fn registrable() -> Vec<&'static Format> {
    formats::all()
        .iter()
        .filter(|f| !route::chain_for_ext(f.ext).is_empty())
        .collect()
}

#[derive(Debug, PartialEq, Eq)]
pub struct Applied {
    /// A per-user registration exists, so the File types list was applied to it.
    pub registered: bool,
    /// At least one extension was added to or removed from the registration.
    pub extensions_changed: bool,
    /// The menu is enabled but the registration has no menu class: Repair adds it.
    pub menu_needs_repair: bool,
}

/// Saves `settings` (portable INI or HKCU, wherever they were loaded from) and, when a
/// per-user registration exists in `layout`, adds or removes exactly the extensions whose
/// File types toggle changed (REQ-REG-3). Without a registration nothing is written
/// outside the settings store: the shell DLL already ignores disabled types at run time.
pub fn apply(dir: &Path, layout: &Layout, settings: &Settings) -> Result<Applied, String> {
    settings
        .save(dir)
        .map_err(|e| format!("could not save the settings: {e}"))?;
    let Some(status) = registration::status(layout) else {
        return Ok(Applied {
            registered: false,
            extensions_changed: false,
            menu_needs_repair: false,
        });
    };
    let universe: Vec<&str> = registrable().iter().map(|f| f.ext).collect();
    let enabled: Vec<&str> = universe
        .iter()
        .copied()
        .filter(|e| settings.type_enabled(e))
        .collect();
    let report = registration::sync_extensions(layout, &enabled, &universe)
        .map_err(|e| format!("settings saved, but updating the file types failed: {e}"))?;
    Ok(Applied {
        registered: true,
        extensions_changed: report != registration::Report::default(),
        menu_needs_repair: settings.menu_enabled && !status.context_menu,
    })
}

/// One line for the Maintenance page.
pub fn registration_summary(layout: &Layout) -> String {
    match registration::status(layout) {
        Some(s) => format!(
            "Explorer thumbnails are ON for this user ({} file types, right-click menu {}).",
            s.thumbnail_extensions,
            if s.context_menu { "on" } else { "off" }
        ),
        None => "Explorer thumbnails are not registered for this user.".into(),
    }
}

/// (Re-)registers every enabled, routable format for this user and returns how many. Only
/// portable copies may do this (REQ-REG-6): from an installed copy a per-user registration
/// would survive uninstall and block thumbnails.
pub fn register_user(dir: &Path, layout: &Layout, settings: &Settings) -> Result<usize, String> {
    if !config::is_portable(dir) {
        return Err(format!(
            "This is an installed copy: its file associations are registered machine-wide by \
             the installer, and a per-user registration would survive uninstall and block \
             thumbnails. Run the installer's Repair instead. (A portable copy has {} next to \
             this program.)",
            config::PORTABLE_INI
        ));
    }
    let dll = dir.join(ids::SHELL_DLL_NAME);
    if !dll.is_file() {
        return Err(format!("{} not found", dll.display()));
    }
    let extensions: Vec<&str> = registrable()
        .iter()
        .map(|f| f.ext)
        .filter(|e| settings.type_enabled(e))
        .collect();
    let n = extensions.len();
    registration::register(
        layout,
        &Plan {
            dll_path: &dll,
            extensions,
            context_menu: settings.menu_enabled,
            cloud: true,
            installer: None,
        },
    )
    .map_err(|e| e.to_string())?;
    Ok(n)
}

/// Removes this user's registration, leaving no product keys behind (REQ-REG-4). Returns how
/// many registered values another program had taken over, which are left alone.
pub fn unregister_user(layout: &Layout) -> Result<usize, String> {
    registration::unregister(layout)
        .map(|report| report.values_left_foreign)
        .map_err(|e| e.to_string())
}

/// The Maintenance status line of an installed copy: the installer registers every file type
/// machine-wide, so what matters is how many types actually use Thumbwright and how many
/// another program handles (the same numbers as `twctl doctor`, section 4).
pub fn installed_summary(
    machine_registered: bool,
    coverage: &registration::Coverage,
    total: usize,
) -> String {
    if !machine_registered {
        return "Thumbwright is not registered for all users. Run the installer's Repair.".into();
    }
    format!(
        "Active for all users. {} of {total} types use Thumbwright, {} are handled by other apps.",
        coverage.hooked,
        coverage.foreign.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use windows_registry::CURRENT_USER;

    struct Sandbox {
        base: String,
        dir: PathBuf,
        layout: Layout,
    }

    impl Sandbox {
        fn new(tag: &str) -> Self {
            let unique = format!(
                "{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            );
            let base = format!("Software\\ThumbwrightTest-settings-{unique}");
            CURRENT_USER.create(&base).unwrap();
            let dir = std::env::temp_dir().join(format!("tw-settings-{unique}"));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(config::portable_ini(&dir), "").unwrap();
            let layout = Layout {
                root: CURRENT_USER,
                classes: format!("{base}\\Classes"),
                product: format!("{base}\\Product"),
                approve: false,
                preview_handlers: format!("{base}\\PreviewHandlers"),
                property_handlers: format!("{base}\\PropertyHandlers"),
                approved: format!("{base}\\Approved"),
                notify: false,
            };
            Self { base, dir, layout }
        }

        fn register(&self, extensions: &[&str], menu: bool) {
            registration::register(
                &self.layout,
                &Plan {
                    dll_path: Path::new(r"C:\x\thumbwright_shell.dll"),
                    extensions: extensions.to_vec(),
                    context_menu: menu,
                    cloud: true,
                    installer: None,
                },
            )
            .unwrap();
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = CURRENT_USER.remove_tree(&self.base);
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn registrable_formats_all_have_a_route() {
        let all = registrable();
        assert!(all.len() > 100, "{} formats", all.len());
        assert!(all.iter().any(|f| f.ext == "png"));
        assert!(all.iter().all(|f| !route::chain_for_ext(f.ext).is_empty()));
    }

    #[test]
    fn apply_saves_and_makes_the_registry_match_the_file_types_list() {
        let sb = Sandbox::new("apply");
        sb.register(&["png", "svg"], true);
        let settings = Settings {
            disabled_types: vec!["svg".into()],
            jpeg_quality: 55,
            ..Settings::default()
        };
        let applied = apply(&sb.dir, &sb.layout, &settings).unwrap();
        assert_eq!(
            applied,
            Applied {
                registered: true,
                extensions_changed: true,
                menu_needs_repair: false
            }
        );
        assert_eq!(Settings::load(&sb.dir), settings, "read back from the INI");
        assert!(registration::extension_registered(&sb.layout, "png"));
        assert!(!registration::extension_registered(&sb.layout, "svg"));
        // Every other routable format is enabled but was never in the registration, so
        // it is added too: the registry now matches the File types list exactly.
        let enabled = registrable()
            .iter()
            .filter(|f| settings.type_enabled(f.ext))
            .count();
        let on = registrable()
            .iter()
            .filter(|f| registration::extension_registered(&sb.layout, f.ext))
            .count();
        assert_eq!(on, enabled);

        let again = apply(&sb.dir, &sb.layout, &settings).unwrap();
        assert!(!again.extensions_changed, "second apply is a no-op");
    }

    #[test]
    fn apply_without_registration_only_saves() {
        let sb = Sandbox::new("unregistered");
        let settings = Settings {
            disabled_types: vec!["png".into()],
            ..Settings::default()
        };
        let applied = apply(&sb.dir, &sb.layout, &settings).unwrap();
        assert!(!applied.registered && !applied.extensions_changed);
        assert_eq!(Settings::load(&sb.dir), settings);
        assert!(
            registration::status(&sb.layout).is_none()
                && !registration::extension_registered(&sb.layout, "tga"),
            "no registry keys appear"
        );
    }

    #[test]
    fn apply_reports_a_missing_menu_class_and_unregister_leaves_no_product_keys() {
        let sb = Sandbox::new("menu");
        sb.register(&["png"], false);
        let applied = apply(&sb.dir, &sb.layout, &Settings::default()).unwrap();
        assert!(applied.menu_needs_repair);

        unregister_user(&sb.layout).unwrap();
        assert!(registration::status(&sb.layout).is_none());
        let classes = CURRENT_USER.open(&sb.layout.classes);
        assert!(
            classes.is_err(),
            "no class keys left after unregister: the file types toggles went through the journal"
        );
    }

    #[test]
    fn installed_copies_refuse_per_user_registration() {
        let sb = Sandbox::new("installed");
        std::fs::remove_file(config::portable_ini(&sb.dir)).unwrap();
        let err = register_user(&sb.dir, &sb.layout, &Settings::default()).unwrap_err();
        assert!(err.contains("installed copy"), "{err}");
        assert!(registration::status(&sb.layout).is_none());
    }

    #[test]
    fn installed_summary_reports_coverage_like_doctor() {
        let coverage = registration::Coverage {
            hooked: 271,
            foreign: vec![("heic".into(), "{X}".into()); 44],
            progid_overridden: 4,
        };
        assert_eq!(
            installed_summary(true, &coverage, 315),
            "Active for all users. 271 of 315 types use Thumbwright, 44 are handled by other apps."
        );
        assert!(installed_summary(false, &coverage, 315).contains("not registered"));
    }
}
