//! Exact-window guards for foreground pointer input. AX ownership and CG
//! hit testing are independent: a key window can still be physically covered.
use crate::ax::bindings::{
    ax_get_window_id, copy_ax_windows, perform_action, set_bool_attr_true,
    AXUIElementCreateApplication,
};
use crate::windows::WindowInfo;
use core_foundation::base::{CFRelease, CFTypeRef};
use std::cell::Cell;

/// Best-effort completion of the one exact-window request. This addresses only
/// the requested AX window; it never orders every window owned by the process.
pub(crate) fn raise_exact_ax_window(pid: i32, window_id: u32) -> bool {
    unsafe {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            return false;
        }
        let mut target = None;
        for window in copy_ax_windows(app) {
            if target.is_none() && ax_get_window_id(window) == Some(window_id) {
                target = Some(window);
            } else {
                CFRelease(window as CFTypeRef);
            }
        }
        CFRelease(app as CFTypeRef);
        let Some(target) = target else {
            return false;
        };
        let raised = perform_action(target, "AXRaise") == 0;
        let main = set_bool_attr_true(target, "AXMain") == 0;
        let focused = set_bool_attr_true(target, "AXFocused") == 0;
        CFRelease(target as CFTypeRef);
        raised || main || focused
    }
}

thread_local! {
    static TARGET: Cell<Option<(i32, u32)>> = const { Cell::new(None) };
}

struct TargetGuard(Option<(i32, u32)>);
impl Drop for TargetGuard {
    fn drop(&mut self) {
        TARGET.with(|slot| slot.set(self.0));
    }
}

/// Ask Accessibility for the actual hit recipient, not the first compositor
/// rectangle. Dock containers and input-transparent overlays cover rectangles
/// without receiving input there; real dialogs and panels remain recipients.
fn hit_window_at(x: f64, y: f64) -> Option<(i32, u32)> {
    use crate::ax::bindings::*;
    if !x.is_finite() || !y.is_finite() {
        return None;
    }
    unsafe {
        let system = AXUIElementCreateSystemWide();
        if system.is_null() {
            return None;
        }
        AXUIElementSetMessagingTimeout(system, 0.1);
        let mut element = std::ptr::null_mut();
        let error = AXUIElementCopyElementAtPosition(system, x as f32, y as f32, &mut element);
        CFRelease(system as CFTypeRef);
        if error != kAXErrorSuccess || element.is_null() {
            return None;
        }
        AXUIElementSetMessagingTimeout(element, 0.1);
        let mut pid = 0;
        let pid_ok = AXUIElementGetPid(element, &mut pid) == kAXErrorSuccess;
        let wid = crate::ax::exact_target::element_window_id(element);
        CFRelease(element as CFTypeRef);
        if pid_ok {
            wid.map(|wid| (pid, wid))
        } else {
            None
        }
    }
}

fn require_hit(
    windows: &[WindowInfo],
    target: (i32, u32),
    point: (f64, f64),
    observed: Option<(i32, u32)>,
) -> anyhow::Result<()> {
    let (x, y) = point;
    let live_target = windows.iter().any(|w| {
        let b = &w.bounds;
        (w.pid, w.window_id) == target
            && w.is_on_screen
            && b.width > 0.0
            && b.height > 0.0
            && x.is_finite()
            && y.is_finite()
            && x >= b.x
            && x < b.x + b.width
            && y >= b.y
            && y < b.y + b.height
    });
    if !live_target || observed != Some(target) {
        anyhow::bail!("foreground_pointer_target_obscured: expected pid {} window {} at ({x:.1},{y:.1}), native hit window is {observed:?}, live target at point={live_target}; no pointer event posted at this point",target.0,target.1);
    }
    Ok(())
}

pub(crate) fn guard_event_point(x: f64, y: f64) -> anyhow::Result<()> {
    if let Some(target) = TARGET.with(Cell::get) {
        require_hit(
            &crate::windows::pointer_hit_windows(),
            target,
            (x, y),
            hit_window_at(x, y),
        )?;
    }
    Ok(())
}

pub(crate) fn current_target() -> Option<(i32, u32)> {
    TARGET.with(Cell::get)
}

fn activate_pointer_target(pid: i32, wid: u32) -> anyhow::Result<()> {
    // NSApplication activation must agree with WindowServer before AppKit
    // starts mouse tracking. This does not order all application windows.
    crate::apps::activate_pid(pid);
    crate::input::skylight::make_exact_window_key(pid, wid);
    if !raise_exact_ax_window(pid, wid) {
        anyhow::bail!("foreground_pointer_raise_failed: could not raise exact window {pid}:{wid}");
    }
    Ok(())
}

