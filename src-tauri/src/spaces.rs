//! Keep the popover reachable from whatever the user is looking at (#58).
//!
//! An AppKit window lives on the space it was created on. The popover is built
//! once, at launch, so it stayed bound to the space the app started on: clicking
//! the tray icon while a full-screen app (or another desktop) was in front either
//! did nothing visible or yanked the user back to the original space, even though
//! the tray icon itself is on every space.
//!
//! Two collection-behaviour flags fix that, and `tao` only exposes the first:
//! `CanJoinAllSpaces` puts the window on every desktop, and `FullScreenAuxiliary`
//! lets it draw over another app's full-screen space (which is its own space, and
//! otherwise only hosts that app's windows).

#[cfg(target_os = "macos")]
use objc2_app_kit::NSWindowCollectionBehavior;
use tauri::{Runtime, WebviewWindow};

/// Make `window` appear on the active space, full-screen spaces included.
///
/// Must be called once per window *object* — the flags survive hide/show, but
/// not the rebuild in `recreate_popover`.
#[cfg(target_os = "macos")]
pub fn follow_active_space<R: Runtime>(window: &WebviewWindow<R>) {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSWindow;

    if MainThreadMarker::new().is_none() {
        log::warn!("not on the main thread; leaving the popover bound to one space");
        return;
    }
    let Ok(ptr) = window.ns_window() else {
        log::warn!("no NSWindow behind the popover; leaving it bound to one space");
        return;
    };
    // SAFETY: `ns_window` hands back the window's own NSWindow, which outlives
    // this borrow, and we are on the main thread.
    let ns_window: &NSWindow = unsafe { &*ptr.cast::<NSWindow>() };
    ns_window.setCollectionBehavior(across_all_spaces(ns_window.collectionBehavior()));
}

#[cfg(not(target_os = "macos"))]
pub fn follow_active_space<R: Runtime>(_window: &WebviewWindow<R>) {}

/// The behaviour flags of a window that shows up wherever the user is.
///
/// `FullScreenPrimary` is cleared because it is mutually exclusive with
/// `FullScreenAuxiliary`: it marks a window that can itself *become* full screen,
/// which the popover never does.
#[cfg(target_os = "macos")]
fn across_all_spaces(current: NSWindowCollectionBehavior) -> NSWindowCollectionBehavior {
    (current & !NSWindowCollectionBehavior::FullScreenPrimary)
        | NSWindowCollectionBehavior::CanJoinAllSpaces
        | NSWindowCollectionBehavior::FullScreenAuxiliary
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::across_all_spaces;
    use objc2_app_kit::NSWindowCollectionBehavior as Behavior;

    #[test]
    fn a_default_window_joins_every_space_and_full_screen() {
        let behavior = across_all_spaces(Behavior::Default);
        assert!(behavior.contains(Behavior::CanJoinAllSpaces));
        assert!(behavior.contains(Behavior::FullScreenAuxiliary));
    }

    #[test]
    fn the_exclusive_full_screen_primary_flag_is_dropped() {
        let behavior = across_all_spaces(Behavior::FullScreenPrimary);
        assert!(!behavior.contains(Behavior::FullScreenPrimary));
        assert!(behavior.contains(Behavior::FullScreenAuxiliary));
    }

    #[test]
    fn unrelated_flags_survive() {
        let behavior = across_all_spaces(Behavior::Managed | Behavior::IgnoresCycle);
        assert!(behavior.contains(Behavior::Managed));
        assert!(behavior.contains(Behavior::IgnoresCycle));
    }
}
