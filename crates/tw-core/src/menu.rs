//! The right-click menu tree, shared by every place that shows it: the classic
//! `IContextMenu` (`tw-shell`) and the popup `twctl menu` opens from the Windows 11 entry. One
//! data set, so the two never drift apart. Pure data and selection rules: no UI here.
//!
//! Each verb is a `twctl` command line (`twctl <args...> --notify <files...>`). The menu's
//! structure is `TREE`; [`shown`] turns it into what one selection actually shows, applying
//! the user's hidden items and order and hiding (never greying) what does not apply.

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::config::{Settings, ordered_menu_ids};
use crate::formats::{self, Category};

/// When an entry applies to the selection; an entry that does not apply is hidden.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum When {
    Always,
    /// Exactly one file.
    Single,
    /// Two or more files.
    Multiple,
    /// Every selected file is an audio file.
    Audio,
}

/// The facts about a selection the menu rules look at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub count: usize,
    /// Non-empty and every file is audio.
    pub audio: bool,
}

impl Selection {
    pub fn of(files: &[PathBuf]) -> Self {
        let audio = !files.is_empty()
            && files.iter().all(|f| {
                let ext = ext_of(f);
                formats::lookup(&ext).is_some_and(|f| f.category == Category::Audio)
            });
        Self {
            count: files.len(),
            audio,
        }
    }
}

impl When {
    pub fn allows(self, sel: &Selection) -> bool {
        match self {
            Self::Always => true,
            Self::Single => sel.count == 1,
            Self::Multiple => sel.count >= 2,
            Self::Audio => sel.audio,
        }
    }
}

fn ext_of(p: &Path) -> String {
    p.extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

/// One invokable command: `twctl <args...> --notify <files...>`, or `twctl <args...> --notify`
/// for a verb that acts on no file (the selection only decides where the menu appears).
#[derive(Clone, Copy, Debug)]
pub struct Verb {
    /// Canonical verb name (GetCommandString / string invocation).
    pub name: &'static str,
    pub label: &'static str,
    pub args: &'static [&'static str],
    /// Whether the selected files are passed on; `twctl` rejects files for commands that take none.
    pub files: bool,
    pub when: When,
}

const fn v(name: &'static str, label: &'static str, args: &'static [&'static str]) -> Verb {
    Verb {
        name,
        label,
        args,
        files: true,
        when: When::Always,
    }
}

/// A verb whose `twctl` command takes no file arguments.
const fn v0(name: &'static str, label: &'static str, args: &'static [&'static str]) -> Verb {
    Verb {
        name,
        label,
        args,
        files: false,
        when: When::Always,
    }
}

impl Verb {
    const fn when(mut self, when: When) -> Self {
        self.when = when;
        self
    }

    /// The `twctl` arguments that run this verb on `files` (given on the command line).
    pub fn argv(&self, files: &[PathBuf]) -> Vec<OsString> {
        let mut args: Vec<OsString> = self.args.iter().map(OsString::from).collect();
        args.push("--notify".into());
        if self.files {
            args.extend(files.iter().map(|f| f.as_os_str().to_owned()));
        }
        args
    }
}

/// An entry inside a submenu.
pub enum Item {
    Verb(Verb),
    Separator,
    Menu {
        label: &'static str,
        items: &'static [Item],
    },
}

pub enum Node {
    Separator,
    /// `id` is the visibility key (settings `HiddenMenuItems`).
    Leaf {
        id: &'static str,
        verb: Verb,
    },
    Sub {
        id: &'static str,
        label: &'static str,
        when: When,
        items: &'static [Item],
    },
}

use Item::{Menu, Separator as Sep, Verb as V};

