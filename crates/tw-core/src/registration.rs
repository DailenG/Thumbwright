//! Shell registration (FUNCTIONAL_SPEC.md section 4).
//!
//! Every key the engine creates and every value it writes is recorded in a write-ahead
//! journal under `<product>\Registration`: one REG_SZ value per step, named by its
//! zero-padded index and written *before* the mutation it describes, together with the value
//! it replaces, if any. An interrupted registration therefore stays undoable. Unregistration
//! validates the whole journal, then replays it newest-first: values are deleted or
//! restored, and created keys are deleted (non-recursively) *only if they are empty*. Each
//! entry is removed once its step is done, so a failed unregistration keeps exactly what is
//! still to undo and can be retried. Pre-existing keys and keys that gained foreign content
//! are left intact (REQ-REG-4), and an extension's default value / ProgID is never written
//! (REQ-REG-2).
//!
//! The journal key itself is always ours and is not journaled; ancestors of it that
//! registration creates are, as soon as the journal key exists. A crash between creating
//! those keys and journaling them, or between unjournaling and deleting them at the end of
//! unregistration, leaves them empty; every unregistration therefore also deletes the
//! journal key and the product key below it when they are empty (settings in the product
//! key keep it alive).

use std::path::Path;

use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_INVALID_DATA};
use windows::Win32::System::Registry::{HKEY, RegDeleteKeyW};
use windows::core::HSTRING;
use windows_registry::{CURRENT_USER, Key, LOCAL_MACHINE, Type};

use crate::ids::{self, ClassId};

/// Microsoft-defined `IThumbnailProvider` handler category.
pub const THUMBNAIL_HANDLER_CATEGORY: &str = "{E357FCCD-A995-4576-B01F-234630154E96}";
/// Microsoft-defined `IExtractImage` handler category (foreign-handler detection only).
pub const EXTRACT_IMAGE_CATEGORY: &str = "{BB2E617C-0920-11D1-9A0B-00C04FC2D6C1}";
const APPROVED_KEY: &str =
    "Software\\Microsoft\\Windows\\CurrentVersion\\Shell Extensions\\Approved";
const MANIFEST_SUBKEY: &str = "Registration";
const DLL_PATH_VALUE: &str = "DllPath";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// HKCU, no elevation (portable `register`), REQ-REG-5.
    User,
    /// HKLM via `DllRegisterServer` (elevated installer), REQ-REG-8.
    Machine,
}

/// Where the engine writes. `classes` and `product` are paths below `root`.
pub struct Layout {
    pub root: &'static Key,
    pub classes: String,
    pub product: String,
    /// Writes `Shell Extensions\Approved` entries (machine scope only).
    pub approve: bool,
    /// Broadcasts `SHCNE_ASSOCCHANGED` after changes (off for sandboxed tests).
    pub notify: bool,
}

impl Layout {
    pub fn for_scope(scope: Scope) -> Self {
        let (root, approve) = match scope {
            Scope::User => (CURRENT_USER, false),
            Scope::Machine => (LOCAL_MACHINE, true),
        };
        Self {
            root,
            classes: "Software\\Classes".into(),
            product: crate::config::PRODUCT_KEY.into(),
            approve,
            notify: true,
        }
    }

    fn classes_path(&self, sub: &str) -> String {
        format!("{}\\{}", self.classes, sub)
    }

    fn manifest_key(&self) -> String {
        format!("{}\\{}", self.product, MANIFEST_SUBKEY)
    }
}

/// What to register.
pub struct Plan<'a> {
    pub dll_path: &'a Path,
    /// Extensions (lower-case, no dot) that get the thumbnail handler.
    pub extensions: Vec<&'a str>,
    pub context_menu: bool,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub keys_created: usize,
    pub values_written: usize,
    pub keys_removed: usize,
    pub values_restored: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Status {
    pub dll_path: String,
    pub thumbnail_extensions: usize,
    pub context_menu: bool,
}

