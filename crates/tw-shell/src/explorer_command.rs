//! Windows 11 modern context menu: `IExplorerCommand` (+ `IEnumExplorerCommand` for the
//! submenus), declared by the sparse package in `packaging/msix`. It mirrors the classic
//! menu's tree (`menu::TREE`, so the two never drift apart) and launches `twctl.exe` through
//! the same `menu::launch`; like the classic menu it never decodes images (REQ-ISO-4).
//!
//! `IExplorerCommand` has no owner-draw item, so the classic menu's thumbnail preview row is
//! the one entry this menu does not have.
//!
//! `EnumSubCommands` receives no selection, so every child decides its own visibility in
//! `GetState`, which does: hidden items, single-file-only items, and separators that would
//! lead, trail or double up.

use std::cell::Cell;
use std::path::PathBuf;

use windows::Win32::Foundation::{E_INVALIDARG, E_NOTIMPL, E_POINTER, HWND, S_FALSE, S_OK};
use windows::Win32::System::Com::{CoTaskMemFree, IBindCtx};
use windows::Win32::UI::Shell::{
    ECF_DEFAULT, ECF_HASSUBCOMMANDS, ECF_ISSEPARATOR, ECS_ENABLED, ECS_HIDDEN,
    IEnumExplorerCommand, IEnumExplorerCommand_Impl, IExplorerCommand, IExplorerCommand_Impl,
    IShellItemArray, SHStrDupW, SIGDN_FILESYSPATH,
};
use windows::core::{BOOL, GUID, HRESULT, HSTRING, PWSTR, Ref, Result, implement};

use tw_core::config::Settings;
use tw_core::ids;

use crate::ModuleRef;
use crate::menu::{Node, TREE, Verb, launch, menu_applies};

/// One command in the tree.
#[derive(Clone, Copy)]
enum Entry {
    /// The root, "Thumbwright".
    Top,
    /// A direct child of the root.
    Node(&'static Node),
    /// A verb inside a submenu node.
    Item(&'static Verb),
}

fn dup(s: &str) -> Result<PWSTR> {
    // SAFETY: SHStrDupW returns a CoTaskMem copy; ownership passes to the caller (the shell).
    unsafe { SHStrDupW(&HSTRING::from(s)) }
}

/// File-system paths of `items`; `None` when any item has no file-system path (a virtual
/// item cannot be handed to `twctl`).
fn paths(items: Option<&IShellItemArray>) -> Option<Vec<PathBuf>> {
    let items = items?;
    // SAFETY: plain shell item queries; the display-name buffer is freed after copying.
    unsafe {
        let n = items.GetCount().ok()?;
        let mut out = Vec::with_capacity(n as usize);
        for i in 0..n {
            let name = items
                .GetItemAt(i)
                .ok()?
                .GetDisplayName(SIGDN_FILESYSPATH)
                .ok()?;
            let path = name.to_string().ok();
            CoTaskMemFree(Some(name.0 as *const _));
            out.push(PathBuf::from(path?));
        }
        Some(out)
    }
}

/// The nodes of the tree that show for this selection, with separators collapsed
/// (none leading, trailing or doubled), as the classic menu builds it.
fn visible_nodes(settings: &Settings, single: bool) -> Vec<&'static Node> {
    let hidden = |id: &str| settings.hidden_menu_items.iter().any(|h| h == id);
    let mut out: Vec<&'static Node> = Vec::new();
    for node in TREE {
        let show = match node {
            Node::Separator => !matches!(out.last(), None | Some(Node::Separator)),
            Node::Leaf {
                id,
                single: needs_one,
                ..
            }
            | Node::Sub {
                id,
                single: needs_one,
                ..
            } => !hidden(id) && !(*needs_one && !single),
        };
        if show {
            out.push(node);
        }
    }
    if matches!(out.last(), Some(Node::Separator)) {
        out.pop();
    }
    out
}

impl Entry {
    fn title(self) -> &'static str {
        match self {
            Self::Top => ids::PRODUCT_NAME,
            Self::Node(Node::Leaf { verb, .. }) | Self::Item(verb) => verb.label,
            Self::Node(Node::Sub { label, .. }) => label,
            Self::Node(Node::Separator) => "",
        }
    }

    fn flags(self) -> i32 {
        match self {
            Self::Top | Self::Node(Node::Sub { .. }) => ECF_HASSUBCOMMANDS.0,
            Self::Node(Node::Separator) => ECF_ISSEPARATOR.0,
            _ => ECF_DEFAULT.0,
        }
    }

    fn state(self, items: Option<&IShellItemArray>) -> u32 {
        let visible = match self {
            // Inside a submenu the parent already gated visibility.
            Self::Item(_) => true,
            Self::Top | Self::Node(_) => {
                let settings = Settings::load(&crate::install_dir());
                match paths(items) {
                    Some(files) if menu_applies(&files, &settings) => match self {
                        Self::Node(node) => visible_nodes(&settings, files.len() == 1)
                            .iter()
                            .any(|v| std::ptr::eq(*v, node)),
                        _ => true,
                    },
                    _ => false,
                }
            }
        };
        (if visible { ECS_ENABLED } else { ECS_HIDDEN }).0 as u32
    }

    fn children(self) -> Vec<Entry> {
        match self {
            Self::Top => TREE.iter().map(Self::Node).collect(),
            Self::Node(Node::Sub { items, .. }) => items.iter().map(Self::Item).collect(),
            _ => Vec::new(),
        }
    }

    fn verb(self) -> Option<&'static Verb> {
        match self {
            Self::Node(Node::Leaf { verb, .. }) | Self::Item(verb) => Some(verb),
            _ => None,
        }
    }
}

