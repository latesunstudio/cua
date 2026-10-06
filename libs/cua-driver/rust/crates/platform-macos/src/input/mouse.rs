//! Background mouse event synthesis via SLEventPostToPid (SkyLight SPI),
//! with fallback to the public CGEvent::post_to_pid for older OS releases.
//!
//! SLEventPostToPid goes through the IOHIDPostEvent path which:
//! - Triggers CGSTickleActivityMonitor (required for Catalyst / Chromium)
//! - Reaches Mac Catalyst windows that CGEventPostToPid misses
//!
//! Mouse events do NOT attach the SLSEventAuthenticationMessage envelope
//! (Swift reference: `attachAuthMessage: false`) — the auth envelope routes
//! events through a direct Mach delivery path that bypasses
//! cgAnnotatedSessionEventTap, which Chromium's window handler subscribes to.

use core_graphics::{
    event::{CGEvent, CGEventFlags, CGEventType, CGMouseButton},
    event_source::{CGEventSource, CGEventSourceStateID},
    geometry::CGPoint,
};
use foreign_types::ForeignType;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MousePostMode {
    Both,
    PublicOnly,
    HidOnly,
}

#[derive(Clone, Copy)]
pub enum WindowClickDelivery {
    Background,
    Foreground,
}

impl WindowClickDelivery {
    pub fn from_foreground(foreground: bool) -> Self {
        if foreground {
            Self::Foreground
        } else {
            Self::Background
        }
    }
}

/// Left-click at `(x, y)` screen coordinates, posted to `pid`.
///
/// Window-local coordinates for backgrounded targets: if `window_local` is
/// `Some((wx, wy))`, stamps a window-local point via `CGEventSetWindowLocation`
/// SPI so WindowServer's hit-test uses the local point directly.
pub fn click_at_xy(
    pid: i32,
    x: f64,
    y: f64,
    count: usize,
    modifiers: &[&str],
) -> anyhow::Result<()> {
    click_at_xy_inner(
        pid,
        (x, y),
        None,
        None,
        count,
        modifiers,
        MousePostMode::Both,
    )
}

/// Screen-absolute click posted to the GLOBAL HID tap (`CGEventTapLocation::HID`),
/// NOT routed to any pid — the OS delivers it to whichever window owns the
/// screen point, the macOS analogue of Windows' `WindowFromPoint` + `SendInput`
/// desktop-scope click. This backs the `capture_scope="desktop"`, window-less
/// (no pid/window_id) branch of the `click` tool: the agent has located the
/// target by vision in `get_desktop_state` and clicks true screen pixels.
///
/// Unlike the pid-routed `click_at_xy`, this honors the real foreground/Z order
/// (it lands on whatever is visually on top at the point) — exactly the
/// foreground, vision-driven model that complements the background contract.
pub fn click_at_xy_desktop(x: f64, y: f64, count: usize, button: &str) -> anyhow::Result<()> {
    click_at_xy_desktop_inner(x, y, count, button, &[], false)
}

/// Global HID click with physical modifier transitions. The caller must guard
/// the exact foreground window because this transport has no pid addressing.
pub fn click_at_xy_desktop_with_modifiers(
    x: f64,
    y: f64,
    count: usize,
    button: &str,
    modifiers: &[&str],
) -> anyhow::Result<()> {
    click_at_xy_desktop_inner(x, y, count, button, modifiers, false)
}

/// Global HID modifier click that restores the user's hardware cursor after
/// the event pair has been queued.
pub fn click_at_xy_desktop_with_modifiers_preserving_cursor(
    x: f64,
    y: f64,
    count: usize,
    button: &str,
    modifiers: &[&str],
) -> anyhow::Result<()> {
    click_at_xy_desktop_inner(x, y, count, button, modifiers, true)
}

fn click_at_xy_desktop_inner(
    x: f64,
    y: f64,
    count: usize,
    button: &str,
    modifiers: &[&str],
    preserve_cursor: bool,
) -> anyhow::Result<()> {
    super::pointer::guard_event_point(x, y)?;
    use core_graphics::display::CGDisplay;
    use core_graphics::event::CGEventTapLocation;
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|_| anyhow::anyhow!("CGEventSource::new failed"))?;
    let prior = if preserve_cursor {
        Some(
            CGEvent::new(source.clone())
                .map_err(|_| anyhow::anyhow!("CGEvent::new failed"))?
                .location(),
        )
    } else {
        None
    };
    let point = CGPoint::new(x, y);
    let (down_ty, up_ty, btn) = match button {
        "right" => (
            CGEventType::RightMouseDown,
            CGEventType::RightMouseUp,
            CGMouseButton::Right,
        ),
        "middle" => (
            CGEventType::OtherMouseDown,
            CGEventType::OtherMouseUp,
            CGMouseButton::Center,
        ),
        _ => (
            CGEventType::LeftMouseDown,
            CGEventType::LeftMouseUp,
            CGMouseButton::Left,
        ),
    };
    // Warp the REAL cursor to the point first (the macOS peer of Linux XTest's
    // pointer warp). A synthetic MouseMoved event does not relocate the hardware
    // cursor, and AppKit hit-tests some clicks against the actual cursor
    // position, so without the warp the down/up can miss the target. Desktop
    // scope is the foreground modality, so moving the visible cursor is expected.
    let _ = CGDisplay::warp_mouse_cursor_position(point);
    // Re-couple cursor + mouse-delta so the synthesized click hit-tests at the
    // warped point, not the pre-warp one.
    unsafe { CGAssociateMouseAndMouseCursorPosition(true) };
    std::thread::sleep(std::time::Duration::from_millis(40));
    let result = super::keyboard::with_global_modifier_keys(modifiers, |flags| {
        if !modifiers.is_empty() {
            // Prime AppKit's cursor-tracking state after the physical modifiers
            // are down so the modified mouseDown preserves the existing
            // selection. Leave ordinary clicks on their established event path.
            let moved = CGEvent::new_mouse_event(
                source.clone(),
                CGEventType::MouseMoved,
                point,
                CGMouseButton::Left,
            )
            .map_err(|_| anyhow::anyhow!("CGEvent::new_mouse_event(move) failed"))?;
            moved.set_flags(flags);
            super::pointer::guard_event_point(x, y)?;
            moved.post(CGEventTapLocation::HID);
            std::thread::sleep(std::time::Duration::from_millis(12));
        }

        for pair_index in 0..count.max(1) {
            super::pointer::guard_event_point(x, y)?;
            let up = CGEvent::new_mouse_event(source.clone(), up_ty, point, btn)
                .map_err(|_| anyhow::anyhow!("CGEvent::new_mouse_event(up) failed"))?;
            let down = CGEvent::new_mouse_event(source.clone(), down_ty, point, btn)
                .map_err(|_| anyhow::anyhow!("CGEvent::new_mouse_event(down) failed"))?;
            down.set_flags(flags);
            down.set_integer_value_field(
                core_graphics::event::EventField::MOUSE_EVENT_CLICK_STATE,
                (pair_index + 1) as i64,
            );
            down.post(CGEventTapLocation::HID);
            std::thread::sleep(std::time::Duration::from_millis(28));
            if let Err(error) = super::pointer::guard_event_point(x, y) {
                if let Some((pid, wid)) = super::pointer::current_target() {
                    if let Err(cleanup) = release_guarded_button(pid, wid, up_ty, btn, point) {
                        return Err(anyhow::anyhow!(
                            "{error}; button release cleanup failed: {cleanup}"
                        ));
                    }
                }
                return Err(error);
            }
            up.set_flags(flags);
            up.set_integer_value_field(
                core_graphics::event::EventField::MOUSE_EVENT_CLICK_STATE,
                (pair_index + 1) as i64,
            );
            up.post(CGEventTapLocation::HID);
            if count > 1 {
                std::thread::sleep(std::time::Duration::from_millis(80));
            }
        }
        Ok(())
    });

    // Let AppKit consume the up event before the pointer is restored. The
    // exact foreground guard remains active around this entire helper.
    std::thread::sleep(std::time::Duration::from_millis(40));
    if let Some(prior) = prior {
        let _ = CGDisplay::warp_mouse_cursor_position(prior);
        unsafe { CGAssociateMouseAndMouseCursorPosition(true) };
    }
    result
}