/// The menu, in the order of UX-REVIEW.md 3.4 with the labels of 7.1. Item ids that kept
/// their meaning kept their key, so saved `HiddenMenuItems` / `MenuOrder` survive.
pub const TREE: &[Node] = &[
    Node::Sub {
        id: "convert_into",
        label: "Convert to",
        when: When::Always,
        items: &[
            V(v("tw.convert.jpg", "JPEG", &["convert-to", "jpg"])),
            V(v("tw.convert.png", "PNG", &["convert-to", "png"])),
            V(v(
                "tw.convert.webp",
                "WebP \u{b7} lossless",
                &["convert-to", "webp"],
            )),
            V(v("tw.convert.avif", "AVIF", &["convert-to", "avif"])),
            V(v(
                "tw.convert.jxl",
                "JPEG XL \u{b7} lossless",
                &["convert-to", "jxl"],
            )),
            Menu {
                label: "More formats",
                items: &[
                    V(v("tw.convert.bmp", "BMP", &["convert-to", "bmp"])),
                    V(v("tw.convert.gif", "GIF", &["convert-to", "gif"])),
                    V(v("tw.convert.tiff", "TIFF", &["convert-to", "tiff"])),
                    V(v("tw.convert.ico", "Icon (.ico)", &["convert-to", "ico"])),
                ],
            },
        ],
    },
    Node::Sub {
        id: "resize",
        label: "Resize",
        when: When::Always,
        items: &[
            V(v(
                "tw.resize.1080",
                "Fit within 1920 x 1080",
                &["resize", "--to", "1920x1080"],
            )),
            V(v(
                "tw.resize.720",
                "Fit within 1280 x 720",
                &["resize", "--to", "1280x720"],
            )),
            V(v(
                "tw.resize.600",
                "Fit within 800 x 600",
                &["resize", "--to", "800x600"],
            )),
            V(v(
                "tw.resize.50",
                "Scale to 50%",
                &["resize", "--to", "50%"],
            )),
            V(v(
                "tw.resize.25",
                "Scale to 25%",
                &["resize", "--to", "25%"],
            )),
        ],
    },
    Node::Sub {
        id: "rotate",
        label: "Rotate & flip",
        when: When::Always,
        items: &[
            V(v(
                "tw.rotate.right",
                "Rotate right (lossless for JPEG)",
                &["rotate", "--by", "right"],
            )),
            V(v(
                "tw.rotate.left",
                "Rotate left (lossless for JPEG)",
                &["rotate", "--by", "left"],
            )),
            V(v(
                "tw.rotate.180",
                "Rotate 180\u{b0}",
                &["rotate", "--by", "180"],
            )),
            V(v(
                "tw.flip.h",
                "Flip horizontal",
                &["rotate", "--by", "fliph"],
            )),
            V(v(
                "tw.flip.v",
                "Flip vertical",
                &["rotate", "--by", "flipv"],
            )),
        ],
    },
    Node::Sub {
        id: "make_smaller",
        label: "Make smaller",
        when: When::Always,
        items: &[
            V(v(
                "tw.email.small",
                "For email, small (640 px longest side)",
                &["email", "--edge", "640"],
            )),
            V(v(
                "tw.email.medium",
                "For email, medium (1024 px)",
                &["email", "--edge", "1024"],
            )),
            V(v(
                "tw.email.large",
                "For email, large (1600 px)",
                &["email", "--edge", "1600"],
            )),
            Sep,
            V(v(
                "tw.compress.1",
                "Under 1 MB",
                &["compress", "--max-size", "1MB"],
            )),
            V(v(
                "tw.compress.5",
                "Under 5 MB",
                &["compress", "--max-size", "5MB"],
            )),
            V(v(
                "tw.compress.10",
                "Under 10 MB",
                &["compress", "--max-size", "10MB"],
            )),
            Sep,
            V(v("tw.strip", "Remove location & camera info", &["strip"])),
        ],
    },
    Node::Separator,
    Node::Leaf {
        id: "copy_text",
        verb: v(
            "tw.ocr",
            "Copy text from image",
            &["ocr", "--copy", "--window"],
        )
        .when(When::Single),
    },
    Node::Sub {
        id: "copy",
        label: "Copy",
        when: When::Single,
        items: &[
            V(v("tw.clip", "Image", &["clip"])),
            V(v("tw.datauri", "Data URI (for developers)", &["data-uri"])),
        ],
    },
    Node::Separator,
    Node::Sub {
        id: "combine",
        label: "Combine into",
        when: When::Multiple,
        items: &[
            V(v("tw.pdf", "One PDF", &["pdf", "--auto"])),
            V(v(
                "tw.pdf.ocr",
                "Searchable PDF",
                &["pdf", "--auto", "--searchable"],
            )),
            V(v("tw.cbz", "Comic book (.cbz)", &["cbz", "--auto"])),
        ],
    },
    Node::Sub {
        id: "organise",
        label: "Organise",
        when: When::Always,
        items: &[
            V(v(
                "tw.rename.date",
                "Rename by date taken",
                &["rename", "--by", "date"],
            )),
            V(v(
                "tw.rename.camera",
                "Rename by camera and date",
                &["rename", "--by", "camera-date"],
            )),
            V(v(
                "tw.rename.artist",
                "Rename by artist - title",
                &["rename", "--by", "artist-title"],
            )
            .when(When::Audio)),
            V(v(
                "tw.rename.track",
                "Rename by track - title",
                &["rename", "--by", "track-title"],
            )
            .when(When::Audio)),
            V(v(
                "tw.rename.pattern",
                "Rename with pattern\u{2026}",
                &["rename", "--by", "pattern", "--ask"],
            )),
            Sep,
            V(v(
                "tw.sort.date",
                "Sort into folders by date taken",
                &["sort", "--by", "date"],
            )),
            V(v(
                "tw.sort.size",
                "Sort into folders by image dimensions",
                &["sort", "--by", "size"],
            )),
            V(v(
                "tw.sort.tag",
                "Sort into folders by artist, album, genre or year\u{2026}",
                &["sort", "--by", "tag", "--ask"],
            )
            .when(When::Audio)),
            Sep,
            V(v("tw.tofolder", "Move into a new folder", &["to-folder"])),
        ],
    },
    Node::Separator,
    Node::Leaf {
        id: "image_info",
        verb: v("tw.info", "Details\u{2026}", &["info", "--gui"]).when(When::Single),
    },
    Node::Sub {
        id: "set_as",
        label: "Set as",
        when: When::Single,
        items: &[
            V(v("tw.lockscreen", "Lock screen", &["lock-screen"])),
            V(v("tw.foldericon", "Folder icon", &["folder-icon"])),
        ],
    },
    Node::Separator,
    // Always present (spec 6.3): `shown` never hides it.
    Node::Leaf {
        id: SETTINGS_ID,
        verb: v0(
            "tw.settings",
            "Thumbwright Workbench\u{2026}",
            &["settings"],
        ),
    },
];