pub type Result<T, E = windows::core::Error> = std::result::Result<T, E>;

fn type_code(t: Type) -> u32 {
    match t {
        Type::U32 => 4,
        Type::U64 => 11,
        Type::String => 1,
        Type::ExpandString => 2,
        Type::MultiString => 7,
        Type::Bytes => 3,
        Type::Other(n) => n,
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// One journal entry. Line formats (tab-separated):
/// `K <path>`                         key created by us
/// `V <path> <name> new`              value created by us
/// `V <path> <name> old <type> <hex>` value replaced by us (restored on unregister)
enum Step {
    Key(String),
    NewValue {
        path: String,
        name: String,
    },
    OldValue {
        path: String,
        name: String,
        ty: u32,
        data: Vec<u8>,
    },
}

impl Step {
    fn parse(line: &str) -> Option<Self> {
        let f: Vec<&str> = line.split('\t').collect();
        Some(match f.as_slice() {
            ["K", path] => Self::Key(path.to_string()),
            ["V", path, name, "new"] => Self::NewValue {
                path: path.to_string(),
                name: name.to_string(),
            },
            ["V", path, name, "old", ty, data] => Self::OldValue {
                path: path.to_string(),
                name: name.to_string(),
                ty: ty.parse().ok()?,
                data: unhex(data)?,
            },
            _ => return None,
        })
    }
}

fn journal_name(index: usize) -> String {
    format!("{index:05}")
}

/// The index of a journal value name (all ASCII digits), `None` for any other name.
fn journal_index(name: &str) -> Option<u64> {
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    name.parse().ok()
}

/// `path` and each of its ancestors, shallowest first.
fn prefixes(path: &str) -> impl Iterator<Item = &str> {
    path.match_indices('\\')
        .map(move |(i, _)| &path[..i])
        .chain(std::iter::once(path))
}

fn is_not_found(e: &windows::core::Error) -> bool {
    e.code() == ERROR_FILE_NOT_FOUND.to_hresult()
}

struct Recorder<'a> {
    root: &'a Key,
    journal: Key,
    next: usize,
    report: Report,
}

impl<'a> Recorder<'a> {
    /// Creates the journal key `manifest` (journaling any ancestor it had to create) and
    /// records `dll`, before any registration mutation.
    fn start(root: &'a Key, manifest: &str, dll: &str) -> Result<Self> {
        let mut created = Vec::new();
        for prefix in prefixes(manifest) {
            if root.open(prefix).is_err() {
                root.create(prefix)?;
                created.push(prefix);
            }
        }
        let mut rec = Self {
            root,
            journal: root.options().read().write().open(manifest)?,
            next: 0,
            report: Report {
                keys_created: created.len(),
                ..Report::default()
            },
        };
        for path in created.into_iter().filter(|&p| p != manifest) {
            rec.log(&format!("K\t{path}"))?;
        }
        rec.journal.set_string(DLL_PATH_VALUE, dll)?;
        Ok(rec)
    }

    /// Durably appends `line` to the journal; callers mutate only after this succeeds.
    fn log(&mut self, line: &str) -> Result<()> {
        self.journal.set_string(journal_name(self.next), line)?;
        self.next += 1;
        #[cfg(test)]
        tests::crash_point(self.next)?;
        Ok(())
    }

    /// Creates `path` and any missing ancestors, journaling each one before creating it.
    fn ensure_key(&mut self, path: &str) -> Result<Key> {
        for prefix in prefixes(path) {
            if self.root.open(prefix).is_err() {
                self.log(&format!("K\t{prefix}"))?;
                self.root.create(prefix)?;
                self.report.keys_created += 1;
            }
        }
        self.root.create(path)
    }

    fn set_string(&mut self, path: &str, name: &str, value: &str) -> Result<()> {
        let key = self.ensure_key(path)?;
        let line = match key.get_value(name) {
            Ok(prev)
                if prev.ty() == Type::String
                    && String::try_from(prev.clone()).is_ok_and(|p| p == value) =>
            {
                return Ok(()); // already ours or identical: nothing to own
            }
            Ok(prev) => format!(
                "V\t{path}\t{name}\told\t{}\t{}",
                type_code(prev.ty()),
                hex(&prev)
            ),
            Err(_) => format!("V\t{path}\t{name}\tnew"),
        };
        self.log(&line)?;
        key.set_string(name, value)?;
        self.report.values_written += 1;
        Ok(())
    }
}

fn register_class(rec: &mut Recorder, layout: &Layout, class: ClassId, dll: &str) -> Result<()> {
    let clsid = layout.classes_path(&format!("CLSID\\{}", class.braced()));
    rec.set_string(&clsid, "", class.display_name)?;
    let inproc = format!("{clsid}\\InprocServer32");
    rec.set_string(&inproc, "", dll)?;
    rec.set_string(&inproc, "ThreadingModel", "Apartment")?;
    if layout.approve {
        rec.set_string(APPROVED_KEY, &class.braced(), class.display_name)?;
    }
    Ok(())
}

/// Registers per `plan`, replacing any previous registration made by this engine.
pub fn register(layout: &Layout, plan: &Plan) -> Result<Report> {
    if layout.root.open(layout.manifest_key()).is_ok() {
        unregister(layout)?;
    }
    let dll = plan.dll_path.to_string_lossy().into_owned();
    let mut rec = Recorder::start(layout.root, &layout.manifest_key(), &dll)?;
    register_class(&mut rec, layout, ids::CLSID_THUMBNAIL, &dll)?;
    let thumb = ids::CLSID_THUMBNAIL.braced();
    for ext in &plan.extensions {
        for base in [format!(".{ext}"), format!("SystemFileAssociations\\.{ext}")] {
            let path =
                layout.classes_path(&format!("{base}\\shellex\\{THUMBNAIL_HANDLER_CATEGORY}"));
            rec.set_string(&path, "", &thumb)?;
        }
    }
    if plan.context_menu {
        register_class(&mut rec, layout, ids::CLSID_CONTEXT_MENU, &dll)?;
        let path = layout.classes_path(&format!(
            "*\\shellex\\ContextMenuHandlers\\{}",
            ids::PRODUCT_NAME
        ));
        rec.set_string(&path, "", &ids::CLSID_CONTEXT_MENU.braced())?;
    }
    if layout.notify {
        notify_assoc_changed();
    }
    Ok(rec.report)
}

/// Every journal entry with its value name, oldest first. Any value other than `DllPath`
/// that is not a well-formed REG_SZ entry is corruption.
fn read_journal(journal: &Key) -> Result<Vec<(String, Step)>> {
    let mut entries = Vec::new();
    for (name, value) in journal.values()? {
        if name.eq_ignore_ascii_case(DLL_PATH_VALUE) {
            continue;
        }
        let step = if value.ty() == Type::String {
            String::try_from(value)
                .ok()
                .and_then(|line| Step::parse(&line))
        } else {
            None
        };
        match (journal_index(&name), step) {
            (Some(index), Some(step)) => entries.push((index, name, step)),
            _ => {
                return Err(windows::core::Error::new(
                    ERROR_INVALID_DATA.to_hresult(),
                    format!("corrupt registration journal entry {name:?}"),
                ));
            }
        }
    }
    entries.sort_by_key(|&(index, ..)| index);
    Ok(entries
        .into_iter()
        .map(|(_, name, step)| (name, step))
        .collect())
}

enum Undo {
    /// The step is undone, or its target is already gone.
    Done,
    /// A created key was left in place because it is not empty.
    Kept,
}

fn undo(root: &Key, step: &Step, report: &mut Report) -> Result<Undo> {
    let (path, name) = match step {
        Step::Key(path) => return remove_created_key(root, path, report),
        Step::NewValue { path, name } | Step::OldValue { path, name, .. } => (path, name),
    };
    let key = match root.options().read().write().open(path) {
        Ok(key) => key,
        Err(e) if is_not_found(&e) => return Ok(Undo::Done),
        Err(e) => return Err(e),
    };
    if let Step::OldValue { ty, data, .. } = step {
        key.set_bytes(name, Type::from(*ty), data)?;
    } else {
        match key.remove_value(name) {
            Ok(()) => {}
            Err(e) if is_not_found(&e) => return Ok(Undo::Done),
            Err(e) => return Err(e),
        }
    }
    report.values_restored += 1;
    Ok(Undo::Done)
}

/// Deletes a key we created if it is empty. `RegDeleteKeyW` is not recursive, so a subkey
/// another program adds after the emptiness check makes the delete fail instead of being
/// destroyed with it.
fn remove_created_key(root: &Key, path: &str, report: &mut Report) -> Result<Undo> {
    match root.open(path) {
        Ok(key) if !is_empty(&key) => return Ok(Undo::Kept),
        Ok(_) => {}
        Err(e) if is_not_found(&e) => return Ok(Undo::Done),
        Err(e) => return Err(e),
    }
    // SAFETY: `root` owns an open registry handle for the whole call, and the temporary
    // `HSTRING` is a NUL-terminated wide string that lives until the call returns.
    let status = unsafe { RegDeleteKeyW(HKEY(root.as_raw()), &HSTRING::from(path)) };
    if status.is_ok() {
        report.keys_removed += 1;
        return Ok(Undo::Done);
    }
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(Undo::Done);
    }
    if root.open(path).is_ok_and(|key| !is_empty(&key)) {
        return Ok(Undo::Kept); // content appeared after the check; it stays
    }
    Err(status.to_hresult().into())
}