/// Cancel tracking in the addressed app, then release only at a newly proven
/// point in the same window. Used after a foreground gesture loses its hit.
fn release_guarded_button(
    pid: i32,
    wid: u32,
    up_type: CGEventType,
    button: CGMouseButton,
    last: CGPoint,
) -> anyhow::Result<()> {
    let _ = super::keyboard::press_key_no_auth(pid, "Escape", &[]);
    let (x, y) = super::pointer::release_point(pid, wid, (last.x, last.y))?;
    super::pointer::guard_event_point(x, y)?;
    let event = new_global_mouse_event(up_type, CGPoint::new(x, y), button)
        .map_err(|_| anyhow::anyhow!("mouse-up cleanup construction failed"))?;
    event.post(core_graphics::event::CGEventTapLocation::HID);
    std::thread::sleep(std::time::Duration::from_millis(40));
    Ok(())
}

/// Move the real hardware cursor to a logical desktop point.
pub fn move_cursor_desktop(x: f64, y: f64) -> anyhow::Result<()> {
    use core_graphics::display::CGDisplay;
    let point = CGPoint::new(x, y);
    CGDisplay::warp_mouse_cursor_position(point)
        .map_err(|error| anyhow::anyhow!("CGWarpMouseCursorPosition failed: {error:?}"))?;
    unsafe { CGAssociateMouseAndMouseCursorPosition(true) };
    Ok(())
}

/// Scroll the foreground desktop surface at a logical screen point through the
/// global HID queue. Mirrors computer-server's pynput wheel behavior while
/// preserving cua-driver's explicit direction/amount contract.
pub fn scroll_wheel_desktop(
    x: f64,
    y: f64,
    delta_y_per_tick: i32,
    delta_x_per_tick: i32,
    ticks: usize,
) -> anyhow::Result<()> {
    use core_graphics::event::{CGEventTapLocation, ScrollEventUnit};

    move_cursor_desktop(x, y)?;
    std::thread::sleep(std::time::Duration::from_millis(40));
    for _ in 0..ticks.max(1) {
        // AppKit does not reliably consume synthetic LINE-unit events posted
        // through the global HID queue. pynput's proven macOS desktop path uses
        // PIXEL units, a null source, and scales each logical wheel notch to ten
        // pixels. Keep that exact controller convention instead of attaching
        // synthetic source state unrelated to the physical pointer we warped.
        let wheel_y = (delta_y_per_tick / 12).clamp(-100, 100);
        let wheel_x = (delta_x_per_tick / 12).clamp(-100, 100);
        let event_ref = unsafe {
            CGEventCreateScrollWheelEvent2(
                std::ptr::null_mut(),
                ScrollEventUnit::PIXEL,
                2,
                wheel_y,
                wheel_x,
                0,
            )
        };
        if event_ref.is_null() {
            return Err(anyhow::anyhow!("CGEventCreateScrollWheelEvent2 failed"));
        }
        let event = unsafe { CGEvent::from_ptr(event_ref) };
        event.post(CGEventTapLocation::HID);
        std::thread::sleep(std::time::Duration::from_millis(30));
    }
    Ok(())
}

/// Click one exact desktop point, then restore the user's cursor position.
///
/// This is intentionally narrower than the public desktop click path. It is
/// used only by approved, bounded setup flows after an accessibility element
/// has proven the exact target point and window.
pub fn click_at_xy_desktop_preserving_cursor(x: f64, y: f64) -> anyhow::Result<()> {
    click_at_xy_desktop_inner(x, y, 1, "left", &[], true)
}

extern "C" {
    /// Quartz's non-variadic scroll-event constructor. The public desktop
    /// path intentionally passes a null source to match real mouse-controller
    /// libraries such as pynput.
    fn CGEventCreateScrollWheelEvent2(
        source: core_graphics::sys::CGEventSourceRef,
        units: core_graphics::event::CGScrollEventUnit,
        wheel_count: u32,
        wheel1: i32,
        wheel2: i32,
        wheel3: i32,
    ) -> core_graphics::sys::CGEventRef;

    /// Reconnect the mouse-delta stream to the (just-warped) cursor position so a
    /// synthesized click hit-tests at the new location, not the pre-warp one.
    fn CGAssociateMouseAndMouseCursorPosition(connected: bool) -> i32;
}