/// Item id that is always shown regardless of the hidden-items setting.
pub const SETTINGS_ID: &str = "settings";

/// The folder-menu entry: prebuilds the Explorer thumbnail cache for the folder.
pub static FOLDER_VERB: Verb = v(
    "tw.prebuild",
    "Build thumbnails for this folder\u{2026}",
    &["prebuild"],
);

static SEPARATOR: Node = Node::Separator;

impl Node {
    /// The settings id of an item; separators have none.
    pub fn item_id(&self) -> Option<&'static str> {
        match self {
            Node::Separator => None,
            Node::Leaf { id, .. } | Node::Sub { id, .. } => Some(id),
        }
    }

    fn when(&self) -> When {
        match self {
            Node::Separator => When::Always,
            Node::Leaf { verb, .. } => verb.when,
            Node::Sub { when, .. } => *when,
        }
    }
}

/// The tree nodes in display order for the saved `MenuOrder`. With no saved order that is
/// `TREE` itself. Otherwise each item keeps the separator-delimited group it has in `TREE`
/// and a separator goes wherever consecutive items belong to different groups, so the
/// default order renders exactly as the default tree.
pub fn display_nodes(order: &[String]) -> Vec<&'static Node> {
    if order.is_empty() {
        return TREE.iter().collect();
    }
    let mut group = 0;
    let mut items: Vec<(usize, &'static Node)> = Vec::new();
    for node in TREE {
        match node {
            Node::Separator => group += 1,
            _ => items.push((group, node)),
        }
    }
    let mut out = Vec::with_capacity(items.len() * 2);
    let mut prev = None;
    for id in ordered_menu_ids(order) {
        if let Some(&(group, node)) = items.iter().find(|(_, n)| n.item_id() == Some(id)) {
            if prev.is_some_and(|p| p != group) {
                out.push(&SEPARATOR);
            }
            out.push(node);
            prev = Some(group);
        }
    }
    out
}

