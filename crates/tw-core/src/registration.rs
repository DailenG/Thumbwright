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
//!
//! Toggling one file type (REQ-REG-3: `register_extension`, `unregister_extension`,
//! `sync_extensions`) uses the same journal: it appends to it, or undoes exactly the entries
//! below that extension's two roots plus any shared parent that becomes empty. Everything
//! else stays journaled, so a later full `unregister` still leaves zero product keys.

use std::path::Path;

use windows::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_INVALID_DATA, ERROR_INVALID_PARAMETER, ERROR_NOT_FOUND,
};
use windows::Win32::System::Registry::{HKEY, RegDeleteKeyW};
use windows::core::HSTRING;
use windows_registry::{CURRENT_USER, Key, LOCAL_MACHINE, Type};

use crate::ids::{self, ClassId};

/// Microsoft-defined `IThumbnailProvider` handler category.
pub const THUMBNAIL_HANDLER_CATEGORY: &str = "{E357FCCD-A995-4576-B01F-234630154E96}";
/// Microsoft-defined `IExtractImage` handler category (foreign-handler detection only).
pub const EXTRACT_IMAGE_CATEGORY: &str = "{BB2E617C-0920-11D1-9A0B-00C04FC2D6C1}";
/// Microsoft-defined `IPreviewHandler` handler category.
pub const PREVIEW_HANDLER_CATEGORY: &str = "{8895B1C6-B41F-4C1C-A562-0D564250836F}";
/// AppID of the 64-bit preview host `prevhost.exe` (Microsoft docs, "Preview Handlers").
pub const PREVHOST_APPID: &str = "{6D2B5079-2F0B-48DD-AB7F-97CEC514D30B}";
const APPROVED_KEY: &str =
    "Software\\Microsoft\\Windows\\CurrentVersion\\Shell Extensions\\Approved";
const PREVIEW_HANDLERS_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\PreviewHandlers";
const PROPERTY_HANDLERS_KEY: &str =
    "Software\\Microsoft\\Windows\\CurrentVersion\\PropertySystem\\PropertyHandlers";
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
    /// The `PreviewHandlers` list that `prevhost.exe` enumerates.
    pub preview_handlers: String,
    /// The `PropertySystem\PropertyHandlers` key (one subkey per extension).
    pub property_handlers: String,
    /// Key (below `root`) holding the `Approved` values; only used when `approve` is set.
    pub approved: String,
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
            preview_handlers: PREVIEW_HANDLERS_KEY.into(),
            property_handlers: PROPERTY_HANDLERS_KEY.into(),
            approved: APPROVED_KEY.into(),
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
    /// The cloud-folder thumbnail class and its surrogate AppID (spec 4.1, REQ-COM-2). The
    /// class refuses every request until the "cloud folders" setting is on; registering it
    /// links it to no sync root.
    pub cloud: bool,
    /// Installer-only classes (spec 4.2); `None` for per-user registration.
    pub installer: Option<InstallerPlan<'a>>,
}

/// The preview and property handler classes and where they attach (spec 4.2).
pub struct InstallerPlan<'a> {
    /// Extensions (lower-case, no dot) that get the preview handler.
    pub preview_extensions: Vec<&'a str>,
    /// Extensions that get the property handler. An extension that already has a property
    /// handler is skipped: replacing the OS handler would drop its richer properties.
    pub property_extensions: Vec<&'a str>,
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

    /// Reopens the journal of an existing registration to append more steps after its
    /// last entry. Fails if there is no complete registration or the journal is corrupt.
    fn resume(root: &'a Key, manifest: &str) -> Result<Self> {
        let journal = root.options().read().write().open(manifest)?;
        if journal.get_string(DLL_PATH_VALUE).is_err() {
            return Err(windows::core::Error::new(
                ERROR_NOT_FOUND.to_hresult(),
                "Thumbwright is not registered in this scope",
            ));
        }
        let next = read_indexed_journal(&journal)?
            .last()
            .map_or(0, |&(index, ..)| index as usize + 1);
        Ok(Self {
            root,
            journal,
            next,
            report: Report::default(),
        })
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
        rec.set_string(&layout.approved, &class.braced(), class.display_name)?;
    }
    Ok(())
}