/// Like `click_at_xy` but also targets a specific window.
///
/// Both arms post one public PID route. `CGEventSetWindowLocation` is the
/// exact window-local point, and the leading `mouseMoved` is at that target.
/// Background delivery does not activate the process, warp the pointer, or
/// post a SkyLight event. Foreground HID clicks are a separate desktop route
/// and do not come through this function.
// The flattened arguments mirror the native event fields used by existing callers.
#[allow(clippy::too_many_arguments)]
pub fn click_at_xy_with_window_local(
    pid: i32,
    x: f64,
    y: f64,
    wx: f64,
    wy: f64,
    wid: u32,
    count: usize,
    modifiers: &[&str],
    delivery: WindowClickDelivery,
) -> anyhow::Result<()> {
    match select_window_left_click(delivery, (x, y), (wx, wy), count) {
        WindowLeftClickSelection::BackgroundPublic(route) => {
            deliver_background_window_left_click(pid, wid, modifiers, route)
        }
        WindowLeftClickSelection::ForegroundPublic {
            screen,
            local,
            count,
            post_mode,
        } => click_at_xy_inner(
            pid,
            screen,
            Some(local),
            Some(wid),
            count,
            modifiers,
            post_mode,
        ),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LeftClickEventKind {
    Move,
    Down,
    Up,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct LeftClickEvent {
    kind: LeftClickEventKind,
    /// Screen point passed to `CGEventCreateMouseEvent`.
    screen: (f64, f64),
    /// Exact point passed to `CGEventSetWindowLocation`.
    window_local: Option<(f64, f64)>,
    click_state: i64,
}

/// One in-target `mouseMoved`, then `count` down/up pairs. No off-screen primer.
fn public_left_click_events(
    screen: (f64, f64),
    window_local: Option<(f64, f64)>,
    count: usize,
) -> Vec<LeftClickEvent> {
    let mut events = Vec::with_capacity(1 + count.saturating_mul(2));
    events.push(LeftClickEvent {
        kind: LeftClickEventKind::Move,
        screen,
        window_local,
        click_state: 0,
    });
    for pair_index in 0..count {
        let click_state = (pair_index + 1) as i64;
        events.push(LeftClickEvent {
            kind: LeftClickEventKind::Down,
            screen,
            window_local,
            click_state,
        });
        events.push(LeftClickEvent {
            kind: LeftClickEventKind::Up,
            screen,
            window_local,
            click_state,
        });
    }
    events
}

#[derive(Clone, Debug, PartialEq)]
struct BackgroundWindowLeftRoute {
    screen: (f64, f64),
    local: (f64, f64),
    count: usize,
    post_mode: MousePostMode,
    activates_target: bool,
    events: Vec<LeftClickEvent>,
}

fn background_window_left_route(
    screen_x: f64,
    screen_y: f64,
    local_x: f64,
    local_y: f64,
    count: usize,
) -> BackgroundWindowLeftRoute {
    let screen = (screen_x, screen_y);
    let local = (local_x, local_y);
    BackgroundWindowLeftRoute {
        screen,
        local,
        count,
        post_mode: MousePostMode::PublicOnly,
        activates_target: false,
        events: public_left_click_events(screen, Some(local), count),
    }
}

enum WindowLeftClickSelection {
    BackgroundPublic(BackgroundWindowLeftRoute),
    ForegroundPublic {
        screen: (f64, f64),
        local: (f64, f64),
        count: usize,
        post_mode: MousePostMode,
    },
}

/// Branch the production window-left poster uses.
fn select_window_left_click(
    delivery: WindowClickDelivery,
    screen: (f64, f64),
    local: (f64, f64),
    count: usize,
) -> WindowLeftClickSelection {
    match delivery {
        WindowClickDelivery::Background => WindowLeftClickSelection::BackgroundPublic(
            background_window_left_route(screen.0, screen.1, local.0, local.1, count),
        ),
        WindowClickDelivery::Foreground => WindowLeftClickSelection::ForegroundPublic {
            screen,
            local,
            count,
            post_mode: MousePostMode::PublicOnly,
        },
    }
}

fn deliver_background_window_left_click(
    pid: i32,
    wid: u32,
    modifiers: &[&str],
    route: BackgroundWindowLeftRoute,
) -> anyhow::Result<()> {
    if route.activates_target || route.post_mode != MousePostMode::PublicOnly {
        anyhow::bail!("background left click refused a non-public activation route");
    }
    post_planned_left_click(
        pid,
        route.screen,
        Some(wid),
        route.count,
        modifiers,
        route.post_mode,
        route.events,
    )
}

/// `screen` is the click point in screen coordinates.
fn click_at_xy_inner(
    pid: i32,
    screen: (f64, f64),
    window_local: Option<(f64, f64)>,
    wid: Option<u32>,
    count: usize,
    modifiers: &[&str],
    post_mode: MousePostMode,
) -> anyhow::Result<()> {
    let events = public_left_click_events(screen, window_local, count);
    post_planned_left_click(pid, screen, wid, count, modifiers, post_mode, events)
}

fn post_planned_left_click(
    pid: i32,
    screen: (f64, f64),
    wid: Option<u32>,
    count: usize,
    modifiers: &[&str],
    post_mode: MousePostMode,
    events: Vec<LeftClickEvent>,
) -> anyhow::Result<()> {
    let (x, y) = screen;
    super::pointer::guard_event_point(x, y)?;
    use std::time::{SystemTime, UNIX_EPOCH};

    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|_| anyhow::anyhow!("CGEventSource::new failed"))?;
    let flags = parse_modifier_flags(modifiers);

    // Shared click-group ID (f58) when window_id is known: keeps all pairs
    // in one gesture so WindowServer / Chromium coalesce them correctly.
    let click_group_id: Option<i64> = wid.map(|_| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos() as i64
    });

    super::keyboard::with_pid_modifier_keys(pid, modifiers, || {
        let mut events = events.into_iter();
        if let Some(primer) = events.next() {
            // Prime the target window's cursor-tracking state with a leading
            // mouseMoved so an AppKit NSButton / NSView hit-tests the down at
            // the right point. The primer is the target point, never off-screen.
            post_mouse_moved_primer(
                pid,
                &source,
                CGPoint::new(primer.screen.0, primer.screen.1),
                primer.window_local,
                wid,
                click_group_id,
                post_mode,
            );
            std::thread::sleep(std::time::Duration::from_millis(12));
        }

        while let Some(down_step) = events.next() {
            let Some(up_step) = events.next() else {
                anyhow::bail!("left-click plan ended on a down without an up");
            };
            super::pointer::guard_event_point(down_step.screen.0, down_step.screen.1)?;
            let point = CGPoint::new(down_step.screen.0, down_step.screen.1);

            let up = CGEvent::new_mouse_event(
                source.clone(),
                CGEventType::LeftMouseUp,
                point,
                CGMouseButton::Left,
            )
            .map_err(|_| anyhow::anyhow!("CGEvent::new_mouse_event(up) failed"))?;
            let down = CGEvent::new_mouse_event(
                source.clone(),
                CGEventType::LeftMouseDown,
                point,
                CGMouseButton::Left,
            )
            .map_err(|_| anyhow::anyhow!("CGEvent::new_mouse_event(down) failed"))?;
            if flags != CGEventFlags::CGEventFlagNull {
                down.set_flags(flags);
            }

            post_mouse_event_with_mode(
                pid,
                &down,
                down_step.window_local,
                wid,
                click_group_id,
                down_step.click_state,
                0,
                3,
                post_mode,
            );
            // 28 ms down→up gap: an NSButton's mouseDown enters a modal
            // tracking loop that polls for the matching mouseUp; too tight a
            // gap can race the loop's first poll and the click is dropped.
            std::thread::sleep(std::time::Duration::from_millis(28));

            if flags != CGEventFlags::CGEventFlagNull {
                up.set_flags(flags);
            }

            if let Err(error) =
                super::pointer::guard_event_point(up_step.screen.0, up_step.screen.1)
            {
                up.post_to_pid(pid);
                return Err(error);
            }
            post_mouse_event_with_mode(
                pid,
                &up,
                up_step.window_local,
                wid,
                click_group_id,
                up_step.click_state,
                0,
                3,
                post_mode,
            );

            if count > 1 {
                std::thread::sleep(std::time::Duration::from_millis(80));
            }
        }
        Ok(())
    })
}

/// Press-drag-release gesture from `(from_x, from_y)` to `(to_x, to_y)` in
/// screen coordinates, addressed to `pid` and the optional exact window.
///
/// `duration_ms` is the wall-clock budget for the drag path; `steps` is the
/// number of intermediate `leftMouseDragged` events linearly interpolated
/// along the path. `modifiers` are held across the entire gesture.
///
/// Like the Swift reference `MouseInput.drag`, uses the SkyLight path for
/// backgrounded-target delivery.
// The drag primitive deliberately exposes its complete native event contract.
#[allow(clippy::too_many_arguments)]
pub fn drag_at_xy(
    pid: i32,
    from_x: f64,
    from_y: f64,
    to_x: f64,
    to_y: f64,
    from_local: Option<(f64, f64)>,
    to_local: Option<(f64, f64)>,
    wid: Option<u32>,
    duration_ms: u64,
    steps: usize,
    modifiers: &[&str],
    button: DragButton,
    foreground_hid: bool,
) -> anyhow::Result<()> {
    drag_at_xy_observed(
        pid,
        from_x,
        from_y,
        to_x,
        to_y,
        from_local,
        to_local,
        wid,
        duration_ms,
        steps,
        modifiers,
        button,
        foreground_hid,
        |_, _| {},
    )
}

/// Window drag with an observer called for every native pointer position.
///
/// Background delivery keeps the established PID-routed sequence. When
/// `foreground_hid` is true, the same stamped gesture is posted once per event
/// through the global HID queue while the hardware cursor follows the path.
///
/// The cursor overlay uses this for the same reason as
/// [`drag_at_xy_foreground_observed`]: the synthetic cursor should follow the
/// actual dispatched path rather than jumping to the endpoint after release.
#[allow(clippy::too_many_arguments)]
pub fn drag_at_xy_observed<F>(
    pid: i32,
    from_x: f64,
    from_y: f64,
    to_x: f64,
    to_y: f64,
    from_local: Option<(f64, f64)>,
    to_local: Option<(f64, f64)>,
    wid: Option<u32>,
    duration_ms: u64,
    steps: usize,
    modifiers: &[&str],
    button: DragButton,
    foreground_hid: bool,
    mut observe: F,
) -> anyhow::Result<()>
where
    F: FnMut(f64, f64),
{
    use std::time::{SystemTime, UNIX_EPOCH};

    if foreground_hid {
        return drag_at_xy_window_foreground_observed(
            pid,
            from_x,
            from_y,
            to_x,
            to_y,
            from_local,
            to_local,
            wid,
            duration_ms,
            steps,
            modifiers,
            button,
            observe,
        );
    }

    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|_| anyhow::anyhow!("CGEventSource::new failed"))?;
    let flags = parse_modifier_flags(modifiers);

    let (cg_button, down_type, dragged_type, up_type) = match button {
        DragButton::Left => (
            CGMouseButton::Left,
            CGEventType::LeftMouseDown,
            CGEventType::LeftMouseDragged,
            CGEventType::LeftMouseUp,
        ),
        DragButton::Right => (
            CGMouseButton::Right,
            CGEventType::RightMouseDown,
            CGEventType::RightMouseDragged,
            CGEventType::RightMouseUp,
        ),
        DragButton::Middle => (
            CGMouseButton::Center,
            CGEventType::OtherMouseDown,
            CGEventType::OtherMouseDragged,
            CGEventType::OtherMouseUp,
        ),
    };
    // f3 button number must match the dragged button (0=left, 1=right, 2=middle).
    let button_number: i64 = match button {
        DragButton::Left => 0,
        DragButton::Right => 1,
        DragButton::Middle => 2,
    };

    let click_group_id: Option<i64> = wid.map(|_| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos() as i64
    });

    let steps = steps.max(1);
    let step_delay_ms = if steps > 1 {
        duration_ms / steps as u64
    } else {
        duration_ms
    };

    // Prime the routed target's cursor-tracking state immediately before the
    // press, matching the click path's stamped mouseMoved + settle interval.
    let from_pt = CGPoint::new(from_x, from_y);
    post_mouse_moved_primer(
        pid,
        &source,
        from_pt,
        from_local,
        wid,
        click_group_id,
        MousePostMode::Both,
    );
    std::thread::sleep(std::time::Duration::from_millis(12));

    // MouseDown at start.
    let down = CGEvent::new_mouse_event(source.clone(), down_type, from_pt, cg_button)
        .map_err(|_| anyhow::anyhow!("drag mouseDown failed"))?;
    if flags != CGEventFlags::CGEventFlagNull {
        down.set_flags(flags);
    }
    post_mouse_event(
        pid,
        &down,
        from_local,
        wid,
        click_group_id,
        1,
        button_number,
        0,
    );
    observe(from_x, from_y);
    std::thread::sleep(std::time::Duration::from_millis(16));

    // Interpolated drag steps.
    for i in 1..=steps {
        let t = i as f64 / steps as f64;
        let ix = from_x + (to_x - from_x) * t;
        let iy = from_y + (to_y - from_y) * t;
        let il = from_local
            .zip(to_local)
            .map(|((fx, fy), (tx, ty))| (fx + (tx - fx) * t, fy + (ty - fy) * t));
        let drag_pt = CGPoint::new(ix, iy);
        let drag = CGEvent::new_mouse_event(source.clone(), dragged_type, drag_pt, cg_button)
            .map_err(|_| anyhow::anyhow!("drag mouseDragged failed"))?;
        if flags != CGEventFlags::CGEventFlagNull {
            drag.set_flags(flags);
        }
        post_mouse_event(pid, &drag, il, wid, click_group_id, 1, button_number, 0);
        observe(ix, iy);
        if step_delay_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(step_delay_ms));
        }
    }

    // MouseUp at end.
    // Give Chromium one run-loop turn to process the final dragged event at
    // the drop point before releasing pointer capture.
    std::thread::sleep(std::time::Duration::from_millis(50));
    let to_pt = CGPoint::new(to_x, to_y);
    let up = CGEvent::new_mouse_event(source.clone(), up_type, to_pt, cg_button)
        .map_err(|_| anyhow::anyhow!("drag mouseUp failed"))?;
    if flags != CGEventFlags::CGEventFlagNull {
        up.set_flags(flags);
    }
    post_mouse_event(pid, &up, to_local, wid, click_group_id, 1, button_number, 0);
    // Chromium may process the final pointerup on the next run-loop turn. In
    // the foreground rung the caller restores the previous app immediately
    // after this function returns, so let the target consume the release and
    // complete pointer capture before that restore.
    std::thread::sleep(std::time::Duration::from_millis(100));

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn post_drag_mouse_event(
    pid: i32,
    event_type: CGEventType,
    point: CGPoint,
    button: CGMouseButton,
    window_local: Option<(f64, f64)>,
    wid: Option<u32>,
    click_group_id: Option<i64>,
    click_state: i64,
    button_number: i64,
    subtype: i64,
    mode: MousePostMode,
    flags: CGEventFlags,
    error_context: &str,
) -> anyhow::Result<()> {
    debug_assert_eq!(mode, MousePostMode::HidOnly);
    super::pointer::guard_event_point(point.x, point.y)?;
    let event = new_global_mouse_event(event_type, point, button)
        .map_err(|_| anyhow::anyhow!(error_context.to_owned()))?;
    if flags != CGEventFlags::CGEventFlagNull {
        event.set_flags(flags);
    }
    post_mouse_event_with_mode(
        pid,
        &event,
        window_local,
        wid,
        click_group_id,
        click_state,
        button_number,
        subtype,
        mode,
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn drag_at_xy_window_foreground_observed<F>(
    pid: i32,
    from_x: f64,
    from_y: f64,
    to_x: f64,
    to_y: f64,
    from_local: Option<(f64, f64)>,
    to_local: Option<(f64, f64)>,
    wid: Option<u32>,
    duration_ms: u64,
    steps: usize,
    modifiers: &[&str],
    button: DragButton,
    mut observe: F,
) -> anyhow::Result<()>
where
    F: FnMut(f64, f64),
{
    use core_graphics::display::CGDisplay;
    use std::time::{SystemTime, UNIX_EPOCH};

    let (cg_button, down_type, dragged_type, up_type, button_number) = match button {
        DragButton::Left => (
            CGMouseButton::Left,
            CGEventType::LeftMouseDown,
            CGEventType::LeftMouseDragged,
            CGEventType::LeftMouseUp,
            0,
        ),
        DragButton::Right => (
            CGMouseButton::Right,
            CGEventType::RightMouseDown,
            CGEventType::RightMouseDragged,
            CGEventType::RightMouseUp,
            1,
        ),
        DragButton::Middle => (
            CGMouseButton::Center,
            CGEventType::OtherMouseDown,
            CGEventType::OtherMouseDragged,
            CGEventType::OtherMouseUp,
            2,
        ),
    };
    let click_group_id = wid.map(|_| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos() as i64
    });
    let steps = steps.max(1);
    let step_delay_ms = if steps > 1 {
        duration_ms / steps as u64
    } else {
        duration_ms
    };
    let events = foreground_drag_events(from_x, from_y, to_x, to_y, steps);

    let mut button_down = false;
    let mut last_point = CGPoint::new(from_x, from_y);
    let result = super::keyboard::with_global_modifier_keys(modifiers, |flags| {
        for spec in events {
            super::pointer::guard_event_point(spec.point.x, spec.point.y)?;
            let (event_type, click_state, event_button, event_button_number, subtype) = match spec
                .kind
            {
                ForegroundDragEventKind::Move => {
                    (CGEventType::MouseMoved, 0, CGMouseButton::Left, 0, 3)
                }
                ForegroundDragEventKind::Down => (down_type, 1, cg_button, button_number, 0),
                ForegroundDragEventKind::Dragged => (dragged_type, 1, cg_button, button_number, 0),
                ForegroundDragEventKind::Up => (up_type, 1, cg_button, button_number, 0),
            };
            let local = from_local.zip(to_local).map(|((fx, fy), (tx, ty))| {
                (
                    fx + (tx - fx) * spec.progress,
                    fy + (ty - fy) * spec.progress,
                )
            });

            match spec.kind {
                ForegroundDragEventKind::Move => {
                    let _ = CGDisplay::warp_mouse_cursor_position(spec.point);
                    unsafe { CGAssociateMouseAndMouseCursorPosition(true) };
                    std::thread::sleep(std::time::Duration::from_millis(40));
                }
                ForegroundDragEventKind::Dragged => {
                    let _ = CGDisplay::warp_mouse_cursor_position(spec.point);
                }
                ForegroundDragEventKind::Up => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                ForegroundDragEventKind::Down => {}
            }

            post_drag_mouse_event(
                pid,
                event_type,
                spec.point,
                event_button,
                local,
                wid,
                click_group_id,
                click_state,
                event_button_number,
                subtype,
                MousePostMode::HidOnly,
                flags,
                "foreground drag event creation failed",
            )?;
            last_point = spec.point;
            if spec.kind == ForegroundDragEventKind::Down {
                button_down = true;
            }
            if spec.kind == ForegroundDragEventKind::Up {
                button_down = false;
            }

            match spec.kind {
                ForegroundDragEventKind::Move => {
                    std::thread::sleep(std::time::Duration::from_millis(12));
                }
                ForegroundDragEventKind::Down => {
                    observe(spec.point.x, spec.point.y);
                    std::thread::sleep(std::time::Duration::from_millis(16));
                }
                ForegroundDragEventKind::Dragged => {
                    observe(spec.point.x, spec.point.y);
                    if step_delay_ms > 0 {
                        std::thread::sleep(std::time::Duration::from_millis(step_delay_ms));
                    }
                }
                ForegroundDragEventKind::Up => {}
            }
        }
        Ok(())
    });
    if button_down {
        let cleanup = wid
            .ok_or_else(|| anyhow::anyhow!("drag lost its target window"))
            .and_then(|wid| release_guarded_button(pid, wid, up_type, cg_button, last_point));
        if let Err(cleanup) = cleanup {
            return Err(anyhow::anyhow!(
                "{}; button release cleanup failed: {cleanup}",
                result.unwrap_err()
            ));
        }
    }
    result?;

    std::thread::sleep(std::time::Duration::from_millis(100));
    Ok(())
}

/// Foreground drag through the global HID event tap.
///
/// A frontmost Chromium/WebKit surface expects a real HID-origin gesture for
/// pointer capture and drag tracking. PID-routed `post_to_pid` events are
/// suitable for background delivery, but they can be silently filtered by the
/// renderer even when the target is frontmost.
// The drag primitive deliberately exposes its complete native event contract.
#[allow(clippy::too_many_arguments)]
pub fn drag_at_xy_foreground(
    from_x: f64,
    from_y: f64,
    to_x: f64,
    to_y: f64,
    duration_ms: u64,
    steps: usize,
    modifiers: &[&str],
    button: DragButton,
) -> anyhow::Result<()> {
    drag_at_xy_foreground_observed(
        from_x,
        from_y,
        to_x,
        to_y,
        duration_ms,
        steps,
        modifiers,
        button,
        |_, _| {},
    )
}

/// Foreground drag with an observer called for every native pointer position.
///
/// The cursor overlay uses this to follow the same interpolated path and
/// cadence as the HID gesture instead of gliding only after the real pointer
/// has already completed the drag.
#[allow(clippy::too_many_arguments)]
pub fn drag_at_xy_foreground_observed<F>(
    from_x: f64,
    from_y: f64,
    to_x: f64,
    to_y: f64,
    duration_ms: u64,
    steps: usize,
    modifiers: &[&str],
    button: DragButton,
    mut observe: F,
) -> anyhow::Result<()>
where
    F: FnMut(f64, f64),
{
    use core_graphics::display::CGDisplay;
    use core_graphics::event::CGEventTapLocation;

    let (cg_button, down_type, dragged_type, up_type) = match button {
        DragButton::Left => (
            CGMouseButton::Left,
            CGEventType::LeftMouseDown,
            CGEventType::LeftMouseDragged,
            CGEventType::LeftMouseUp,
        ),
        DragButton::Right => (
            CGMouseButton::Right,
            CGEventType::RightMouseDown,
            CGEventType::RightMouseDragged,
            CGEventType::RightMouseUp,
        ),
        DragButton::Middle => (
            CGMouseButton::Center,
            CGEventType::OtherMouseDown,
            CGEventType::OtherMouseDragged,
            CGEventType::OtherMouseUp,
        ),
    };
    let events = foreground_drag_events(from_x, from_y, to_x, to_y, steps);
    let steps = steps.max(1);
    let step_delay_ms = if steps > 1 {
        duration_ms / steps as u64
    } else {
        duration_ms
    };

    // Keep WindowServer's hardware cursor and event stream coupled. AppKit
    // hit-tests some pointer-capture surfaces against the actual cursor even
    // when the HID event carries an explicit location.
    let _ = CGDisplay::warp_mouse_cursor_position(CGPoint::new(from_x, from_y));
    unsafe { CGAssociateMouseAndMouseCursorPosition(true) };
    observe(from_x, from_y);
    std::thread::sleep(std::time::Duration::from_millis(40));

    super::keyboard::with_global_modifier_keys(modifiers, |flags| {
        for spec in events {
            let event_type = match spec.kind {
                ForegroundDragEventKind::Move => CGEventType::MouseMoved,
                ForegroundDragEventKind::Down => down_type,
                ForegroundDragEventKind::Dragged => dragged_type,
                ForegroundDragEventKind::Up => up_type,
            };
            // A null source matches macOS's established global mouse-controller
            // path. WindowServer then carries the pressed-button state from the
            // down through every dragged event instead of treating each event as
            // an independent HIDSystemState snapshot.
            let event =
                new_global_mouse_event(event_type, spec.point, cg_button).map_err(|_| {
                    anyhow::anyhow!("foreground drag {:?} event creation failed", spec.kind)
                })?;
            event.set_flags(flags);
            event.post(CGEventTapLocation::HID);

            match spec.kind {
                ForegroundDragEventKind::Move => {
                    std::thread::sleep(std::time::Duration::from_millis(30));
                }
                ForegroundDragEventKind::Down => {
                    std::thread::sleep(std::time::Duration::from_millis(16));
                }
                ForegroundDragEventKind::Dragged => {
                    observe(spec.point.x, spec.point.y);
                    if step_delay_ms > 0 {
                        std::thread::sleep(std::time::Duration::from_millis(step_delay_ms));
                    }
                }
                ForegroundDragEventKind::Up => {}
            }
        }
        Ok(())
    })?;
    // The foreground wrapper restores the previous app immediately after this
    // function returns. Let the target's run loop consume the queued HID
    // gesture, including pointer-capture release, before that restore happens.
    std::thread::sleep(std::time::Duration::from_millis(100));
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ForegroundDragEventKind {
    Move,
    Down,
    Dragged,
    Up,
}

#[derive(Clone, Copy, Debug)]
struct ForegroundDragEvent {
    kind: ForegroundDragEventKind,
    point: CGPoint,
    progress: f64,
}

fn foreground_drag_events(
    from_x: f64,
    from_y: f64,
    to_x: f64,
    to_y: f64,
    steps: usize,
) -> Vec<ForegroundDragEvent> {
    let steps = steps.max(1);
    let mut events = Vec::with_capacity(steps + 3);
    let from = CGPoint::new(from_x, from_y);
    events.push(ForegroundDragEvent {
        kind: ForegroundDragEventKind::Move,
        point: from,
        progress: 0.0,
    });
    events.push(ForegroundDragEvent {
        kind: ForegroundDragEventKind::Down,
        point: from,
        progress: 0.0,
    });
    for i in 1..=steps {
        let t = i as f64 / steps as f64;
        events.push(ForegroundDragEvent {
            kind: ForegroundDragEventKind::Dragged,
            point: CGPoint::new(from_x + (to_x - from_x) * t, from_y + (to_y - from_y) * t),
            progress: t,
        });
    }
    events.push(ForegroundDragEvent {
        kind: ForegroundDragEventKind::Up,
        point: CGPoint::new(to_x, to_y),
        progress: 1.0,
    });
    events
}

fn new_global_mouse_event(
    event_type: CGEventType,
    point: CGPoint,
    button: CGMouseButton,
) -> Result<CGEvent, ()> {
    let event_ref =
        unsafe { CGEventCreateMouseEvent(std::ptr::null_mut(), event_type, point, button) };
    if event_ref.is_null() {
        Err(())
    } else {
        Ok(unsafe { CGEvent::from_ptr(event_ref) })
    }
}

/// Mouse button for drag gestures.
#[derive(Clone, Copy, Debug)]
pub enum DragButton {
    Left,
    Right,
    Middle,
}

/// Middle-click at `(x, y)` with optional modifier keys.
///
/// Posts an `OtherMouseDown` / `OtherMouseUp` pair with `CGMouseButton::Center`
/// to the target pid through the same SkyLight + public-API postBoth path the
/// left- and right-click primitives use. Window-local stamping mirrors
/// `right_click_at_xy_with_window_local`.
pub fn middle_click_at_xy(pid: i32, x: f64, y: f64, modifiers: &[&str]) -> anyhow::Result<()> {
    middle_click_at_xy_inner(pid, x, y, None, modifiers)
}

/// Like `middle_click_at_xy` but stamps the window-local `(wx, wy)` point.
pub fn middle_click_at_xy_with_window_local(
    pid: i32,
    x: f64,
    y: f64,
    wx: f64,
    wy: f64,
    modifiers: &[&str],
) -> anyhow::Result<()> {
    middle_click_at_xy_inner(pid, x, y, Some((wx, wy)), modifiers)
}

fn middle_click_at_xy_inner(
    pid: i32,
    x: f64,
    y: f64,
    window_local: Option<(f64, f64)>,
    modifiers: &[&str],
) -> anyhow::Result<()> {
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|_| anyhow::anyhow!("CGEventSource::new failed"))?;
    let point = CGPoint::new(x, y);
    let flags = parse_modifier_flags(modifiers);

    let down = CGEvent::new_mouse_event(
        source.clone(),
        CGEventType::OtherMouseDown,
        point,
        CGMouseButton::Center,
    )
    .map_err(|_| anyhow::anyhow!("middle mouse down failed"))?;
    if flags != CGEventFlags::CGEventFlagNull {
        down.set_flags(flags);
    }
    post_mouse_event(pid, &down, window_local, None, None, 1, 2, 3);
    std::thread::sleep(std::time::Duration::from_millis(16));

    let up = CGEvent::new_mouse_event(
        source,
        CGEventType::OtherMouseUp,
        point,
        CGMouseButton::Center,
    )
    .map_err(|_| anyhow::anyhow!("middle mouse up failed"))?;
    if flags != CGEventFlags::CGEventFlagNull {
        up.set_flags(flags);
    }
    post_mouse_event(pid, &up, window_local, None, None, 1, 2, 3);

    Ok(())
}

/// Right-click at `(x, y)` with optional modifier keys (no window routing).
pub fn right_click_at_xy(pid: i32, x: f64, y: f64, modifiers: &[&str]) -> anyhow::Result<()> {
    right_click_at_xy_inner(pid, x, y, None, None, modifiers)
}

/// Like `right_click_at_xy` but stamps `CGEventSetWindowLocation` with the
/// window-local `(wx, wy)` point AND the window-routing fields (f51/f91/f92) so
/// the `rightMouseDown` reaches a backgrounded `NSView`.
///
/// `wid` is required for the routing fields: without a window number stamped,
/// WindowServer falls back to a screen-location hit-test that skips non-key
/// (backgrounded) windows, so the right-down never reached the NSView — the
/// reported "right-click does not fire rightMouseDown" bug. The left-click path
/// already threaded `wid`; right-click did not, which is why it broke.
pub fn right_click_at_xy_with_window_local(
    pid: i32,
    x: f64,
    y: f64,
    wx: f64,
    wy: f64,
    wid: u32,
    modifiers: &[&str],
) -> anyhow::Result<()> {
    right_click_at_xy_inner(pid, x, y, Some((wx, wy)), Some(wid), modifiers)
}

fn right_click_at_xy_inner(
    pid: i32,
    x: f64,
    y: f64,
    window_local: Option<(f64, f64)>,
    wid: Option<u32>,
    modifiers: &[&str],
) -> anyhow::Result<()> {
    use std::time::{SystemTime, UNIX_EPOCH};

    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|_| anyhow::anyhow!("CGEventSource::new failed"))?;
    let point = CGPoint::new(x, y);
    let flags = parse_modifier_flags(modifiers);

    let click_group_id: Option<i64> = wid.map(|_| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos() as i64
    });

    // Prime cursor-tracking state at the target so AppKit hit-tests the
    // right-down at the right NSView (same rationale as the left path).
    post_mouse_moved_primer(
        pid,
        &source,
        point,
        window_local,
        wid,
        click_group_id,
        MousePostMode::Both,
    );
    std::thread::sleep(std::time::Duration::from_millis(12));

    let down = CGEvent::new_mouse_event(
        source.clone(),
        CGEventType::RightMouseDown,
        point,
        CGMouseButton::Right,
    )
    .map_err(|_| anyhow::anyhow!("right mouse down failed"))?;
    if flags != CGEventFlags::CGEventFlagNull {
        down.set_flags(flags);
    }
    // button_number = 1 (right). Stamping 0 here routes the event as a left
    // button-number on the receiving side even though the type is rightMouseDown.
    post_mouse_event(pid, &down, window_local, wid, click_group_id, 1, 1, 3);
    std::thread::sleep(std::time::Duration::from_millis(28));

    let up = CGEvent::new_mouse_event(
        source,
        CGEventType::RightMouseUp,
        point,
        CGMouseButton::Right,
    )
    .map_err(|_| anyhow::anyhow!("right mouse up failed"))?;
    if flags != CGEventFlags::CGEventFlagNull {
        up.set_flags(flags);
    }
    post_mouse_event(pid, &up, window_local, wid, click_group_id, 1, 1, 3);

    Ok(())
}