/// Every verb the menus can run, in a stable order (for string-verb lookup and tests).
pub fn all_verbs() -> Vec<&'static Verb> {
    fn walk(items: &'static [Item], out: &mut Vec<&'static Verb>) {
        for item in items {
            match item {
                Item::Verb(v) => out.push(v),
                Item::Menu { items, .. } => walk(items, out),
                Item::Separator => {}
            }
        }
    }
    let mut out = Vec::new();
    for node in TREE {
        match node {
            Node::Leaf { verb, .. } => out.push(verb),
            Node::Sub { items, .. } => walk(items, &mut out),
            Node::Separator => {}
        }
    }
    out.push(&FOLDER_VERB);
    out
}

/// One entry of the menu a selection actually shows.
#[derive(Debug)]
pub enum Shown {
    Verb(&'static Verb),
    Separator,
    Menu {
        label: &'static str,
        items: Vec<Shown>,
    },
}

/// Drops separators that lead, trail or double up, and menus left with nothing.
fn tidy(entries: Vec<Shown>) -> Vec<Shown> {
    let mut out: Vec<Shown> = Vec::with_capacity(entries.len());
    for e in entries {
        match e {
            Shown::Separator => {
                if !matches!(out.last(), None | Some(Shown::Separator)) {
                    out.push(e);
                }
            }
            Shown::Menu { items, .. } if items.is_empty() => {}
            e => out.push(e),
        }
    }
    if matches!(out.last(), Some(Shown::Separator)) {
        out.pop();
    }
    out
}

fn shown_items(items: &'static [Item], sel: &Selection) -> Vec<Shown> {
    tidy(
        items
            .iter()
            .filter_map(|item| match item {
                Item::Verb(v) => v.when.allows(sel).then_some(Shown::Verb(v)),
                Item::Separator => Some(Shown::Separator),
                Item::Menu { label, items } => Some(Shown::Menu {
                    label,
                    items: shown_items(items, sel),
                }),
            })
            .collect(),
    )
}

/// The menu for `sel`: the user's order, hidden items dropped (Settings never hides), items
/// that do not apply to this selection dropped, and the separators and empty submenus that
/// leaves behind collapsed.
pub fn shown(settings: &Settings, sel: &Selection) -> Vec<Shown> {
    let hidden = |id: &str| id != SETTINGS_ID && settings.hidden_menu_items.iter().any(|h| h == id);
    tidy(
        display_nodes(&settings.menu_order)
            .into_iter()
            .filter_map(|node| match node {
                Node::Separator => Some(Shown::Separator),
                Node::Leaf { id, verb } => {
                    (!hidden(id) && node.when().allows(sel)).then_some(Shown::Verb(verb))
                }
                Node::Sub {
                    id, label, items, ..
                } => (!hidden(id) && node.when().allows(sel)).then(|| Shown::Menu {
                    label,
                    items: shown_items(items, sel),
                }),
            })
            .collect(),
    )
}

/// Every verb reachable in `entries`, depth first.
pub fn verbs_in(entries: &[Shown]) -> Vec<&'static Verb> {
    let mut out = Vec::new();
    for e in entries {
        match e {
            Shown::Verb(v) => out.push(*v),
            Shown::Menu { items, .. } => out.extend(verbs_in(items)),
            Shown::Separator => {}
        }
    }
    out
}

/// `text` as a Win32 menu string: a lone `&` would turn the next character into an underlined
/// mnemonic and vanish ("Rotate & flip"), so each is doubled.
pub fn menu_text(text: &str) -> String {
    text.replace('&', "&&")
}

