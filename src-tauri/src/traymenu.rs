//! macOS 27 workaround: present the tray menu ourselves instead of letting the
//! status item own it.
//!
//! `tray-icon` 0.24.x attaches the tray's `NSMenu` to the `NSStatusItem` once, at
//! build time, and leaves it there. On macOS 27 a status item that owns an
//! `NSMenu` stops forwarding clicks to its view at all (TN3212 gesture
//! recognizers; tauri-apps/tray-icon#355), so `TrayIconEvent::Click` never fires
//! and `show_menu_on_left_click(false)` is ignored — macOS just shows the menu.
//! On a tray-only app that means the popover, which is the entire UI, becomes
//! unreachable (#54).
//!
//! Upstream fixed this in tray-icon 0.25.1 by attaching the menu to the status
//! item only while it is being presented, but `tauri` 2.x requires
//! `tray-icon = "0.24"` and there is no 0.24 backport, so we cannot pull the fix
//! in. Instead we apply the same idea one level up: on affected systems the
//! status item is built with no menu — which is what restores the click events —
//! and this module presents the menu on right-click.
//!
//! Delete this module (and the branch in `lib.rs`) once tauri depends on
//! tray-icon 0.25: the native path is better in every way.

use tauri::menu::{ContextMenu, Menu};
use tauri::{Runtime, Window};

/// Overrides the OS check, so the workaround can be exercised on a macOS that
/// does not have the bug. Unset means "decide from the OS version".
const FORCE_ENV: &str = "OMNIROUTE_TRAY_DETACHED_MENU";

/// Whether the tray must be built *without* a menu of its own, leaving it to
/// [`present`] to show one.
pub fn detached() -> bool {
    match std::env::var(FORCE_ENV) {
        Ok(value) => forced(&value),
        Err(_) => os_swallows_clicks(),
    }
}

/// Show `menu` where the pointer is, standing in for the presentation the status
/// item would normally do itself.
///
/// `window` is only the owner AppKit needs for the menu; it is never shown, and
/// the menu is positioned at the pointer (i.e. over the tray icon), not over the
/// window. This blocks in AppKit's menu tracking loop until the menu closes, so
/// it must be called from the main thread — which is where tray events arrive.
pub fn present<R: Runtime>(window: Window<R>, menu: &Menu<R>) {
    highlight(true);
    if let Err(err) = menu.popup(window) {
        log::warn!("could not present the tray menu: {err}");
    }
    highlight(false);
}

/// Anything but an explicit "off" turns the workaround on, so that a bare
/// `OMNIROUTE_TRAY_DETACHED_MENU=1` does the obvious thing.
fn forced(value: &str) -> bool {
    !matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "" | "0" | "false" | "no" | "off"
    )
}

/// macOS 27 and later swallow the clicks; everything before it does not.
#[cfg(target_os = "macos")]
fn os_swallows_clicks() -> bool {
    use objc2_foundation::{NSOperatingSystemVersion, NSProcessInfo};

    NSProcessInfo::processInfo().isOperatingSystemAtLeastVersion(NSOperatingSystemVersion {
        majorVersion: 27,
        minorVersion: 0,
        patchVersion: 0,
    })
}

#[cfg(not(target_os = "macos"))]
fn os_swallows_clicks() -> bool {
    false
}

/// Light up the status item for as long as our menu is open.
///
/// `tray-icon` highlights the item on *any* mouse-down but only clears it from
/// its left-button `mouseUp:` handler — the right-button one does not — because
/// it assumes a right-click is handled by AppKit presenting the attached menu,
/// which restores the item itself. With no menu attached that assumption breaks
/// and the icon would stay lit until the next left-click, so we drive it here.
#[cfg(target_os = "macos")]
fn highlight(on: bool) {
    use objc2::rc::Retained;
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSStatusBarButton};

    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    // AppKit offers no way to enumerate status items, but this process owns
    // exactly one and its button is the content view of a status bar window.
    let button = NSApplication::sharedApplication(mtm)
        .windows()
        .iter()
        .filter_map(|window| window.contentView())
        .find_map(|view| Retained::downcast::<NSStatusBarButton>(view).ok());
    if let Some(button) = button {
        button.highlight(on);
    }
}

#[cfg(not(target_os = "macos"))]
fn highlight(_on: bool) {}

#[cfg(test)]
mod tests {
    use super::forced;

    #[test]
    fn explicit_off_values_keep_the_native_menu() {
        for value in ["", " ", "0", "false", "FALSE", "no", "off"] {
            assert!(!forced(value), "{value:?} should keep the native menu");
        }
    }

    #[test]
    fn anything_else_detaches_the_menu() {
        for value in ["1", "true", "yes", " on ", "please"] {
            assert!(forced(value), "{value:?} should detach the menu");
        }
    }
}