/// Post a mouse event to `pid`.
///
/// Matches Swift's `MouseInput.postBoth(_:toPid:)`:
/// - Fires `SLEventPostToPid` (SkyLight path — reaches backgrounded Chromium/Catalyst).
/// - Also fires `CGEvent::post_to_pid` (public path — lands on AppKit targets where
///   SkyLight mouse delivery drops).
///
/// Both are always posted in sequence regardless of whether the other succeeded.
///
/// Field stamps applied (always):
/// - f40 = `pid`  (Chromium's synthetic-event filter)
/// - `CGEventSetWindowLocation` = window-local point (if provided)
///
/// Additional stamps when `wid` is provided (Chromium window-routing fields):
/// - f1  = `click_state`    (kCGMouseEventClickState)
/// - f3  = `button_number`  (kCGMouseEventButtonNumber: 0=left, 1=right, 2=middle)
/// - f7  = 3                (kCGMouseEventSubtype = NSEventSubtypeTouch)
/// - f51 = window_id        (windowNumber, NSEvent bridge equivalent)
/// - f58 = click_group_id   (gesture coalescing across pairs)
/// - f91 = window_id        (kCGMouseEventWindowUnderMousePointer)
/// - f92 = window_id        (kCGMouseEventWindowUnderMousePointerThatCanHandleThisEvent)
///
/// `button_number` MUST match the button encoded in the event type (right-down
/// stamped with f3=0 routes as a left-click on the receiving side — this was the
/// right-click-lands-as-nothing bug). Left=0, Right=1, Middle=2.
// These arguments are the individual CGEvent fields stamped by this low-level primitive.
#[allow(clippy::too_many_arguments)]
pub(super) fn post_mouse_event(
    pid: i32,
    event: &CGEvent,
    window_local: Option<(f64, f64)>,
    wid: Option<u32>,
    click_group_id: Option<i64>,
    click_state: i64,
    button_number: i64,
    subtype: i64,
) {
    post_mouse_event_with_mode(
        pid,
        event,
        window_local,
        wid,
        click_group_id,
        click_state,
        button_number,
        subtype,
        MousePostMode::Both,
    );
}