fn remove_value_if_present(key: &Key, name: &str) -> Result<()> {
    match key.remove_value(name) {
        Err(e) if !is_not_found(&e) => Err(e),
        _ => Ok(()),
    }
}

/// Undoes everything recorded in the journal. A missing journal key is a no-op.
///
/// The whole journal is validated first: an entry of the wrong type or format is an error
/// and nothing is changed. Entries are then replayed newest-first, each removed once its
/// step is done; a failed step keeps its entry and makes the call return the first error.
/// `DllPath`, the journal key and its created ancestors go only after a clean replay.
pub fn unregister(layout: &Layout) -> Result<Report> {
    let mut report = Report::default();
    let manifest = layout.manifest_key();
    let journal = match layout.root.options().read().write().open(&manifest) {
        Ok(key) => key,
        Err(e) if is_not_found(&e) => {
            remove_empty_product_keys(layout, &mut report)?;
            return Ok(report);
        }
        Err(e) => return Err(e),
    };
    // Created ancestors of the journal key can only go once the journal itself is gone.
    let (own, steps): (Vec<_>, Vec<_>) = read_journal(&journal)?.into_iter().partition(
        |(_, step)| matches!(step, Step::Key(p) if prefixes(&manifest).any(|a| a == p.as_str())),
    );
    let mut first_err = None;
    for (name, step) in steps.iter().rev() {
        let done = match undo(layout.root, step, &mut report) {
            Ok(Undo::Done) => true,
            // After a failure the kept key may hold that failed step's value: stay journaled
            // so a retry can still delete it.
            Ok(Undo::Kept) => first_err.is_none(),
            Err(e) => {
                first_err = first_err.or(Some(e));
                false
            }
        };
        if done && let Err(e) = journal.remove_value(name) {
            first_err = first_err.or(Some(e));
        }
    }
    if let Some(e) = first_err {
        if layout.notify {
            notify_assoc_changed();
        }
        return Err(e);
    }
    remove_value_if_present(&journal, DLL_PATH_VALUE)?;
    for (name, _) in &own {
        journal.remove_value(name)?;
    }
    drop(journal);
    remove_created_key(layout.root, &manifest, &mut report)?;
    remove_empty_product_keys(layout, &mut report)?;
    for (_, step) in own.iter().rev() {
        if let Step::Key(path) = step {
            remove_created_key(layout.root, path, &mut report)?;
        }
    }
    if layout.notify {
        notify_assoc_changed();
    }
    Ok(report)
}