/// Recover a proven point in the same target for error-path button release.
/// Never post an unguarded global mouse-up into the newly covering app.
pub(crate) fn release_point(pid: i32, wid: u32, last: (f64, f64)) -> anyhow::Result<(f64, f64)> {
    activate_pointer_target(pid, wid)?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(400);
    loop {
        let windows = crate::windows::pointer_hit_windows();
        let mut points = vec![last];
        if let Some(w) = windows.iter().find(|w| (w.pid, w.window_id) == (pid, wid)) {
            for fx in [0.5, 0.1, 0.9] {
                for fy in [0.5, 0.1, 0.9] {
                    points.push((
                        w.bounds.x + w.bounds.width * fx,
                        w.bounds.y + w.bounds.height * fy,
                    ));
                }
            }
        }
        for point in points {
            if require_hit(&windows, (pid, wid), point, hit_window_at(point.0, point.1)).is_ok() {
                return Ok(point);
            }
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!("could not prove a target point for mouse-up cleanup");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Raise only the addressed AX window, wait for all gesture endpoints to be
/// physically exposed, then maintain the event-level guard until completion.
pub(crate) fn with_target(
    pid: i32,
    wid: u32,
    points: &[(f64, f64)],
    action: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    activate_pointer_target(pid, wid)?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(400);
    let mut ready_since = None;
    loop {
        let windows = crate::windows::pointer_hit_windows();
        let result = points
            .iter()
            .try_for_each(|&(x, y)| require_hit(&windows, (pid, wid), (x, y), hit_window_at(x, y)));
        let exact_focus = crate::apps::frontmost_pid() == Some(pid)
            && crate::ax::bindings::focused_window_id_of_pid(pid) == Some(wid);
        match result {
            Ok(()) if exact_focus => {
                let first = ready_since.get_or_insert_with(std::time::Instant::now);
                if first.elapsed() >= std::time::Duration::from_millis(40) {
                    break;
                }
            }
            Err(error) if std::time::Instant::now() >= deadline => return Err(error),
            _ => ready_since = None,
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!("foreground pointer target did not acquire stable app/window focus");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    with_guarded_target((pid, wid), action)
}

fn with_guarded_target(
    target: (i32, u32),
    action: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let previous = TARGET.with(|slot| slot.replace(Some(target)));
    let _guard = TargetGuard(previous);
    action()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::windows::WindowBounds;
    fn window(pid: i32, id: u32, z: usize, layer: i32, x: f64) -> WindowInfo {
        WindowInfo {
            pid,
            window_id: id,
            z_index: z,
            layer,
            app_name: String::new(),
            title: String::new(),
            bounds: WindowBounds {
                x,
                y: 0.,
                width: 100.,
                height: 100.,
            },
            is_on_screen: true,
            current_space_id: None,
            on_current_space: None,
            space_ids: None,
        }
    }
    #[test]
    fn pointer_hit_refuses_other_apps_siblings_and_panels() {
        let target = window(1, 10, 1, 0, 0.);
        for blocker in [(2, 20), (1, 11), (1, 12)] {
            assert!(require_hit(&[target.clone()], (1, 10), (25., 25.), Some(blocker)).is_err());
        }
        assert!(require_hit(&[target], (1, 10), (25., 25.), Some((1, 10))).is_ok());
    }
    #[test]
    fn transparent_containers_do_not_override_native_hit_recipient() {
        let target = window(1, 10, 1, 0, 0.);
        let dock = window(2, 20, 2, 20, 0.);
        assert!(require_hit(&[target, dock], (1, 10), (25., 25.), Some((1, 10))).is_ok());
    }
    #[test]
    fn drag_endpoint_and_later_occlusion_are_independently_guarded() {
        let windows = [window(1, 10, 1, 0, 0.)];
        assert!(require_hit(&windows, (1, 10), (25., 25.), Some((1, 10))).is_ok());
        assert!(require_hit(&windows, (1, 10), (75., 25.), Some((2, 20))).is_err());
        assert!(require_hit(&windows, (1, 10), (25., 25.), Some((2, 21))).is_err());
    }
    #[test]
    fn missing_hidden_degenerate_and_nonfinite_targets_fail_closed() {
        let hit = Some((1, 10));
        assert!(require_hit(&[], (1, 10), (25., 25.), hit).is_err());
        let mut w = window(1, 10, 1, 0, 0.);
        w.is_on_screen = false;
        assert!(require_hit(&[w.clone()], (1, 10), (25., 25.), hit).is_err());
        w.is_on_screen = true;
        w.bounds.width = 0.;
        assert!(require_hit(&[w], (1, 10), (0., 25.), hit).is_err());
        let windows = [window(1, 10, 1, 0, 0.)];
        assert!(require_hit(&windows, (1, 10), (f64::NAN, 25.), hit).is_err());
        assert!(require_hit(&windows, (1, 10), (100., 25.), hit).is_err());
        assert!(require_hit(&windows, (1, 10), (25., 25.), None).is_err());
    }
    #[test]
    fn event_target_is_cleared_after_error() {
        assert!(with_guarded_target((1, 10), || {
            assert_eq!(current_target(), Some((1, 10)));
            Err(anyhow::anyhow!("dispatch failed"))
        })
        .is_err());
        assert_eq!(current_target(), None);
    }
}
