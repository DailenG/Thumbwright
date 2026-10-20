# Thumbwright: rules for coding agents

Read `CONTRIBUTING.md` first; its clean-room rule binds agents too.

## Visual acceptance (binding)

Every user-visible surface is part of the product, and "the tests pass" does not mean it looks right.

- **Any change to a visible surface needs a screenshot, compared against the intended look, before the change counts as done.** That covers:
  - the `twctl menu` popup and the classic menu
  - Workbench (`thumbwright-settings.exe`)
  - progress windows, message boxes and toasts
  - the MSI UI
  - the Windows 11 menu entries
  - the website and the README images
- **Compare against the design.** The website mockups in `site/index.html` and Windows 11's own menus are the reference. Look at alignment, spacing, clipping, light and dark mode, and 100% and 150% DPI.
- **Any visual oddity is a bug.** File it with the label `bug`. Never call it polish, a "styling difference" or a "follow-up".
- **Releases:** before tagging, capture every surface above on the release build and look at each image.
- **Windows 11 shell menus are invisible to UI Automation,** so use screen captures:
  - make the capturing process DPI-aware (`SetProcessDPIAware`)
  - right-click with `SetCursorPos` + `mouse_event`
  - capture with `Graphics.CopyFromScreen`
  - `twctl menu --at X,Y <file>` opens the popup directly
- **Subagent tasks that touch a visible surface must list the screenshot check in their acceptance criteria.**

## Gates

- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets --locked -- -D warnings`
- `cargo test --workspace --locked`
- `cargo xtask licenses --check`

Give each command a time limit of about 20 minutes. Builders run only the tests for the crates they touch. CI is the full gate.

## Working directory

Start agents from this folder (the repository root), not a parent folder, so this file loads.