/// Registers `class` and points its `AppID` value at `app_id`, so COM activates it in the
/// process that AppID names.
fn register_class_with_appid(
    rec: &mut Recorder,
    layout: &Layout,
    class: ClassId,
    dll: &str,
    app_id: &str,
) -> Result<()> {
    register_class(rec, layout, class, dll)?;
    let clsid = layout.classes_path(&format!("CLSID\\{}", class.braced()));
    rec.set_string(&clsid, "AppID", app_id)
}

/// `AppID\{cloud}`: an empty `DllSurrogate` selects the system default surrogate.
fn register_cloud(rec: &mut Recorder, layout: &Layout, dll: &str) -> Result<()> {
    let app_id = ids::braced(ids::APPID_CLOUD);
    register_class_with_appid(rec, layout, ids::CLSID_CLOUD_THUMBNAIL, dll, &app_id)?;
    let app = layout.classes_path(&format!("AppID\\{app_id}"));
    rec.set_string(&app, "", ids::CLSID_CLOUD_THUMBNAIL.display_name)?;
    rec.set_string(&app, "DllSurrogate", "")
}

fn register_installer(
    rec: &mut Recorder,
    layout: &Layout,
    plan: &InstallerPlan,
    dll: &str,
) -> Result<()> {
    let preview = ids::CLSID_PREVIEW;
    register_class_with_appid(rec, layout, preview, dll, PREVHOST_APPID)?;
    let clsid = layout.classes_path(&format!("CLSID\\{}", preview.braced()));
    rec.set_string(&clsid, "DisplayName", preview.display_name)?;
    rec.set_string(
        &layout.preview_handlers,
        &preview.braced(),
        preview.display_name,
    )?;
    for ext in &plan.preview_extensions {
        for base in [format!(".{ext}"), format!("SystemFileAssociations\\.{ext}")] {
            let path = layout.classes_path(&format!("{base}\\shellex\\{PREVIEW_HANDLER_CATEGORY}"));
            rec.set_string(&path, "", &preview.braced())?;
        }
    }

    register_class(rec, layout, ids::CLSID_PROPERTY, dll)?;
    for ext in &plan.property_extensions {
        let path = format!("{}\\.{ext}", layout.property_handlers);
        let occupied = layout
            .root
            .open(&path)
            .ok()
            .and_then(|k| k.get_string("").ok())
            .is_some_and(|v| !v.is_empty());
        if !occupied {
            rec.set_string(&path, "", &ids::CLSID_PROPERTY.braced())?;
        }
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
    for ext in &plan.extensions {
        register_thumbnail_ext(&mut rec, layout, ext)?;
    }
    if plan.context_menu {
        register_class(&mut rec, layout, ids::CLSID_CONTEXT_MENU, &dll)?;
        let path = layout.classes_path(&format!(
            "*\\shellex\\ContextMenuHandlers\\{}",
            ids::PRODUCT_NAME
        ));
        rec.set_string(&path, "", &ids::CLSID_CONTEXT_MENU.braced())?;
    }
    if plan.cloud {
        register_cloud(&mut rec, layout, &dll)?;
    }
    if let Some(installer) = &plan.installer {
        register_installer(&mut rec, layout, installer, &dll)?;
    }
    if layout.notify {
        notify_assoc_changed();
    }
    Ok(rec.report)
}

/// Registry paths below `layout.classes` under which `ext`'s two thumbnail-handler keys
/// live: the extension key itself and its `SystemFileAssociations` twin (REQ-REG-1).
fn ext_roots(layout: &Layout, ext: &str) -> [String; 2] {
    [
        layout.classes_path(&format!(".{ext}")),
        layout.classes_path(&format!("SystemFileAssociations\\.{ext}")),
    ]
}

/// Writes the thumbnail handler value at both of `ext`'s locations, journaling first.
fn register_thumbnail_ext(rec: &mut Recorder, layout: &Layout, ext: &str) -> Result<()> {
    let thumb = ids::CLSID_THUMBNAIL.braced();
    for root in ext_roots(layout, ext) {
        rec.set_string(
            &format!("{root}\\shellex\\{THUMBNAIL_HANDLER_CATEGORY}"),
            "",
            &thumb,
        )?;
    }
    Ok(())
}

/// An extension becomes part of a registry path, so it must be a plain token: no
/// separators, wildcards or dots that would address a different key.
fn check_ext(ext: &str) -> Result<()> {
    let plain = !ext.is_empty()
        && ext.len() <= 32
        && ext
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'+' | b'~'));
    if plain {
        Ok(())
    } else {
        Err(windows::core::Error::new(
            ERROR_INVALID_PARAMETER.to_hresult(),
            format!("invalid file extension {ext:?}"),
        ))
    }
}