/// Deletes the journal key and every key between it and the product key, product key
/// included, deepest first, as long as each is empty. These keys are ours by name even when
/// a crash kept them out of the journal; one that holds anything (such as settings) stays.
fn remove_empty_product_keys(layout: &Layout, report: &mut Report) -> Result<()> {
    let manifest = layout.manifest_key();
    let ours = prefixes(&manifest).filter(|p| p.len() >= layout.product.len());
    for path in ours.collect::<Vec<_>>().into_iter().rev() {
        if let Undo::Kept = remove_created_key(layout.root, path, report)? {
            break;
        }
    }
    Ok(())
}

fn is_empty(key: &Key) -> bool {
    key.keys().is_ok_and(|mut i| i.next().is_none())
        && key.values().is_ok_and(|mut i| i.next().is_none())
}

pub fn status(layout: &Layout) -> Option<Status> {
    let manifest = layout.root.open(layout.manifest_key()).ok()?;
    let dll_path = manifest.get_string(DLL_PATH_VALUE).ok();
    let lines: Vec<String> = manifest
        .values()
        .ok()?
        .filter(|(name, _)| journal_index(name).is_some())
        .filter_map(|(_, value)| String::try_from(value).ok())
        .collect();
    if dll_path.is_none() && lines.is_empty() {
        return None;
    }
    let thumb_suffix = format!("\\shellex\\{THUMBNAIL_HANDLER_CATEGORY}\t\t");
    let menu = format!("ContextMenuHandlers\\{}\t", ids::PRODUCT_NAME);
    Some(Status {
        dll_path: dll_path.unwrap_or_default(),
        thumbnail_extensions: lines
            .iter()
            .filter(|l| {
                l.starts_with("V\t")
                    && l.contains(&thumb_suffix)
                    && !l.contains("SystemFileAssociations")
            })
            .count(),
        context_menu: lines
            .iter()
            .any(|l| l.starts_with("V\t") && l.contains(&menu)),
    })
}