#[allow(clippy::too_many_arguments)]
fn post_mouse_event_with_mode(
    pid: i32,
    event: &CGEvent,
    window_local: Option<(f64, f64)>,
    wid: Option<u32>,
    click_group_id: Option<i64>,
    click_state: i64,
    button_number: i64,
    subtype: i64,
    mode: MousePostMode,
) {
    let event_ptr = event.as_ptr() as *mut std::ffi::c_void;

    // Stamp window-local point for backgrounded window targeting.
    if let Some((wx, wy)) = window_local {
        crate::input::skylight::set_window_location(event_ptr, wx, wy);
    }

    // Chromium / AppKit window-routing fields — stamp when window_id is known.
    if let (Some(wid), Some(cgid)) = (wid, click_group_id) {
        let window_id = wid as i64;
        let set = |f: u32, v: i64| {
            crate::input::skylight::set_integer_field(event_ptr, f, v);
        };
        set(1, click_state); // kCGMouseEventClickState
        set(3, button_number); // kCGMouseEventButtonNumber (0=left, 1=right, 2=middle)
        set(7, subtype); // kCGMouseEventSubtype (touch for clicks, normal for drags)
        set(51, window_id); // windowNumber
        set(58, cgid); // click-group ID (gesture coalescing)
        set(91, window_id); // kCGMouseEventWindowUnderMousePointer
        set(92, window_id); // kCGMouseEventWindowUnderMousePointerThatCanHandleThisEvent
    }

    // Always stamp f40 = target pid (Chromium synthetic-event filter).
    crate::input::skylight::set_integer_field(event_ptr, 40, pid as i64);

    match mode {
        MousePostMode::Both => {
            // Preserve the established transport for non-left-click callers.
            crate::input::skylight::post_to_pid(pid as libc::pid_t, event_ptr, false);
            event.post_to_pid(pid as libc::pid_t);
        }
        MousePostMode::PublicOnly => event.post_to_pid(pid as libc::pid_t),
        MousePostMode::HidOnly => event.post(core_graphics::event::CGEventTapLocation::HID),
    }
}