#[implement(IExplorerCommand)]
pub(crate) struct ExplorerCommand {
    entry: Entry,
    _module: ModuleRef,
}

impl ExplorerCommand {
    /// The root command, what the class factory creates.
    pub(crate) fn new() -> Self {
        Self::of(Entry::Top)
    }

    fn of(entry: Entry) -> Self {
        Self {
            entry,
            _module: ModuleRef::new(),
        }
    }
}

impl IExplorerCommand_Impl for ExplorerCommand_Impl {
    fn GetTitle(&self, _items: Ref<IShellItemArray>) -> Result<PWSTR> {
        dup(self.entry.title())
    }

    fn GetIcon(&self, _items: Ref<IShellItemArray>) -> Result<PWSTR> {
        Err(E_NOTIMPL.into())
    }

    fn GetToolTip(&self, _items: Ref<IShellItemArray>) -> Result<PWSTR> {
        Err(E_NOTIMPL.into())
    }

    fn GetCanonicalName(&self) -> Result<GUID> {
        match self.entry {
            Entry::Top => Ok(GUID::from_u128(ids::CLSID_EXPLORER_COMMAND.guid)),
            _ => Err(E_NOTIMPL.into()),
        }
    }

    fn GetState(&self, items: Ref<IShellItemArray>, _ok_to_be_slow: BOOL) -> Result<u32> {
        Ok(self.entry.state(items.as_ref()))
    }

    fn Invoke(&self, items: Ref<IShellItemArray>, _bind: Ref<IBindCtx>) -> Result<()> {
        let verb = self.entry.verb().ok_or(E_NOTIMPL)?;
        let files = paths(items.as_ref())
            .filter(|f| !f.is_empty())
            .ok_or(E_INVALIDARG)?;
        launch(verb, &files, HWND::default())
    }

    fn GetFlags(&self) -> Result<u32> {
        Ok(self.entry.flags() as u32)
    }

    fn EnumSubCommands(&self) -> Result<IEnumExplorerCommand> {
        let children = self.entry.children();
        if children.is_empty() {
            return Err(E_NOTIMPL.into());
        }
        Ok(CommandEnum::new(children).into())
    }
}

#[implement(IEnumExplorerCommand)]
struct CommandEnum {
    children: Vec<Entry>,
    next: Cell<usize>,
    _module: ModuleRef,
}

impl CommandEnum {
    fn new(children: Vec<Entry>) -> Self {
        Self {
            children,
            next: Cell::new(0),
            _module: ModuleRef::new(),
        }
    }
}

impl IEnumExplorerCommand_Impl for CommandEnum_Impl {
    fn Next(
        &self,
        celt: u32,
        commands: *mut Option<IExplorerCommand>,
        fetched: *mut u32,
    ) -> HRESULT {
        if commands.is_null() && celt != 0 {
            return E_POINTER;
        }
        let start = self.next.get();
        let n = (celt as usize).min(self.children.len() - start);
        for (i, entry) in self.children[start..start + n].iter().enumerate() {
            // SAFETY: the caller provides room for `celt` slots, uninitialised, so each
            // slot is written without dropping its previous contents.
            unsafe {
                commands
                    .add(i)
                    .write(Some(ExplorerCommand::of(*entry).into()))
            };
        }
        self.next.set(start + n);
        if !fetched.is_null() {
            // SAFETY: optional out-pointer, checked non-null.
            unsafe { *fetched = n as u32 };
        }
        if n == celt as usize { S_OK } else { S_FALSE }
    }

    fn Skip(&self, celt: u32) -> Result<()> {
        let to = self.next.get().saturating_add(celt as usize);
        self.next.set(to.min(self.children.len()));
        if to <= self.children.len() {
            Ok(())
        } else {
            Err(S_FALSE.into())
        }
    }

    fn Reset(&self) -> Result<()> {
        self.next.set(0);
        Ok(())
    }

    fn Clone(&self) -> Result<IEnumExplorerCommand> {
        let copy = CommandEnum::new(self.children.clone());
        copy.next.set(self.next.get());
        Ok(copy.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(nodes: &[&'static Node]) -> Vec<&'static str> {
        nodes
            .iter()
            .map(|n| Entry::Node(n).title())
            .collect::<Vec<_>>()
    }

    #[test]
    fn separators_never_lead_trail_or_double() {
        let nodes = visible_nodes(&Settings::default(), true);
        assert!(!matches!(nodes.first(), Some(Node::Separator)));
        assert!(!matches!(nodes.last(), Some(Node::Separator)));
        assert!(
            nodes
                .windows(2)
                .all(|w| !(matches!(w[0], Node::Separator) && matches!(w[1], Node::Separator)))
        );
    }

    #[test]
    fn single_only_items_drop_out_of_multi_selection() {
        let all = labels(&visible_nodes(&Settings::default(), true));
        let multi = labels(&visible_nodes(&Settings::default(), false));
        assert!(all.contains(&"Copy to clipboard") && !multi.contains(&"Copy to clipboard"));
        assert!(multi.contains(&"Convert into"));
    }

    #[test]
    fn hidden_items_are_dropped_and_their_separators_collapse() {
        let settings = Settings {
            hidden_menu_items: ["copy", "copy_data_uri"].map(String::from).to_vec(),
            ..Settings::default()
        };
        let nodes = visible_nodes(&settings, true);
        let l = labels(&nodes);
        assert!(!l.contains(&"Copy to clipboard") && !l.contains(&"Copy as data URI"));
        assert!(
            nodes
                .windows(2)
                .all(|w| !(matches!(w[0], Node::Separator) && matches!(w[1], Node::Separator)))
        );
    }
}