/// `3.1 MB`, `512 KB`, `900 bytes` (1024-based like Explorer).
pub fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    let one_decimal = |n: u64, unit: u64, name: &str| {
        let tenths = (n * 10 + unit / 2) / unit;
        if tenths.is_multiple_of(10) {
            format!("{} {name}", tenths / 10)
        } else {
            format!("{}.{} {name}", tenths / 10, tenths % 10)
        }
    };
    match bytes {
        0..KB => format!("{bytes} bytes"),
        KB..MB => format!("{} KB", (bytes + KB / 2) / KB),
        MB..GB => one_decimal(bytes, MB, "MB"),
        _ => one_decimal(bytes, GB, "GB"),
    }
}

/// The header text from `(file name, size in bytes)` pairs: `IMG.HEIC  3.1 MB` for one file,
/// `12 files, 48 MB` for several.
pub fn header_for(files: &[(String, u64)]) -> String {
    match files {
        [] => String::new(),
        [(name, size)] => format!("{name}  {}", format_size(*size)),
        many => {
            let total = many.iter().map(|(_, s)| s).sum();
            format!("{} files, {}", many.len(), format_size(total))
        }
    }
}

/// The header text for `files` (sizes are read from the file system; unreadable ones count 0).
pub fn header_text(files: &[PathBuf]) -> String {
    let pairs: Vec<(String, u64)> = files
        .iter()
        .map(|f| {
            (
                f.file_name()
                    .map_or_else(String::new, |n| n.to_string_lossy().into_owned()),
                std::fs::metadata(f).map_or(0, |m| m.len()),
            )
        })
        .collect();
    header_for(&pairs)
}

/// Writes `files` one per line to a new `%TEMP%\tw-list-*.txt` (the shape `twctl --files-from`
/// consumes and deletes) and returns its path.
pub fn write_list(files: &[PathBuf]) -> std::io::Result<PathBuf> {
    let list = std::env::temp_dir().join(format!(
        "tw-list-{}-{}.txt",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let body: String = files.iter().map(|f| format!("{}\n", f.display())).collect();
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&list)?;
    f.write_all(body.as_bytes())?;
    Ok(list)
}

/// Windows 11 22000+ shows the packaged "Thumbwright" entry and copies it into the classic
/// menu, so a classic root next to it would be a twin.
pub const FIRST_WINDOWS_11_BUILD: u32 = 22000;

/// Set to `1` to keep the classic root even where the packaged entry is present.
pub const FORCE_CLASSIC_ENV: &str = "THUMBWRIGHT_CLASSIC_MENU";

/// Per-user registration of the sparse package: a subkey of the app-model repository named
/// `<package name>_<version>_<arch>__<publisher id>` exists once Windows registered the package
/// for this user (the MSI stages it for everyone; Windows registers it at sign-in).
const PACKAGE_REPOSITORY: &str = "Software\\Classes\\Local Settings\\Software\\Microsoft\\Windows\\\
     CurrentVersion\\AppModel\\Repository\\Packages";
const PACKAGE_PREFIX: &str = "Thumbwright.ShellExtension_";

/// The classic root is redundant exactly when the packaged entry is shown by this Windows.
pub fn classic_root_suppressed(package_registered: bool, os_build: u32) -> bool {
    package_registered && os_build >= FIRST_WINDOWS_11_BUILD
}

/// Whether the package is registered for the current user (uncached).
#[cfg(windows)]
pub fn package_registered() -> bool {
    let Ok(repository) = windows_registry::CURRENT_USER.open(PACKAGE_REPOSITORY) else {
        return false;
    };
    repository
        .keys()
        .is_ok_and(|mut names| names.any(|n| n.starts_with(PACKAGE_PREFIX)))
}

/// The Windows build number (`CurrentBuildNumber`), 0 when unreadable.
#[cfg(windows)]
pub fn os_build() -> u32 {
    windows_registry::LOCAL_MACHINE
        .open("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion")
        .and_then(|k| k.get_string("CurrentBuildNumber"))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// Whether the classic menu should leave the file menu to the packaged entry. Explorer lives
/// for days, so the answer is cached for [`SUPPRESSION_CACHE`] rather than forever: removing
/// the package brings the classic menu back without restarting Explorer.
#[cfg(windows)]
pub fn classic_root_suppressed_now() -> bool {
    use std::sync::Mutex;
    use std::time::Instant;
    static CACHE: Mutex<Option<(Instant, bool)>> = Mutex::new(None);
    if std::env::var_os(FORCE_CLASSIC_ENV).is_some_and(|v| v == "1") {
        return false;
    }
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, answer)) = *cache
        && at.elapsed() < SUPPRESSION_CACHE
    {
        return answer;
    }
    let answer = classic_root_suppressed(package_registered(), os_build());
    *cache = Some((Instant::now(), answer));
    answer
}