/// Post a stamped `mouseMoved` to `pid` at `point` before a down/up pair.
///
/// AppKit hit-tests a button/`NSView` against the cursor-tracking state the
/// window last saw; a backgrounded window that never received a move event has
/// stale tracking state, so the synthetic `mouseDown` lands "outside" the
/// control and `-mouseDown:` never fires (the synthetic-NSButton-ignored bug).
/// A leading `mouseMoved` at the target primes that state — this is exactly the
/// Step-3 `mouseMoved` of the Swift `clickViaAuthSignedPost` recipe, which the
/// default Rust pixel path had dropped. Click-state 0 / no button (move events
/// carry no button); window-routing fields still stamped so the move reaches the
/// right backgrounded window.
fn post_mouse_moved_primer(
    pid: i32,
    source: &CGEventSource,
    point: CGPoint,
    window_local: Option<(f64, f64)>,
    wid: Option<u32>,
    click_group_id: Option<i64>,
    mode: MousePostMode,
) {
    if let Ok(mv) = CGEvent::new_mouse_event(
        source.clone(),
        CGEventType::MouseMoved,
        point,
        CGMouseButton::Left,
    ) {
        post_mouse_event_with_mode(pid, &mv, window_local, wid, click_group_id, 0, 0, 3, mode);
    }
}