/// True when `path` is `root` or lies below it (registry paths compare case-insensitively).
fn is_at_or_under(path: &str, root: &str) -> bool {
    path.get(..root.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(root))
        && (path.len() == root.len() || path.as_bytes()[root.len()] == b'\\')
}

/// True when `ancestor` is a proper ancestor key of `path`.
fn is_proper_ancestor(ancestor: &str, path: &str) -> bool {
    path.len() > ancestor.len() && is_at_or_under(path, ancestor)
}

/// Registers the thumbnail handler for one more extension on top of an existing
/// registration (REQ-REG-3). Exactly its two handler keys, plus any missing parent keys,
/// are created and journaled; the extension's default value is never touched. Fails when
/// the engine has no registration in this layout: there is nothing for the handler to
/// point at. Broadcasts `SHCNE_ASSOCCHANGED` when `layout.notify` is set.
pub fn register_extension(layout: &Layout, ext: &str) -> Result<Report> {
    let result = add_extension(layout, ext);
    if layout.notify {
        notify_assoc_changed();
    }
    result
}

fn add_extension(layout: &Layout, ext: &str) -> Result<Report> {
    check_ext(ext)?;
    let mut rec = Recorder::resume(layout.root, &layout.manifest_key())?;
    register_thumbnail_ext(&mut rec, layout, ext)?;
    Ok(rec.report)
}

/// Removes `ext`'s thumbnail handler keys: undoes exactly the journal entries below its two
/// roots, newest first, then deletes the shared parents (such as `SystemFileAssociations`)
/// this engine created if nothing else lives in them any more. Entries of other extensions
/// and of the classes stay journaled. Extensions with no journaled entries are a no-op.
/// Broadcasts `SHCNE_ASSOCCHANGED` when `layout.notify` is set.
pub fn unregister_extension(layout: &Layout, ext: &str) -> Result<Report> {
    let result = remove_extension(layout, ext);
    if layout.notify {
        notify_assoc_changed();
    }
    result
}

