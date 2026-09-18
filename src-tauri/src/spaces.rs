//! Keep the popover reachable from whatever the user is looking at (#58).
//!
//! Two things have to be true, and only one of them is a flag.
//!
//! **The window must be born into an accessory app.** A window created while the
//! process is still a regular app is pinned to the space it was created on for
//! the rest of its life, and no later `collectionBehavior` can move it. Measured
//! side by side, two windows with identical flags and level: the one created
//! under `.regular` and then switched to `.accessory` reports
//! `isOnActiveSpace == false` on a full-screen space; the one created after the
//! switch reports `true`. Tauri builds `"create": true` windows inside its own
//! setup, before the `setup` hook where `set_activation_policy` runs, so the
//! popover is declared `"create": false` and built by `build_popover` instead.
//! (`LSUIElement` in `Info.plist` does not help: `tao` sets the policy back to
//! regular when it creates the event loop, and the status item goes missing.)
//!
//! **And the window needs the right collection behaviour**, which is what this
//! module sets: `CanJoinAllSpaces` puts it on every desktop, `FullScreenAuxiliary`
//! lets it draw over another app's full-screen space (its own space, which
//! otherwise hosts only that app's windows). `tao` exposes the first only, via
//! `set_visible_on_all_workspaces`. On its own — the state shipped in v0.1.19 —
//! this changes nothing at all, because of the binding above.

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
/// Both flags we want have members they are mutually exclusive with, and AppKit
/// says nothing about which one wins when they are set together: `CanJoinAllSpaces`
/// rules out `MoveToActiveSpace`, and `FullScreenAuxiliary` rules out
/// `FullScreenPrimary` (a window that can itself *become* full screen, which the
/// popover never does) and `FullScreenNone`. `tao` sets none of the three today,
/// but the result should not depend on that.
#[cfg(target_os = "macos")]
fn across_all_spaces(current: NSWindowCollectionBehavior) -> NSWindowCollectionBehavior {
    let conflicting = NSWindowCollectionBehavior::MoveToActiveSpace
        | NSWindowCollectionBehavior::FullScreenPrimary
        | NSWindowCollectionBehavior::FullScreenNone;
    (current & !conflicting)
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
    fn the_flags_that_contradict_the_two_we_set_are_dropped() {
        for conflicting in [
            Behavior::MoveToActiveSpace,
            Behavior::FullScreenPrimary,
            Behavior::FullScreenNone,
        ] {
            let behavior = across_all_spaces(conflicting);
            assert!(!behavior.contains(conflicting), "{conflicting:?} survived");
            assert!(behavior.contains(Behavior::CanJoinAllSpaces));
            assert!(behavior.contains(Behavior::FullScreenAuxiliary));
        }
    }

    #[test]
    fn unrelated_flags_survive() {
        let behavior = across_all_spaces(Behavior::Managed | Behavior::IgnoresCycle);
        assert!(behavior.contains(Behavior::Managed));
        assert!(behavior.contains(Behavior::IgnoresCycle));
    }
}