/// How long [`classic_root_suppressed_now`] trusts its last answer.
pub const SUPPRESSION_CACHE: std::time::Duration = std::time::Duration::from_secs(30);

#[cfg(test)]
mod tests {
    use super::*;

    fn png() -> Vec<PathBuf> {
        vec![PathBuf::from(r"C:\a.png")]
    }

    fn labels(entries: &[Shown]) -> Vec<String> {
        entries
            .iter()
            .map(|e| match e {
                Shown::Verb(v) => v.label.to_owned(),
                Shown::Menu { label, .. } => (*label).to_owned(),
                Shown::Separator => "-".to_owned(),
            })
            .collect()
    }

    fn menu<'a>(entries: &'a [Shown], name: &str) -> Option<&'a [Shown]> {
        entries.iter().find_map(|e| match e {
            Shown::Menu { label, items } if *label == name => Some(items.as_slice()),
            _ => None,
        })
    }

    #[test]
    fn verb_names_are_unique() {
        let names: Vec<_> = all_verbs().iter().map(|v| v.name).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len());
    }

    #[test]
    fn settings_is_last_and_present_exactly_once() {
        let ids: Vec<&str> = TREE.iter().filter_map(Node::item_id).collect();
        assert_eq!(ids.last(), Some(&SETTINGS_ID));
        assert_eq!(ids.iter().filter(|i| **i == SETTINGS_ID).count(), 1);
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len(), "item ids are unique");
    }

    #[test]
    fn tree_and_settings_item_list_agree() {
        let tree: Vec<&str> = TREE.iter().filter_map(Node::item_id).collect();
        let items: Vec<&str> = crate::config::MENU_ITEMS.iter().map(|i| i.id).collect();
        assert_eq!(tree, items, "MENU_ITEMS lists the tree's items in order");
    }

    #[test]
    fn the_default_menu_for_one_png() {
        let menu = shown(&Settings::default(), &Selection::of(&png()));
        assert_eq!(
            labels(&menu),
            [
                "Convert to",
                "Resize",
                "Rotate & flip",
                "Make smaller",
                "-",
                "Copy text from image",
                "Copy",
                "-",
                "Organise",
                "-",
                "Details\u{2026}",
                "Set as",
                "-",
                "Thumbwright Workbench\u{2026}",
            ]
        );
    }

    #[test]
    fn items_that_do_not_apply_are_hidden_not_greyed() {
        let s = Settings::default();
        let one = shown(&s, &Selection::of(&png()));
        assert!(menu(&one, "Combine into").is_none(), "Combine needs 2+");
        let many = shown(
            &s,
            &Selection::of(&[PathBuf::from("a.png"), PathBuf::from("b.png")]),
        );
        assert!(menu(&many, "Combine into").is_some());
        for single_only in ["Set as", "Copy"] {
            assert!(menu(&many, single_only).is_none(), "{single_only}");
            assert!(menu(&one, single_only).is_some(), "{single_only}");
        }
        let l = labels(&many);
        assert!(!l.contains(&"Copy text from image".to_owned()));
        assert!(!l.contains(&"Details\u{2026}".to_owned()));
    }

    #[test]
    fn audio_only_items_need_an_all_audio_selection() {
        let s = Settings::default();
        let has = |files: &[&str], label: &str| {
            let files: Vec<PathBuf> = files.iter().map(PathBuf::from).collect();
            let m = shown(&s, &Selection::of(&files));
            menu(&m, "Organise")
                .unwrap()
                .iter()
                .any(|e| matches!(e, Shown::Verb(v) if v.label == label))
        };
        assert!(!has(&["a.png"], "Rename by artist - title"));
        assert!(!has(&["a.mp3", "b.png"], "Rename by artist - title"));
        assert!(has(&["a.mp3"], "Rename by artist - title"));
        assert!(has(&["a.mp3", "b.flac"], "Rename by track - title"));
        assert!(!has(
            &["a.png"],
            "Sort into folders by artist, album, genre or year\u{2026}"
        ));
        assert!(has(
            &["a.mp3"],
            "Sort into folders by artist, album, genre or year\u{2026}"
        ));
        assert!(!Selection::of(&[]).audio);
    }

    #[test]
    fn hiding_an_item_collapses_its_separators_and_settings_never_hides() {
        let s = Settings {
            hidden_menu_items: ["copy_text", "copy", "settings"].map(String::from).to_vec(),
            ..Settings::default()
        };
        let m = shown(&s, &Selection::of(&png()));
        let l = labels(&m);
        assert!(!l.contains(&"Copy".to_owned()));
        assert!(l.contains(&"Thumbwright Workbench\u{2026}".to_owned()));
        assert!(!matches!(m.first(), Some(Shown::Separator)));
        assert!(!matches!(m.last(), Some(Shown::Separator)));
        assert!(
            m.windows(2)
                .all(|w| !matches!((&w[0], &w[1]), (Shown::Separator, Shown::Separator)))
        );
    }

    #[test]
    fn tidy_drops_stray_separators_and_empty_submenus() {
        let m = tidy(vec![
            Shown::Separator,
            Shown::Menu {
                label: "empty",
                items: vec![],
            },
            Shown::Separator,
            Shown::Verb(&FOLDER_VERB),
            Shown::Separator,
            Shown::Separator,
        ]);
        assert_eq!(labels(&m), [FOLDER_VERB.label]);
    }

    #[test]
    fn every_verb_is_reachable_from_the_menu() {
        // Union over the selections that exist: one image, several images, one audio file.
        let s = Settings::default();
        let mut seen: Vec<&str> = Vec::new();
        for files in [
            vec!["a.png"],
            vec!["a.png", "b.png"],
            vec!["a.mp3"],
            vec!["a.mp3", "b.mp3"],
        ] {
            let files: Vec<PathBuf> = files.into_iter().map(PathBuf::from).collect();
            seen.extend(
                verbs_in(&shown(&s, &Selection::of(&files)))
                    .iter()
                    .map(|v| v.name),
            );
        }
        for v in all_verbs() {
            if v.name == FOLDER_VERB.name {
                continue; // the folder menu, not the file tree
            }
            assert!(seen.contains(&v.name), "{} is an orphan verb", v.name);
        }
    }

    #[test]
    fn default_order_renders_the_default_tree() {
        let shape = |nodes: &[&Node]| nodes.iter().map(|n| n.item_id()).collect::<Vec<_>>();
        let tree: Vec<&Node> = TREE.iter().collect();
        assert_eq!(shape(&display_nodes(&[])), shape(&tree));
        let default: Vec<String> = crate::config::MENU_ITEMS
            .iter()
            .map(|i| i.id.to_string())
            .collect();
        assert_eq!(shape(&display_nodes(&default)), shape(&tree));
    }

    #[test]
    fn custom_order_moves_items_and_regroups_separators() {
        let shape = |nodes: &[&Node]| nodes.iter().map(|n| n.item_id()).collect::<Vec<_>>();
        let order = ["set_as".to_string(), "resize".to_string()];
        let shown = shape(&display_nodes(&order));
        // set_as and resize sit in different default groups; the rest follow in default order.
        assert_eq!(&shown[..3], [Some("set_as"), None, Some("resize")]);
        assert!(!shown[3..].contains(&Some("set_as")) && !shown[3..].contains(&Some("resize")));
        assert_eq!(
            shown.iter().flatten().count(),
            TREE.iter().filter_map(Node::item_id).count(),
            "no item lost"
        );
        assert!(
            shown.windows(2).all(|w| w[0].is_some() || w[1].is_some()),
            "no doubled separators"
        );
    }

    #[test]
    fn argv_passes_files_only_to_verbs_that_take_them() {
        let files = png();
        let convert = all_verbs()
            .into_iter()
            .find(|v| v.name == "tw.convert.jpg")
            .unwrap();
        assert_eq!(
            convert.argv(&files),
            ["convert-to", "jpg", "--notify", r"C:\a.png"].map(OsString::from)
        );
        let settings = all_verbs()
            .into_iter()
            .find(|v| v.name == "tw.settings")
            .unwrap();
        assert_eq!(
            settings.argv(&files),
            ["settings", "--notify"].map(OsString::from)
        );
    }

    #[test]
    fn ampersands_are_doubled_for_menu_strings() {
        assert_eq!(menu_text("Rotate & flip"), "Rotate && flip");
        assert_eq!(menu_text("Plain"), "Plain");
        let every_label_with_amp = all_verbs().into_iter().filter(|v| v.label.contains('&'));
        assert!(every_label_with_amp.count() >= 1, "the tree does use '&'");
    }

    #[test]
    fn sizes_and_headers_read_like_explorer() {
        assert_eq!(format_size(0), "0 bytes");
        assert_eq!(format_size(1023), "1023 bytes");
        assert_eq!(format_size(1024), "1 KB");
        assert_eq!(format_size(512 * 1024), "512 KB");
        assert_eq!(format_size(3_250_000), "3.1 MB");
        assert_eq!(format_size(48 * 1024 * 1024), "48 MB");
        assert_eq!(format_size(5 * 1024 * 1024 * 1024), "5 GB");
        assert_eq!(
            header_for(&[("IMG.HEIC".into(), 3_250_000)]),
            "IMG.HEIC  3.1 MB"
        );
        let many: Vec<(String, u64)> = (0..12).map(|i| (format!("{i}.png"), 4 << 20)).collect();
        assert_eq!(header_for(&many), "12 files, 48 MB");
        assert_eq!(header_for(&[]), "");
    }

    #[test]
    fn the_classic_root_yields_only_to_a_packaged_entry_windows_shows() {
        assert!(classic_root_suppressed(true, 26100));
        assert!(classic_root_suppressed(true, FIRST_WINDOWS_11_BUILD));
        assert!(
            !classic_root_suppressed(true, 19045),
            "Windows 10 never shows it"
        );
        assert!(
            !classic_root_suppressed(false, 26100),
            "no package, no twin"
        );
    }

    /// The registry probe agrees with `reg.exe` listing the same key, whatever this PC has.
    #[cfg(windows)]
    #[test]
    fn the_package_probe_and_build_number_agree_with_the_registry_tool() {
        let out = std::process::Command::new("reg")
            .args(["query", &format!("HKCU\\{PACKAGE_REPOSITORY}")])
            .output()
            .unwrap();
        let listed = String::from_utf8_lossy(&out.stdout).contains(PACKAGE_PREFIX);
        assert_eq!(package_registered(), listed);
        assert!(os_build() >= 10240, "{}", os_build());
    }

    #[test]
    fn list_files_use_the_shape_twctl_consumes() {
        let list = write_list(&[PathBuf::from(r"C:\a b.png"), PathBuf::from(r"C:\c.png")]).unwrap();
        let name = list.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("tw-list-") && name.ends_with(".txt"));
        assert_eq!(
            std::fs::read_to_string(&list).unwrap(),
            "C:\\a b.png\nC:\\c.png\n"
        );
        std::fs::remove_file(list).unwrap();
    }
}