/// Synthesize a targeted mouse-wheel scroll at `(screen_x, screen_y)`,
/// posted to `pid`.
///
/// This is the targeted wheel path. Unlike the keystroke path (PageDown /
/// arrow keys), which only ever drives the *focused* / page scroller, a real
/// wheel event is hit-tested by the renderer at the cursor point: whatever
/// element sits under `(screen_x, screen_y)` receives the scroll. That is the
/// only way to scroll a nested `overflow:auto` div that has no `tabindex` and
/// therefore can never take keyboard focus (verified no-op via keystrokes on
/// WKWebView's inner `scroll-tall` and WebView2).
///
/// `CGEventCreateScrollWheelEvent2` builds a line-unit event; we then anchor it
/// at the target point with `CGEventSetLocation` so the renderer routes it
/// correctly, and stamp the same background-delivery fields the click
/// primitives use (window-local point + f40 pid filter + window-routing fields)
/// so it reaches backgrounded Chromium/Catalyst/WKWebView targets.
///
/// Sign convention (macOS): a POSITIVE `delta_y_per_tick` scrolls the content
/// toward the top (reveals content ABOVE); NEGATIVE reveals content BELOW.
/// POSITIVE `delta_x_per_tick` reveals content to the LEFT; NEGATIVE to the
/// RIGHT. The direction→delta mapping lives in the `scroll` tool; this primitive
/// stays sign-agnostic (if a live target scrolls inverted, flip there).
///
/// `ticks` discrete wheel events are posted (one per notch), mirroring the
/// keystroke path's `amount` repetitions, each separated by a short gap so the
/// renderer animates per-notch instead of coalescing into a single jump.
///
/// `window_local`/`wid`: when known, stamp the window-local point and the
/// Chromium window-routing fields (f51/f91/f92) for backgrounded delivery —
/// identical in spirit to `post_mouse_event`.
// These arguments are the individual wheel-event fields used by existing callers.
#[allow(clippy::too_many_arguments)]
pub fn scroll_wheel_at_xy(
    pid: i32,
    screen_x: f64,
    screen_y: f64,
    window_local: Option<(f64, f64)>,
    wid: Option<u32>,
    delta_y_per_tick: i32,
    delta_x_per_tick: i32,
    ticks: usize,
) -> anyhow::Result<()> {
    use core_graphics::event::ScrollEventUnit;
    super::pointer::guard_event_point(screen_x, screen_y)?;

    // Prime AppKit/WebKit's tracking state at the target before the wheel
    // gesture. Background windows can retain a stale hit-test location; a
    // nested overflow scroller then receives the wheel nowhere even though
    // the event is successfully posted to the target pid.
    let primer_source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|_| anyhow::anyhow!("CGEventSource::new failed"))?;
    post_mouse_moved_primer(
        pid,
        &primer_source,
        CGPoint::new(screen_x, screen_y),
        window_local,
        wid,
        Some(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos() as i64,
        ),
        MousePostMode::Both,
    );
    std::thread::sleep(std::time::Duration::from_millis(12));

    for _ in 0..ticks.max(1) {
        super::pointer::guard_event_point(screen_x, screen_y)?;
        // Fresh source per event, matching the click primitives.
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|_| anyhow::anyhow!("CGEventSource::new failed"))?;
        // wheel_count = 2 → both axes carried (vertical = wheel1/axis-1,
        // horizontal = wheel2/axis-2). Convert the driver's pixel-tuned step
        // into a bounded line delta for the event API.
        let wheel_y = (delta_y_per_tick / 120).clamp(-10, 10);
        let wheel_x = (delta_x_per_tick / 120).clamp(-10, 10);
        let event =
            CGEvent::new_scroll_event(source, ScrollEventUnit::LINE, 2, wheel_y, wheel_x, 0)
                .map_err(|_| anyhow::anyhow!("CGEvent::new_scroll_event failed"))?;

        let event_ptr = event.as_ptr() as *mut std::ffi::c_void;

        // Anchor the event at the target screen point so the renderer's wheel
        // hit-test routes the scroll to the element under the cursor.
        unsafe { CGEventSetLocation(event_ptr, screen_x, screen_y) };

        // Background-delivery stamps (mirror post_mouse_event).
        if let Some((wx, wy)) = window_local {
            crate::input::skylight::set_window_location(event_ptr, wx, wy);
        }
        if let Some(wid) = wid {
            let window_id = wid as i64;
            let set = |f: u32, v: i64| {
                crate::input::skylight::set_integer_field(event_ptr, f, v);
            };
            set(51, window_id); // windowNumber
            set(91, window_id); // kCGMouseEventWindowUnderMousePointer
            set(92, window_id); // ...ThatCanHandleThisEvent
        }
        // f40 = target pid (Chromium synthetic-event filter).
        crate::input::skylight::set_integer_field(event_ptr, 40, pid as i64);

        // Belt+suspenders post: SkyLight reaches backgrounded Chromium/Catalyst;
        // the public path lands on AppKit/WKWebView. Mouse-class → no auth envelope.
        crate::input::skylight::post_to_pid(pid as libc::pid_t, event_ptr, false);
        event.post_to_pid(pid as libc::pid_t);

        std::thread::sleep(std::time::Duration::from_millis(30));
    }
    Ok(())
}

extern "C" {
    /// Quartz mouse constructor with a null source, matching established
    /// foreground controller libraries and the global scroll path above.
    fn CGEventCreateMouseEvent(
        source: core_graphics::sys::CGEventSourceRef,
        event_type: core_graphics::event::CGEventType,
        point: CGPoint,
        button: core_graphics::event::CGMouseButton,
    ) -> core_graphics::sys::CGEventRef;

    /// `void CGEventSetLocation(CGEventRef event, CGPoint location)`.
    ///
    /// `CGPoint { double x, double y }` is classified as two FP eightbytes on
    /// arm64 / x86-64, so passing the two doubles as separate args is
    /// ABI-identical to passing the struct by value — the same trick the
    /// SkyLight bridge uses for `CGEventSetWindowLocation`.
    fn CGEventSetLocation(event: *mut std::ffi::c_void, x: f64, y: f64);
}