fn remove_extension(layout: &Layout, ext: &str) -> Result<Report> {
    check_ext(ext)?;
    let mut report = Report::default();
    let journal = match layout
        .root
        .options()
        .read()
        .write()
        .open(layout.manifest_key())
    {
        Ok(key) => key,
        Err(e) if is_not_found(&e) => return Ok(report),
        Err(e) => return Err(e),
    };
    let entries = read_indexed_journal(&journal)?;
    let roots = ext_roots(layout, ext);
    let own = |step: &Step| match step {
        Step::Key(path) | Step::NewValue { path, .. } | Step::OldValue { path, .. } => {
            roots.iter().any(|r| is_at_or_under(path, r))
        }
    };
    let mut first_err = None;
    for (_, name, step) in entries.iter().rev().filter(|(.., step)| own(step)) {
        let done = match undo(layout.root, step, &mut report) {
            Ok(Undo::Done) => true,
            // As in `unregister`: after a failure a kept key may hold the failed step's value.
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
        return Err(e);
    }
    // Parents we created that only this extension needed, deepest first.
    for (_, name, step) in entries.iter().rev() {
        if let Step::Key(path) = step
            && roots.iter().any(|r| is_proper_ancestor(path, r))
            && let Undo::Done = remove_created_key(layout.root, path, &mut report)?
        {
            journal.remove_value(name)?;
        }
    }
    Ok(report)
}

/// True when both of `ext`'s thumbnail-handler keys point at this product's thumbnail class.
pub fn extension_registered(layout: &Layout, ext: &str) -> bool {
    if check_ext(ext).is_err() {
        return false;
    }
    let thumb = ids::CLSID_THUMBNAIL.braced();
    ext_roots(layout, ext).iter().all(|root| {
        layout
            .root
            .open(format!("{root}\\shellex\\{THUMBNAIL_HANDLER_CATEGORY}"))
            .and_then(|key| key.get_string(""))
            .is_ok_and(|v| v.eq_ignore_ascii_case(&thumb))
    })
}

/// Makes the registered set match `enabled` within `universe` (every extension the product
/// can register): missing ones are added, registered ones that are not enabled are removed,
/// extensions outside `universe` are left alone. One `SHCNE_ASSOCCHANGED` broadcast covers
/// all changes. Needs an existing registration only when something must be added.
pub fn sync_extensions(layout: &Layout, enabled: &[&str], universe: &[&str]) -> Result<Report> {
    let mut total = Report::default();
    let mut changed = false;
    let result = universe.iter().try_for_each(|ext| {
        let want = enabled.contains(ext);
        if want == extension_registered(layout, ext) {
            return Ok(());
        }
        changed = true;
        let step = if want {
            add_extension(layout, ext)?
        } else {
            remove_extension(layout, ext)?
        };
        total.keys_created += step.keys_created;
        total.values_written += step.values_written;
        total.keys_removed += step.keys_removed;
        total.values_restored += step.values_restored;
        Ok(())
    });
    if changed && layout.notify {
        notify_assoc_changed();
    }
    result.map(|()| total)
}

/// Every journal entry with its index and value name, oldest first. Any value other than
/// `DllPath` that is not a well-formed REG_SZ entry is corruption.
fn read_indexed_journal(journal: &Key) -> Result<Vec<(u64, String, Step)>> {
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
    Ok(entries)
}

/// Every journal entry with its value name, oldest first.
fn read_journal(journal: &Key) -> Result<Vec<(String, Step)>> {
    Ok(read_indexed_journal(journal)?
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
                preview_handlers: format!("{base}\\PreviewHandlers"),
                property_handlers: format!("{base}\\PropertyHandlers"),
                approved: format!("{base}\\Approved"),
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

        /// Pre-existing user state: a ProgID, an unrelated shellex handler, a foreign
        /// thumbnail handler and a foreign preview handler on .svg that registration
        /// overrides and must restore, and the OS property handler on .png that it must
        /// leave alone.
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
            CURRENT_USER
                .create(format!(
                    "{classes}\\.svg\\shellex\\{PREVIEW_HANDLER_CATEGORY}"
                ))
                .unwrap()
                .set_string("", "{FOREIGN-PREVIEW}")
                .unwrap();
            CURRENT_USER
                .create(format!("{}\\.png", self.layout.property_handlers))
                .unwrap()
                .set_string("", "{OS-PROPERTY-HANDLER}")
                .unwrap();
            CURRENT_USER
                .create(&self.layout.preview_handlers)
                .unwrap()
                .set_string("{FOREIGN-PREVIEW}", "Other previewer")
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
            cloud: true,
            installer: Some(InstallerPlan {
                preview_extensions: vec!["png", "svg"],
                property_extensions: vec!["png", "qoi"],
            }),
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
    fn cloud_preview_and_property_classes_register_and_undo() {
        let sb = Sandbox::new("classes");
        sb.seed_user_state();
        let before = sb.snapshot();
        let dll = PathBuf::from(r"C:\x\thumbwright_shell.dll");
        register(&sb.layout, &plan(&dll)).unwrap();

        let value = |path: String, name: &str| CURRENT_USER.open(path).ok()?.get_string(name).ok();
        let classes = format!("{}\\Classes", sb.base);
        let cloud = ids::CLSID_CLOUD_THUMBNAIL.braced();
        let appid = ids::braced(ids::APPID_CLOUD);
        assert_eq!(
            value(format!("{classes}\\CLSID\\{cloud}"), "AppID").as_deref(),
            Some(appid.as_str())
        );
        assert_eq!(
            value(format!("{classes}\\CLSID\\{cloud}\\InprocServer32"), "").as_deref(),
            Some(r"C:\x\thumbwright_shell.dll")
        );
        // REQ-COM-2: the value must exist and be empty (default surrogate), not be absent.
        assert_eq!(
            value(format!("{classes}\\AppID\\{appid}"), "DllSurrogate").as_deref(),
            Some("")
        );

        let preview = ids::CLSID_PREVIEW.braced();
        assert_eq!(
            value(format!("{classes}\\CLSID\\{preview}"), "AppID").as_deref(),
            Some(PREVHOST_APPID)
        );
        assert_eq!(
            value(sb.layout.preview_handlers.clone(), &preview).as_deref(),
            Some(ids::CLSID_PREVIEW.display_name)
        );
        assert_eq!(
            value(sb.layout.preview_handlers.clone(), "{FOREIGN-PREVIEW}").as_deref(),
            Some("Other previewer"),
            "other previewers stay listed"
        );
        for ext in ["png", "svg"] {
            for base in [format!(".{ext}"), format!("SystemFileAssociations\\.{ext}")] {
                assert_eq!(
                    sb.get(&format!("{base}\\shellex\\{PREVIEW_HANDLER_CATEGORY}")),
                    Some(preview.clone())
                );
            }
        }

        let property = |ext: &str| value(format!("{}\\.{ext}", sb.layout.property_handlers), "");
        assert_eq!(
            property("png").as_deref(),
            Some("{OS-PROPERTY-HANDLER}"),
            "an existing property handler is never replaced"
        );
        assert_eq!(property("qoi"), Some(ids::CLSID_PROPERTY.braced()));

        unregister(&sb.layout).unwrap();
        assert_eq!(sb.snapshot(), before, "zero-diff after unregister");
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

    fn plan_for<'a>(dll: &'a Path, extensions: &[&'a str]) -> Plan<'a> {
        Plan {
            dll_path: dll,
            extensions: extensions.to_vec(),
            context_menu: true,
            cloud: false,
            installer: None,
        }
    }

    fn dll() -> PathBuf {
        PathBuf::from(r"C:\x\thumbwright_shell.dll")
    }

    /// Key paths (entries without a value part) of a snapshot.
    fn keys(snapshot: &[String]) -> Vec<&String> {
        snapshot.iter().filter(|l| !l.contains('|')).collect()
    }

    #[test]
    fn extension_toggle_adds_and_removes_exactly_its_two_handler_keys() {
        let sb = Sandbox::new("toggle");
        sb.seed_user_state();
        register(&sb.layout, &plan_for(&dll(), &["png"])).unwrap();
        let before = sb.snapshot();
        assert!(!extension_registered(&sb.layout, "tga"));

        let added = register_extension(&sb.layout, "tga").unwrap();
        assert!(extension_registered(&sb.layout, "tga"));
        let after = sb.snapshot();
        let new_keys: Vec<&String> = keys(&after)
            .into_iter()
            .filter(|k| !keys(&before).contains(k))
            .collect();
        let handler = format!("shellex\\{THUMBNAIL_HANDLER_CATEGORY}");
        let leaves = new_keys.iter().filter(|k| k.ends_with(&handler)).count();
        assert_eq!(leaves, 2, "exactly two handler keys: {new_keys:?}");
        let want = [
            "\\Classes\\.tga".to_string(),
            format!("\\Classes\\.tga\\{handler}"),
            "\\Classes\\SystemFileAssociations\\.tga".to_string(),
            format!("\\Classes\\SystemFileAssociations\\.tga\\{handler}"),
        ];
        for key in &want {
            assert!(new_keys.contains(&key), "{key} missing from {new_keys:?}");
        }
        assert!(
            new_keys.iter().all(|k| k.contains(".tga")),
            "only .tga keys are added: {new_keys:?}"
        );
        assert_eq!(added.keys_created, 6);
        assert_eq!(sb.get(".tga"), None, "no default value is written");
        assert_eq!(status(&sb.layout).unwrap().thumbnail_extensions, 2);

        let removed = unregister_extension(&sb.layout, "tga").unwrap();
        assert_eq!(removed.keys_removed, 6);
        assert!(!extension_registered(&sb.layout, "tga"));
        assert!(extension_registered(&sb.layout, "png"));
        assert_eq!(
            sb.snapshot(),
            before,
            "keys, values and journal are exactly as before the toggle"
        );
    }

    #[test]
    fn extension_toggle_restores_foreign_handler_and_keeps_other_extensions() {
        let sb = Sandbox::new("toggle-foreign");
        sb.seed_user_state();
        let pristine = sb.snapshot();
        register(&sb.layout, &plan_for(&dll(), &["png", "svg"])).unwrap();

        unregister_extension(&sb.layout, "svg").unwrap();
        let svg_handler = format!(".svg\\shellex\\{THUMBNAIL_HANDLER_CATEGORY}");
        assert_eq!(sb.get(&svg_handler).as_deref(), Some("{FOREIGN}"));
        assert_eq!(
            sb.get(&format!(
                "SystemFileAssociations\\.svg\\shellex\\{THUMBNAIL_HANDLER_CATEGORY}"
            )),
            None
        );
        assert!(extension_registered(&sb.layout, "png"));
        assert!(!extension_registered(&sb.layout, "svg"));
        assert_eq!(status(&sb.layout).unwrap().thumbnail_extensions, 1);

        register_extension(&sb.layout, "svg").unwrap();
        assert!(extension_registered(&sb.layout, "svg"));
        unregister_extension(&sb.layout, "svg").unwrap();
        // The journal must still describe everything left, so a full unregister is clean.
        unregister(&sb.layout).unwrap();
        assert_eq!(sb.snapshot(), pristine);
    }

    #[test]
    fn removing_the_last_extension_deletes_the_systemfileassociations_parent_we_created() {
        let sb = Sandbox::new("toggle-parent");
        let pristine = sb.snapshot();
        register(&sb.layout, &plan_for(&dll(), &["png", "tga"])).unwrap();
        let sfa = format!("{}\\Classes\\SystemFileAssociations", sb.base);
        unregister_extension(&sb.layout, "png").unwrap();
        assert!(
            CURRENT_USER.open(&sfa).is_ok(),
            "tga still needs the parent"
        );
        unregister_extension(&sb.layout, "tga").unwrap();
        assert!(
            CURRENT_USER.open(&sfa).is_err(),
            "parent goes with its last child"
        );
        assert!(
            status(&sb.layout).is_some(),
            "the registration itself stays"
        );
        unregister(&sb.layout).unwrap();
        assert_eq!(sb.snapshot(), pristine);
    }

    #[test]
    fn extension_toggle_keeps_foreign_content_below_the_extension_key() {
        let sb = Sandbox::new("toggle-keep");
        register(&sb.layout, &plan_for(&dll(), &["png"])).unwrap();
        register_extension(&sb.layout, "tga").unwrap();
        CURRENT_USER
            .create(format!("{}\\Classes\\.tga", sb.base))
            .unwrap()
            .set_string("", "tgafile")
            .unwrap();
        unregister_extension(&sb.layout, "tga").unwrap();
        assert_eq!(sb.get(".tga").as_deref(), Some("tgafile"));
        assert!(!extension_registered(&sb.layout, "tga"));
        unregister(&sb.layout).unwrap();
        assert_eq!(sb.get(".tga").as_deref(), Some("tgafile"));
    }

    #[test]
    fn extension_toggle_rejects_bad_input_without_writing() {
        let sb = Sandbox::new("toggle-bad");
        let pristine = sb.snapshot();
        // Nothing registered: there is no class for the handler to point at.
        assert!(register_extension(&sb.layout, "png").is_err());
        assert_eq!(
            unregister_extension(&sb.layout, "png").unwrap(),
            Report::default()
        );
        assert_eq!(sb.snapshot(), pristine);

        register(&sb.layout, &plan_for(&dll(), &[])).unwrap();
        let registered = sb.snapshot();
        for bad in ["", "a\\b", "x/y", ".png", "..", "*", &"a".repeat(33)] {
            assert!(register_extension(&sb.layout, bad).is_err(), "{bad:?}");
            assert!(unregister_extension(&sb.layout, bad).is_err(), "{bad:?}");
            assert!(!extension_registered(&sb.layout, bad));
        }
        assert_eq!(sb.snapshot(), registered);
    }

    #[test]
    fn sync_extensions_reconciles_and_is_idempotent() {
        let sb = Sandbox::new("sync");
        register(&sb.layout, &plan_for(&dll(), &["png"])).unwrap();
        let universe = ["png", "svg", "tga", "bmp"];
        sync_extensions(&sb.layout, &["svg", "tga"], &universe).unwrap();
        let on: Vec<bool> = universe
            .iter()
            .map(|e| extension_registered(&sb.layout, e))
            .collect();
        assert_eq!(on, [false, true, true, false]);
        let settled = sb.snapshot();
        assert_eq!(
            sync_extensions(&sb.layout, &["svg", "tga"], &universe).unwrap(),
            Report::default()
        );
        assert_eq!(sb.snapshot(), settled);
        // An extension outside the universe is not this call's business.
        sync_extensions(&sb.layout, &[], &["svg"]).unwrap();
        assert!(extension_registered(&sb.layout, "tga"));
        assert!(!extension_registered(&sb.layout, "svg"));
    }

    #[test]
    fn crash_during_extension_registration_is_fully_undone_by_unregister() {
        let (first, last) = {
            let sb = Sandbox::new("xcount");
            register(&sb.layout, &plan_for(&dll(), &["png"])).unwrap();
            let first = sb.journal_len();
            register_extension(&sb.layout, "tga").unwrap();
            (first, sb.journal_len())
        };
        assert!(last > first);
        for n in first + 1..=last {
            let sb = Sandbox::new("xcrash");
            let pristine = sb.snapshot();
            register(&sb.layout, &plan_for(&dll(), &["png"])).unwrap();
            CRASH_AFTER.set(Some(n));
            let crashed = register_extension(&sb.layout, "tga");
            CRASH_AFTER.set(None);
            assert!(crashed.is_err(), "crash point {n} not reached");
            unregister(&sb.layout).unwrap();
            assert_eq!(sb.snapshot(), pristine, "crash after {n} journal entries");
        }
    }

    /// The registration `DllRegisterServer` performs, minus the HKLM root: `approve` on,
    /// every routable extension, both classes. Install, repair and upgrade (re-register over
    /// an older path) must each leave exactly one registration, and uninstall must restore
    /// the starting state, whether or not the `Approved` key pre-existed (it always does on a
    /// real machine, holding other vendors' entries).
    #[test]
    fn machine_scope_plan_registers_approves_and_unregisters_cleanly() {
        let extensions: Vec<&str> = crate::formats::all()
            .iter()
            .map(|f| f.ext)
            .filter(|e| !crate::decode::route::chain_for_ext(e).is_empty())
            .collect();
        assert!(
            extensions.len() > 100,
            "{} routable extensions",
            extensions.len()
        );
        let old_dll = PathBuf::from(r"C:\Program Files\Thumbwright\old\thumbwright_shell.dll");
        let new_dll = PathBuf::from(r"C:\Program Files\Thumbwright\thumbwright_shell.dll");
        let plan = |dll| Plan {
            dll_path: dll,
            extensions: extensions.clone(),
            context_menu: true,
            cloud: true,
            installer: Some(InstallerPlan {
                preview_extensions: vec!["png", "svg"],
                property_extensions: vec!["png", "qoi"],
            }),
        };
        let foreign = "{00000000-0000-0000-0000-0000000000AA}";
        for approved_preexists in [true, false] {
            let mut sb = Sandbox::new("machine");
            sb.layout.approve = true;
            sb.seed_user_state();
            if approved_preexists {
                CURRENT_USER
                    .create(&sb.layout.approved)
                    .unwrap()
                    .set_string(foreign, "Other vendor extension")
                    .unwrap();
            }
            let before = sb.snapshot();
            let approved = |name: &str| {
                CURRENT_USER
                    .open(&sb.layout.approved)
                    .ok()?
                    .get_string(name)
                    .ok()
            };

            register(&sb.layout, &plan(&old_dll)).unwrap();
            for class in [ids::CLSID_THUMBNAIL, ids::CLSID_CONTEXT_MENU] {
                assert_eq!(
                    approved(&class.braced()).as_deref(),
                    Some(class.display_name),
                    "{} approved",
                    class.display_name
                );
            }
            let handlers = CURRENT_USER
                .open(format!("{}\\Classes", sb.base))
                .unwrap()
                .keys()
                .unwrap()
                .filter(|k| k.starts_with('.'))
                .count();
            assert!(handlers >= extensions.len(), "{handlers} .ext keys");
            assert_eq!(approved(foreign).is_some(), approved_preexists);

            // Upgrade: a newer install registers over the older one.
            register(&sb.layout, &plan(&new_dll)).unwrap();
            let inproc = format!("CLSID\\{}\\InprocServer32", ids::CLSID_THUMBNAIL.braced());
            assert_eq!(sb.get(&inproc).as_deref(), Some(new_dll.to_str().unwrap()));
            let installed = sb.snapshot();
            // Repair: the same install registers again; nothing changes.
            register(&sb.layout, &plan(&new_dll)).unwrap();
            assert_eq!(sb.snapshot(), installed, "repair is idempotent");

            unregister(&sb.layout).unwrap();
            assert_eq!(
                sb.snapshot(),
                before,
                "uninstall leaves zero diff (Approved pre-existed: {approved_preexists})"
            );
            assert_eq!(approved(foreign).is_some(), approved_preexists);
        }
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