/// A ProgID-level thumbnail/extract-image handler takes precedence over extension-level
/// registration (REQ-REG-7). Returns `(progid, handler clsid)` when one exists for `ext`.
pub fn foreign_progid_handler(ext: &str) -> Option<(String, String)> {
    let classes = windows_registry::CLASSES_ROOT;
    let progid = classes.open(format!(".{ext}")).ok()?.get_string("").ok()?;
    if progid.is_empty() {
        return None;
    }
    for cat in [THUMBNAIL_HANDLER_CATEGORY, EXTRACT_IMAGE_CATEGORY] {
        if let Ok(k) = classes.open(format!("{progid}\\ShellEx\\{cat}"))
            && let Ok(clsid) = k.get_string("")
            && !clsid.eq_ignore_ascii_case(&ids::CLSID_THUMBNAIL.braced())
        {
            return Some((progid, clsid));
        }
    }
    None
}

fn notify_assoc_changed() {
    use windows::Win32::UI::Shell::{SHCNE_ASSOCCHANGED, SHCNF_IDLIST, SHChangeNotify};
    // SAFETY: documented call with null item pointers for SHCNE_ASSOCCHANGED.
    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::path::PathBuf;

    thread_local! {
        /// Journal entry count at which `Recorder::log` simulates a crash (this thread only).
        static CRASH_AFTER: Cell<Option<usize>> = const { Cell::new(None) };
    }

    /// Fails once `entries` journal entries are durable but before the step the last one
    /// describes runs: exactly what a crash at that point leaves behind.
    pub(super) fn crash_point(entries: usize) -> Result<()> {
        if CRASH_AFTER.get() == Some(entries) {
            return Err(windows::Win32::Foundation::E_ABORT.into());
        }
        Ok(())
    }

    /// A throw-away layout in its own key `HKCU\Software\ThumbwrightTest-<unique>`,
    /// removed (with everything below it) on drop, so tests leave no residue.
    struct Sandbox {
        base: String,
        layout: Layout,
    }

    impl Sandbox {
        fn new(tag: &str) -> Self {
            let base = format!(
                "Software\\ThumbwrightTest-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            );
            CURRENT_USER.create(&base).unwrap();
            let layout = Layout {
                root: CURRENT_USER,
                classes: format!("{base}\\Classes"),
                product: format!("{base}\\Product"),
                approve: false,
                notify: false,
            };
            Self { base, layout }
        }

        /// Every key path and value below the sandbox, for before/after diffs.
        fn snapshot(&self) -> Vec<String> {
            fn walk(key: &Key, path: &str, out: &mut Vec<String>) {
                for (name, v) in key.values().unwrap() {
                    out.push(format!("{path}|{name}={}", hex(&v)));
                }
                for sub in key.keys().unwrap() {
                    let p = format!("{path}\\{sub}");
                    out.push(p.clone());
                    walk(&key.open(&sub).unwrap(), &p, out);
                }
            }
            let mut out = Vec::new();
            walk(&CURRENT_USER.open(&self.base).unwrap(), "", &mut out);
            out.sort();
            out
        }

        fn get(&self, sub: &str) -> Option<String> {
            CURRENT_USER
                .open(format!("{}\\Classes\\{sub}", self.base))
                .ok()?
                .get_string("")
                .ok()
        }

        /// Pre-existing user state: a ProgID, an unrelated shellex handler, and a foreign
        /// thumbnail handler on .svg that registration overrides and must restore.
        fn seed_user_state(&self) {
            let classes = format!("{}\\Classes", self.base);
            CURRENT_USER
                .create(format!("{classes}\\.png"))
                .unwrap()
                .set_string("", "pngfile")
                .unwrap();
            CURRENT_USER
                .create(format!(
                    "{classes}\\.png\\shellex\\{{00000000-0000-0000-0000-000000000001}}"
                ))
                .unwrap()
                .set_string("", "other")
                .unwrap();
            CURRENT_USER
                .create(format!(
                    "{classes}\\.svg\\shellex\\{THUMBNAIL_HANDLER_CATEGORY}"
                ))
                .unwrap()
                .set_string("", "{FOREIGN}")
                .unwrap();
        }

        /// The journal key, opened for writing.
        fn journal(&self) -> Key {
            CURRENT_USER
                .options()
                .read()
                .write()
                .open(self.layout.manifest_key())
                .unwrap()
        }

        fn journal_len(&self) -> usize {
            self.journal()
                .values()
                .unwrap()
                .filter(|(name, _)| journal_index(name).is_some())
                .count()
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = CURRENT_USER.remove_tree(&self.base);
        }
    }

    fn plan(dll: &Path) -> Plan<'_> {
        Plan {
            dll_path: dll,
            extensions: vec!["png", "svg"],
            context_menu: true,
        }
    }

    #[test]
    fn registers_both_handler_locations_without_touching_progid() {
        let sb = Sandbox::new("dual");
        CURRENT_USER
            .create(format!("{}\\Classes\\.png", sb.base))
            .unwrap()
            .set_string("", "pngfile")
            .unwrap();
        let dll = PathBuf::from(r"C:\x\thumbwright_shell.dll");
        register(&sb.layout, &plan(&dll)).unwrap();

        let thumb = Some(ids::CLSID_THUMBNAIL.braced());
        for ext in ["png", "svg"] {
            assert_eq!(
                sb.get(&format!(".{ext}\\shellex\\{THUMBNAIL_HANDLER_CATEGORY}")),
                thumb
            );
            assert_eq!(
                sb.get(&format!(
                    "SystemFileAssociations\\.{ext}\\shellex\\{THUMBNAIL_HANDLER_CATEGORY}"
                )),
                thumb
            );
        }
        assert_eq!(
            sb.get(".png").as_deref(),
            Some("pngfile"),
            "ProgID untouched"
        );
        assert_eq!(
            sb.get(".svg"),
            None,
            "no default value on created .ext keys"
        );
        let inproc = format!("CLSID\\{}\\InprocServer32", ids::CLSID_THUMBNAIL.braced());
        assert_eq!(
            sb.get(&inproc).as_deref(),
            Some(r"C:\x\thumbwright_shell.dll")
        );
        let tm = CURRENT_USER
            .open(format!("{}\\Classes\\{inproc}", sb.base))
            .unwrap()
            .get_string("ThreadingModel")
            .unwrap();
        assert_eq!(tm, "Apartment");
        assert_eq!(
            sb.get(&format!(
                "*\\shellex\\ContextMenuHandlers\\{}",
                ids::PRODUCT_NAME
            )),
            Some(ids::CLSID_CONTEXT_MENU.braced())
        );
        let st = status(&sb.layout).unwrap();
        assert_eq!(st.thumbnail_extensions, 2);
        assert!(st.context_menu);
    }

    #[test]
    fn unregister_restores_exact_prior_state() {
        let sb = Sandbox::new("clean");
        sb.seed_user_state();
        let before = sb.snapshot();

        let dll = PathBuf::from(r"C:\x\thumbwright_shell.dll");
        register(&sb.layout, &plan(&dll)).unwrap();
        assert_ne!(sb.snapshot(), before);
        // Re-registering replaces the previous registration instead of stacking on it.
        register(&sb.layout, &plan(&dll)).unwrap();
        unregister(&sb.layout).unwrap();

        assert_eq!(
            sb.snapshot(),
            before,
            "post-unregister state must equal pre-register state"
        );
        assert!(status(&sb.layout).is_none());
    }

    #[test]
    fn created_key_with_foreign_content_survives_unregister() {
        let sb = Sandbox::new("foreign");
        let dll = PathBuf::from(r"C:\x\thumbwright_shell.dll");
        register(&sb.layout, &plan(&dll)).unwrap();
        // Another program adds a value under a key we created.
        CURRENT_USER
            .create(format!("{}\\Classes\\.svg", sb.base))
            .unwrap()
            .set_string("", "svgfile")
            .unwrap();
        unregister(&sb.layout).unwrap();
        assert_eq!(sb.get(".svg").as_deref(), Some("svgfile"));
        assert_eq!(
            sb.get(&format!(".svg\\shellex\\{THUMBNAIL_HANDLER_CATEGORY}")),
            None
        );
    }

    #[test]
    fn crash_after_any_journal_entry_is_fully_undone() {
        let dll = PathBuf::from(r"C:\x\thumbwright_shell.dll");
        let total = {
            let sb = Sandbox::new("count");
            sb.seed_user_state();
            register(&sb.layout, &plan(&dll)).unwrap();
            sb.journal_len()
        };
        assert!(total > 10, "journal has {total} entries");
        for n in 1..=total {
            let sb = Sandbox::new("crash");
            sb.seed_user_state();
            let before = sb.snapshot();
            CRASH_AFTER.set(Some(n));
            let crashed = register(&sb.layout, &plan(&dll));
            CRASH_AFTER.set(None);
            assert!(crashed.is_err(), "crash point {n} not reached");
            assert_eq!(sb.journal_len(), n);
            unregister(&sb.layout).unwrap();
            assert_eq!(sb.snapshot(), before, "crash after {n} journal entries");
            assert!(status(&sb.layout).is_none());
        }
    }

    #[test]
    fn corrupt_journal_entry_is_an_error_and_changes_nothing() {
        let dll = PathBuf::from(r"C:\x\thumbwright_shell.dll");
        let corruptions: [fn(&Key); 4] = [
            |k| k.set_u32("00000", 7).unwrap(),
            |k| k.set_string("00000", "V\tbogus").unwrap(),
            |k| k.set_string("00000", "V\tp\tn\told\t1\tzz").unwrap(),
            |k| k.set_string("stray", "K\tSoftware").unwrap(),
        ];
        for (i, corrupt) in corruptions.iter().enumerate() {
            let sb = Sandbox::new("corrupt");
            sb.seed_user_state();
            register(&sb.layout, &plan(&dll)).unwrap();
            // The oldest entry is replayed last, so only up-front validation keeps the
            // newer, valid entries from being undone.
            corrupt(&sb.journal());
            let before = sb.snapshot();
            assert!(unregister(&sb.layout).is_err(), "corruption {i}");
            assert_eq!(sb.snapshot(), before, "corruption {i} changed the registry");
        }
    }

    #[test]
    fn unregister_is_idempotent() {
        let sb = Sandbox::new("twice");
        assert_eq!(unregister(&sb.layout).unwrap(), Report::default());
        let dll = PathBuf::from(r"C:\x\thumbwright_shell.dll");
        register(&sb.layout, &plan(&dll)).unwrap();
        assert_ne!(unregister(&sb.layout).unwrap(), Report::default());
        assert_eq!(unregister(&sb.layout).unwrap(), Report::default());
        assert!(status(&sb.layout).is_none());
    }

    #[test]
    fn foreign_subkey_under_created_key_survives_unregister() {
        let sb = Sandbox::new("subkey");
        let dll = PathBuf::from(r"C:\x\thumbwright_shell.dll");
        register(&sb.layout, &plan(&dll)).unwrap();
        let handler = format!(".svg\\shellex\\{THUMBNAIL_HANDLER_CATEGORY}");
        let foreign = format!("{}\\Classes\\{handler}\\Foreign", sb.base);
        // Another program adds an (empty) subkey below a key that only we wrote to.
        CURRENT_USER.create(&foreign).unwrap();
        unregister(&sb.layout).unwrap();
        assert!(CURRENT_USER.open(&foreign).is_ok());
        assert_eq!(sb.get(&handler), None, "our handler value is removed");
        assert!(status(&sb.layout).is_none());
    }

    /// The two crash windows the journal cannot record: right after `Recorder::start`
    /// created the product and journal keys (nothing journaled yet), and right after the
    /// journal key was deleted at the end of unregistration (product key still there).
    #[test]
    fn unjournaled_empty_product_keys_are_removed() {
        for journal_left in [true, false] {
            let sb = Sandbox::new("emptykeys");
            sb.seed_user_state();
            let before = sb.snapshot();
            let leftover = if journal_left {
                sb.layout.manifest_key()
            } else {
                sb.layout.product.clone()
            };
            CURRENT_USER.create(&leftover).unwrap();
            unregister(&sb.layout).unwrap();
            assert_eq!(sb.snapshot(), before, "journal key left: {journal_left}");
        }
    }

    #[test]
    fn product_key_with_settings_survives_unregister() {
        let sb = Sandbox::new("settings");
        let dll = PathBuf::from(r"C:\x\thumbwright_shell.dll");
        register(&sb.layout, &plan(&dll)).unwrap();
        CURRENT_USER
            .create(&sb.layout.product)
            .unwrap()
            .set_u32("MaxFileMb", 64)
            .unwrap();
        unregister(&sb.layout).unwrap();
        let product = CURRENT_USER.open(&sb.layout.product).unwrap();
        assert_eq!(product.get_u32("MaxFileMb").unwrap(), 64);
        assert!(CURRENT_USER.open(sb.layout.manifest_key()).is_err());
    }

    #[test]
    fn hex_round_trip() {
        assert_eq!(
            unhex(&hex(&[0, 1, 0xab, 0xff])),
            Some(vec![0, 1, 0xab, 0xff])
        );
        assert_eq!(unhex("abc"), None);
        assert_eq!(unhex("zz"), None);
    }
}