fn parse_modifier_flags(modifiers: &[&str]) -> CGEventFlags {
    let mut flags = CGEventFlags::CGEventFlagNull;
    for m in modifiers {
        match m.to_lowercase().as_str() {
            "cmd" | "command" | "super" | "meta" | "win" => {
                flags |= CGEventFlags::CGEventFlagCommand
            }
            "shift" => flags |= CGEventFlags::CGEventFlagShift,
            "option" | "alt" => flags |= CGEventFlags::CGEventFlagAlternate,
            "ctrl" | "control" => flags |= CGEventFlags::CGEventFlagControl,
            _ => {}
        }
    }
    flags
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_left_window_dispatches_public_exact_local_pairs() {
        let frame = crate::tools::px_frame::WindowPxFrame {
            bounds: crate::windows::WindowBounds {
                x: 170.0,
                y: 194.0,
                width: 760.0,
                height: 460.0,
            },
            scale: 2.0,
        };
        let capture_local = (394.0, 310.0);
        let admitted_native = (capture_local.0 * frame.scale, capture_local.1 * frame.scale);
        let (screen_x, screen_y, local_x, local_y) =
            frame.to_screen(admitted_native.0, admitted_native.1);
        assert_eq!((screen_x, screen_y), (564.0, 504.0));
        assert_eq!((local_x, local_y), capture_local);

        let selected = select_window_left_click(
            WindowClickDelivery::Background,
            (screen_x, screen_y),
            (local_x, local_y),
            1,
        );
        let WindowLeftClickSelection::BackgroundPublic(single) = selected else {
            panic!("background left must take the public pid route");
        };
        assert_eq!(single.post_mode, MousePostMode::PublicOnly);
        assert!(!single.activates_target);
        assert_eq!(single.local, capture_local);
        assert_eq!(single.screen, (screen_x, screen_y));
        assert_eq!(
            single.events,
            public_left_click_events((screen_x, screen_y), Some(capture_local), 1)
        );
        let kinds: Vec<_> = single.events.iter().map(|event| event.kind).collect();
        assert_eq!(
            kinds,
            vec![
                LeftClickEventKind::Move,
                LeftClickEventKind::Down,
                LeftClickEventKind::Up,
            ]
        );
        for event in &single.events {
            assert_eq!(event.screen, (screen_x, screen_y));
            assert_eq!(event.window_local, Some(capture_local));
            assert_ne!(event.screen, (-1.0, -1.0));
            assert_ne!(event.window_local, Some((-1.0, -1.0)));
        }
        assert_eq!(single.events[0].click_state, 0);
        assert_eq!(single.events[1].click_state, 1);
        assert_eq!(single.events[2].click_state, 1);

        let doubled = select_window_left_click(
            WindowClickDelivery::Background,
            (screen_x, screen_y),
            (local_x, local_y),
            2,
        );
        let WindowLeftClickSelection::BackgroundPublic(doubled) = doubled else {
            panic!("background double-click must take the public pid route");
        };
        assert_eq!(doubled.post_mode, MousePostMode::PublicOnly);
        assert!(!doubled.activates_target);
        let double_kinds: Vec<_> = doubled.events.iter().map(|event| event.kind).collect();
        assert_eq!(
            double_kinds,
            vec![
                LeftClickEventKind::Move,
                LeftClickEventKind::Down,
                LeftClickEventKind::Up,
                LeftClickEventKind::Down,
                LeftClickEventKind::Up,
            ]
        );
        assert_eq!(doubled.events[1].click_state, 1);
        assert_eq!(doubled.events[2].click_state, 1);
        assert_eq!(doubled.events[3].click_state, 2);
        assert_eq!(doubled.events[4].click_state, 2);
        assert!(doubled
            .events
            .iter()
            .all(|event| event.window_local == Some(capture_local)));

        // Gate geometry: capture (85, 209) on a window at (80, 678) is local
        // (85, 209), not the screen point (165, 887).
        let canvas = select_window_left_click(
            WindowClickDelivery::Background,
            (165.0, 887.0),
            (85.0, 209.0),
            1,
        );
        let WindowLeftClickSelection::BackgroundPublic(canvas) = canvas else {
            panic!("canvas background click must take the public pid route");
        };
        assert_eq!(canvas.post_mode, MousePostMode::PublicOnly);
        assert!(!canvas.activates_target);
        assert!(canvas
            .events
            .iter()
            .all(|event| event.window_local == Some((85.0, 209.0))));
        assert!(canvas
            .events
            .iter()
            .all(|event| event.screen == (165.0, 887.0)));

        let foreground = select_window_left_click(
            WindowClickDelivery::Foreground,
            (165.0, 887.0),
            (85.0, 209.0),
            1,
        );
        match foreground {
            WindowLeftClickSelection::ForegroundPublic {
                screen,
                local,
                count,
                post_mode,
            } => {
                assert_eq!(screen, (165.0, 887.0));
                assert_eq!(local, (85.0, 209.0));
                assert_eq!(count, 1);
                assert_eq!(post_mode, MousePostMode::PublicOnly);
            }
            WindowLeftClickSelection::BackgroundPublic(_) => {
                panic!("foreground arm must stay on its existing public route");
            }
        }
    }

    #[test]
    fn foreground_drag_plan_is_one_coherent_gesture() {
        let events = foreground_drag_events(10.0, 20.0, 40.0, 80.0, 3);
        let kinds: Vec<_> = events.iter().map(|event| event.kind).collect();
        assert_eq!(
            kinds,
            [
                ForegroundDragEventKind::Move,
                ForegroundDragEventKind::Down,
                ForegroundDragEventKind::Dragged,
                ForegroundDragEventKind::Dragged,
                ForegroundDragEventKind::Dragged,
                ForegroundDragEventKind::Up,
            ]
        );
        let points: Vec<_> = events
            .iter()
            .map(|event| (event.point.x, event.point.y))
            .collect();
        assert_eq!(
            points,
            [
                (10.0, 20.0),
                (10.0, 20.0),
                (20.0, 40.0),
                (30.0, 60.0),
                (40.0, 80.0),
                (40.0, 80.0),
            ]
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.kind == ForegroundDragEventKind::Up)
                .count(),
            1,
            "the HID gesture must release exactly once"
        );
        let cursor_plan: Vec<_> = events
            .iter()
            .filter(|event| {
                matches!(
                    event.kind,
                    ForegroundDragEventKind::Move | ForegroundDragEventKind::Dragged
                )
            })
            .map(|event| (event.point.x, event.point.y))
            .collect();
        assert_eq!(
            cursor_plan,
            [(10.0, 20.0), (20.0, 40.0), (30.0, 60.0), (40.0, 80.0)]
        );
    }

    #[test]
    fn foreground_drag_plan_keeps_a_drag_sample_for_zero_steps() {
        let events = foreground_drag_events(1.0, 2.0, 3.0, 4.0, 0);
        assert_eq!(events.len(), 4);
        assert_eq!(events[2].kind, ForegroundDragEventKind::Dragged);
        assert_eq!((events[2].point.x, events[2].point.y), (3.0, 4.0));
    }

    #[test]
    fn global_drag_events_use_hardware_like_source_and_button_fields() {
        use core_graphics::event::EventField;

        let point = CGPoint::new(10.0, 20.0);
        let cases = [
            (CGEventType::LeftMouseDown, 1, 1.0),
            (CGEventType::LeftMouseDragged, 1, 1.0),
            (CGEventType::LeftMouseUp, 0, 0.0),
        ];
        for (event_type, click_state, pressure) in cases {
            let event = new_global_mouse_event(event_type, point, CGMouseButton::Left).unwrap();
            assert_eq!(
                event.get_integer_value_field(EventField::EVENT_SOURCE_STATE_ID),
                CGEventSourceStateID::CombinedSessionState as i64
            );
            assert_eq!(
                event.get_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE),
                click_state
            );
            assert_eq!(
                event.get_double_value_field(EventField::MOUSE_EVENT_PRESSURE),
                pressure
            );
        }
    }
}
