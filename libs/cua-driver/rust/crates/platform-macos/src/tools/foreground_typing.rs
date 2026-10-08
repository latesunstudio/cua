//! Guards for foreground `type_text` keystrokes.
//!
//! Foreground typing posts real key events to whatever the target app has
//! focused. When that is not an editable field, every keystroke is rejected and
//! macOS plays the alert sound once per character. This module keeps that
//! bounded without limiting typing that lands: the focused element is checked
//! before the first keystroke, typing stops after a few keystrokes the field
//! visibly doesn't take, long text tries one accessibility write first, a
//! process-wide guard stops repeated rejected bursts, and a cancelled call
//! stops before its next keystroke. Keystrokes a field takes are never capped
//! or rate limited.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use cua_driver_core::protocol::ToolResult;

/// Longer text first tries one accessibility write into the verified field,
/// which is faster than keystrokes; when that verifiably doesn't land, it is
/// typed as guarded keystrokes like any other text.
pub(crate) const LONG_TEXT_CHARS: usize = 64;

/// Keystrokes posted before the first check that the field took them.
pub(crate) const PROBE_KEYSTROKES: usize = 3;

/// Keystrokes between later acceptance checks.
pub(crate) const CHECK_INTERVAL: usize = 4;

/// How long a check waits for the field's value to reflect posted keystrokes.
/// Generous so a slow app isn't mistaken for a rejecting one; nothing is
/// posted while waiting.
const ACCEPT_WAIT: Duration = Duration::from_secs(1);
const ACCEPT_POLL: Duration = Duration::from_millis(10);

/// Rejected keystrokes (each one a beep) allowed across calls per window
/// before foreground typing pauses for the person.
const RATE_WINDOW: Duration = Duration::from_secs(60);
const MAX_REJECTED_PER_WINDOW: usize = 12;

/// Apps that forward keystrokes to another machine (remote desktops and
/// virtual machines). They expose no local text field to check or read back,
/// and the keys never reach a local app that could beep, so they keep the
/// unguarded foreground route.
const INPUT_FORWARDER_BUNDLE_IDS: &[&str] = &[
    "com.apple.ScreenSharing",
    "com.microsoft.rdc.macos",
    "com.vmware.fusion",
    "com.parallels.desktop.console",
    "com.utmapp.UTM",
    "org.virtualbox.app.VirtualBoxVM",
];

pub(crate) fn is_input_forwarder(pid: i32) -> bool {
    crate::apps::bundle_id_for_pid(pid)
        .is_some_and(|bundle| INPUT_FORWARDER_BUNDLE_IDS.contains(&bundle.as_str()))
}

// ── Route ────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ForegroundRoute {
    /// Guarded keystrokes.
    Keystrokes,
    /// Long text into a readable field: one accessibility write, read back,
    /// falling back to keystrokes when it verifiably didn't land.
    AtomicAxFirst,
}

/// An AX write is tried only where its effect can be read back; otherwise a
/// failed write could not be told apart from a landed one.
pub(crate) fn foreground_route(chars: usize, value_readable: bool) -> ForegroundRoute {
    if chars > LONG_TEXT_CHARS && value_readable {
        ForegroundRoute::AtomicAxFirst
    } else {
        ForegroundRoute::Keystrokes
    }
}

// ── Focused field ────────────────────────────────────────────────────────────

/// Facts about the field that would receive foreground keystrokes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FieldFacts {
    /// Whether the app has a focused element at all.
    pub has_focus: bool,
    /// For an element-addressed call: whether the focused element is it.
    /// `None` when no element was requested.
    pub is_requested: Option<bool>,
    pub role: Option<String>,
    pub enabled: Option<bool>,
    pub value_settable: bool,
    pub selected_text_settable: bool,
    /// Password fields: the value is hidden, so it can't show acceptance.
    pub secure: bool,
    /// Terminals take keys without exposing a text field.
    pub key_driven: bool,
}

impl FieldFacts {
    /// Whether the field's value shows what keystrokes did. Terminals may not
    /// echo (password prompts) and secure fields hide their value.
    pub(crate) fn value_readable(&self) -> bool {
        !self.key_driven && !self.secure
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FieldRefusal {
    pub code: &'static str,
    pub reason: String,
}

const TEXT_ROLES: &[&str] = &["AXTextField", "AXTextArea", "AXComboBox", "AXSearchField"];

pub(crate) fn classify_field(facts: &FieldFacts) -> Result<(), FieldRefusal> {
    let role = facts.role.as_deref().unwrap_or("none");
    if !facts.has_focus {
        return Err(FieldRefusal {
            code: "foreground_target_not_focused",
            reason: "the app has no focused element to type into".into(),
        });
    }
    if facts.is_requested == Some(false) {
        return Err(FieldRefusal {
            code: "foreground_target_not_focused",
            reason: format!(
                "the requested element did not take keyboard focus (a {role} is focused instead)"
            ),
        });
    }
    // Text roles pass even when AX calls them unsettable: some editable
    // fields (Catalyst) report that, and a truly read-only one is caught by
    // the acceptance probe after a few keystrokes. Custom editors qualify by
    // a settable text value.
    let editable = facts.key_driven
        || TEXT_ROLES.contains(&role)
        || facts.value_settable
        || facts.selected_text_settable;
    if !editable {
        return Err(FieldRefusal {
            code: "foreground_target_not_editable",
            reason: format!("the focused element is a {role}, not a text field"),
        });
    }
    if facts.enabled == Some(false) {
        return Err(FieldRefusal {
            code: "foreground_target_not_editable",
            reason: format!("the focused {role} is disabled"),
        });
    }
    Ok(())
}

/// Read [`FieldFacts`] for `field`, the element that would take keystrokes.
///
/// # Safety
/// `field` must be a live `AXUIElementRef` or null.
pub(crate) unsafe fn read_field_facts(
    field: crate::ax::bindings::AXUIElementRef,
    is_requested: Option<bool>,
    key_driven: bool,
) -> FieldFacts {
    use crate::ax::bindings::{copy_bool_attr, copy_string_attr, is_attribute_settable};
    if field.is_null() {
        return FieldFacts {
            has_focus: false,
            is_requested,
            role: None,
            enabled: None,
            value_settable: false,
            selected_text_settable: false,
            secure: false,
            key_driven,
        };
    }
    FieldFacts {
        has_focus: true,
        is_requested,
        role: copy_string_attr(field, "AXRole"),
        enabled: copy_bool_attr(field, "AXEnabled"),
        value_settable: is_attribute_settable(field, "AXValue"),
        selected_text_settable: is_attribute_settable(field, "AXSelectedText"),
        secure: copy_string_attr(field, "AXSubrole").as_deref() == Some("AXSecureTextField"),
        key_driven,
    }
}

// ── Rate guard ───────────────────────────────────────────────────────────────

/// Process-wide guard against repeated rejected bursts. Only keystrokes a
/// field visibly didn't take count — each is a beep — so typing that lands is
/// never limited. One rejected probe stops its own call; this stops an agent
/// that keeps retrying into a field that won't take input.
#[derive(Default)]
pub(crate) struct RateGuard {
    rejected: Mutex<VecDeque<Instant>>,
}

impl RateGuard {
    fn rejected(&self, now: Instant) -> std::sync::MutexGuard<'_, VecDeque<Instant>> {
        let mut rejected = self
            .rejected
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while rejected
            .front()
            .is_some_and(|at| now.saturating_duration_since(*at) >= RATE_WINDOW)
        {
            rejected.pop_front();
        }
        rejected
    }

    /// Whether foreground keystrokes may be posted.
    pub(crate) fn open(&self, now: Instant) -> bool {
        self.rejected(now).len() < MAX_REJECTED_PER_WINDOW
    }

    pub(crate) fn record_rejected(&self, now: Instant, keystrokes: usize) {
        self.rejected(now)
            .extend(std::iter::repeat_n(now, keystrokes));
    }

    /// When the guard opens again.
    pub(crate) fn retry_after(&self, now: Instant) -> Duration {
        let rejected = self.rejected(now);
        if rejected.len() < MAX_REJECTED_PER_WINDOW {
            return Duration::ZERO;
        }
        let freeing = rejected[rejected.len() - MAX_REJECTED_PER_WINDOW];
        (freeing + RATE_WINDOW).saturating_duration_since(now)
    }
}

pub(crate) fn rate_guard() -> &'static RateGuard {
    static GUARD: OnceLock<RateGuard> = OnceLock::new();
    GUARD.get_or_init(RateGuard::default)
}

// ── Cancellation ─────────────────────────────────────────────────────────────

/// Set when the call's future is dropped (its connection went away) or its
/// session is ending (the client stopped or exited). Checked before every
/// keystroke.
#[derive(Clone, Default)]
pub(crate) struct Cancellation {
    dropped: Arc<AtomicBool>,
    sessions: Vec<String>,
}

/// Marks the paired [`Cancellation`] cancelled when dropped. Held by the async
/// tool invocation for its whole lifetime.
pub(crate) struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

impl Cancellation {
    pub(crate) fn for_call(args: &serde_json::Value) -> (CancelOnDrop, Self) {
        let dropped = Arc::new(AtomicBool::new(false));
        let sessions = ["_session_id", "_transport_session_id"]
            .into_iter()
            .filter_map(|key| args.get(key).and_then(serde_json::Value::as_str))
            .filter(|id| !id.is_empty() && *id != "default")
            .map(str::to_owned)
            .collect();
        (CancelOnDrop(dropped.clone()), Self { dropped, sessions })
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.dropped.load(Ordering::Acquire)
            || self
                .sessions
                .iter()
                .any(|id| cua_driver_core::session::is_session_ending(id))
    }
}

// ── Guarded keystrokes ───────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeystrokeStop {
    Completed,
    /// The field's value did not change after keystrokes were posted.
    FieldUnchanged,
    /// The target lost keyboard focus.
    FocusLost,
    Cancelled,
    RateLimited,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct KeystrokeReport {
    pub posted: usize,
    pub stop: KeystrokeStop,
}

/// What guarded typing drives. Real implementations post CGEvents and read the
/// field through accessibility; tests use a scripted field.
pub(crate) trait KeystrokeTarget {
    fn post(&mut self, ch: char) -> anyhow::Result<()>;
    /// The field's current value; `None` when it can't be read.
    fn value(&mut self) -> Option<String>;
    /// Whether the field still holds keyboard focus.
    fn focused(&mut self) -> bool;
    fn now(&self) -> Instant;
    fn sleep(&mut self, duration: Duration);
}

enum Acceptance {
    Changed(String),
    Unchanged,
    Unreadable,
}

/// Wait briefly for the field to show the keystrokes posted since `last`.
fn await_acceptance(target: &mut impl KeystrokeTarget, last: Option<&str>) -> Acceptance {
    let deadline = target.now() + ACCEPT_WAIT;
    loop {
        match (target.value(), last) {
            (None, _) => return Acceptance::Unreadable,
            (Some(value), None) => return Acceptance::Changed(value),
            (Some(value), Some(last)) if value != last => return Acceptance::Changed(value),
            _ => {}
        }
        if target.now() >= deadline {
            return Acceptance::Unchanged;
        }
        target.sleep(ACCEPT_POLL);
    }
}

fn is_checkpoint(posted: usize, total: usize) -> bool {
    posted == total
        || posted == PROBE_KEYSTROKES
        || (posted > PROBE_KEYSTROKES && (posted - PROBE_KEYSTROKES) % CHECK_INTERVAL == 0)
}

/// Post `text` one character at a time, stopping as soon as the field stops
/// taking keystrokes, loses focus, the call is cancelled, or the rate guard
/// trips. A field whose value can't be read gets no acceptance checks.
pub(crate) fn post_guarded(
    text: &str,
    before: Option<&str>,
    target: &mut impl KeystrokeTarget,
    cancel: &Cancellation,
    guard: &RateGuard,
) -> anyhow::Result<KeystrokeReport> {
    let total = text.chars().count();
    let mut last = before.map(str::to_owned);
    let mut posted = 0;
    // Keystrokes posted when the field last showed a change.
    let mut accepted_through = 0;
    let report = |posted, stop| Ok(KeystrokeReport { posted, stop });
    for ch in text.chars() {
        if cancel.is_cancelled() {
            return report(posted, KeystrokeStop::Cancelled);
        }
        if !guard.open(target.now()) {
            return report(posted, KeystrokeStop::RateLimited);
        }
        target.post(ch)?;
        posted += 1;
        if !is_checkpoint(posted, total) {
            continue;
        }
        if !target.focused() {
            return report(posted, KeystrokeStop::FocusLost);
        }
        match await_acceptance(target, last.as_deref()) {
            Acceptance::Changed(value) => {
                last = Some(value);
                accepted_through = posted;
            }
            Acceptance::Unchanged => {
                guard.record_rejected(target.now(), posted - accepted_through);
                return report(posted, KeystrokeStop::FieldUnchanged);
            }
            Acceptance::Unreadable => {}
        }
    }
    report(posted, KeystrokeStop::Completed)
}

// ── Results ──────────────────────────────────────────────────────────────────

fn in_field(verified: Option<usize>, requested: usize) -> String {
    match verified {
        Some(n) => format!("{n} of {requested} character(s) are in the field"),
        None => "the field can't be read back to count what landed".into(),
    }
}

/// Shape the tool result for guarded foreground keystrokes. `posted` keystrokes
/// and `verified` characters (read back in the field; `None` when it can't be
/// read) are reported separately, and a retry is suggested only for a suffix
/// after characters that were actually verified.
pub(crate) fn keystroke_result(
    path: &'static str,
    requested: usize,
    report: KeystrokeReport,
    verified: Option<usize>,
    detail: &str,
    web_content: bool,
) -> ToolResult {
    let posted = report.posted;
    let complete = report.stop == KeystrokeStop::Completed && verified == Some(requested);
    if complete {
        let mut structured = serde_json::json!({
            "path": path,
            "requested_chars": requested,
            "posted_keystrokes": posted,
            "verified_chars": requested,
            "verified": !web_content,
            "effect": if web_content { "unverifiable" } else { "confirmed" },
        });
        if web_content {
            structured["escalation"] = serde_json::json!({
                "recommended": "page",
                "reason": "Web content: the field's accessibility value is not proof the page saw the input. Confirm with a screenshot, or drive the page with a browser tool.",
            });
            return ToolResult::text(format!(
                "📨 Sent {requested} char(s){detail} — web content; confirm via screenshot."
            ))
            .with_structured(structured);
        }
        return ToolResult::text(format!("✅ Inserted {requested} char(s){detail}."))
            .with_structured(structured);
    }
    if report.stop == KeystrokeStop::Completed && verified.is_none() {
        return ToolResult::text(format!(
            "📨 Sent {posted} keystroke(s){detail} — {}; verify via screenshot.",
            in_field(None, requested)
        ))
        .with_structured(serde_json::json!({
            "path": path,
            "requested_chars": requested,
            "posted_keystrokes": posted,
            "verified_chars": null,
            "verified": false,
            "effect": "unverifiable",
            "retryable": false,
        }));
    }

    let landed = in_field(verified, requested);
    let retry_from = verified.filter(|n| *n > 0 && *n < requested);
    let retryable = retry_from.is_some()
        && matches!(
            report.stop,
            KeystrokeStop::FocusLost | KeystrokeStop::Completed
        );
    let (code, message, escalation) = match report.stop {
        KeystrokeStop::FieldUnchanged => (
            "foreground_field_not_accepting",
            format!(
                "Foreground typing stopped: the focused field didn't take the keystrokes (its value didn't change after {posted}); {landed}."
            ),
            Some((
                "verify_state",
                "Take a fresh look: the field may be read-only or another control may hold focus. For a web page, use a browser tool instead.",
            )),
        ),
        KeystrokeStop::FocusLost => (
            "foreground_focus_lost",
            format!("Foreground typing stopped: the field lost keyboard focus after {posted} keystroke(s); {landed}."),
            Some(("bring_to_front", "Refocus the field before typing again.")),
        ),
        KeystrokeStop::Cancelled => (
            "cancelled",
            format!("Foreground typing cancelled after {posted} keystroke(s); {landed}."),
            None,
        ),
        KeystrokeStop::RateLimited => (
            "foreground_input_rate_limited",
            format!(
                "Foreground typing paused: fields kept rejecting keystrokes in the last minute ({posted} sent in this call; {landed}). Foreground typing stays paused for up to a minute."
            ),
            Some((
                "needs_person",
                "Stop sending input and ask the person to check the screen before continuing.",
            )),
        ),
        KeystrokeStop::Completed => (
            "type_text_incomplete",
            format!("Typed {posted} keystroke(s) but {landed}."),
            Some((
                "verify_state",
                "Re-read the field before deciding whether any text remains to be typed.",
            )),
        ),
    };
    let message = if retryable {
        let n = retry_from.unwrap_or_default();
        format!("{message} Retry only the remaining suffix, from character {n}.")
    } else {
        message
    };
    let effect = match verified {
        Some(0) => "refused",
        Some(_) => "partial",
        None => "unverifiable",
    };
    let mut structured = serde_json::json!({
        "code": code,
        "path": path,
        "effect": effect,
        "requested_chars": requested,
        "posted_keystrokes": posted,
        "verified_chars": verified,
        "retryable": retryable,
    });
    if retryable {
        structured["retry_from_character"] = serde_json::json!(retry_from);
    }
    if let Some((recommended, reason)) = escalation {
        structured["escalation"] = serde_json::json!({
            "recommended": recommended,
            "reason": reason,
        });
    }
    ToolResult::error(message).with_structured(structured)
}

/// The focused-field check refused: no keystroke was sent.
pub(crate) fn field_refusal_result(
    path: &'static str,
    requested: usize,
    refusal: &FieldRefusal,
) -> ToolResult {
    ToolResult::error(format!(
        "Foreground typing refused: {}. No keystrokes were sent. Focus an editable field first (click it, then take a fresh look); for a web page, use a browser tool.",
        refusal.reason
    ))
    .with_structured(serde_json::json!({
        "code": refusal.code,
        "path": path,
        "effect": "refused",
        "reason": refusal.reason,
        "requested_chars": requested,
        "posted_keystrokes": 0,
        "verified_chars": 0,
        "retryable": false,
        "escalation": {
            "recommended": "verify_state",
            "reason": "Keystrokes go to the focused element, and it is not the requested editable field.",
        },
    }))
}

/// The rate guard is closed: no keystroke was sent.
pub(crate) fn rate_limited_result(path: &'static str, requested: usize) -> ToolResult {
    let mut result = keystroke_result(
        path,
        requested,
        KeystrokeReport {
            posted: 0,
            stop: KeystrokeStop::RateLimited,
        },
        Some(0),
        "",
        false,
    );
    let retry_after = rate_guard().retry_after(Instant::now());
    if let Some(structured) = result.structured_content.as_mut() {
        structured["retry_after_ms"] = serde_json::json!(retry_after.as_millis() as u64);
    }
    result
}

/// Long text whose one-call accessibility write could not be confirmed either
/// way: retyping could duplicate it, so the caller must look first.
pub(crate) fn atomic_indeterminate_result(
    path: &'static str,
    requested: usize,
    verified: Option<usize>,
) -> ToolResult {
    let landed = in_field(verified, requested);
    ToolResult::error(format!(
        "type_text could not confirm its accessibility write of {requested} characters; {landed}. No keystrokes were sent. Re-read the field before typing again."
    ))
    .with_structured(serde_json::json!({
        "code": "type_text_indeterminate",
        "path": path,
        "effect": "indeterminate",
        "requested_chars": requested,
        "posted_keystrokes": 0,
        "verified_chars": verified,
        "retryable": false,
        "escalation": {
            "recommended": "verify_state",
            "reason": "Re-read the field before deciding whether any text remains to be typed.",
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scripted text field on a fake clock.
    struct Field {
        value: Option<String>,
        accepts: bool,
        focused: bool,
        lose_focus_after: Option<usize>,
        cancel_after: Option<(usize, Arc<AtomicBool>)>,
        posted: Vec<char>,
        clock: Instant,
    }

    impl Field {
        fn new(value: &str, accepts: bool) -> Self {
            Self {
                value: Some(value.to_owned()),
                accepts,
                focused: true,
                lose_focus_after: None,
                cancel_after: None,
                posted: Vec::new(),
                clock: Instant::now(),
            }
        }
    }

    impl KeystrokeTarget for Field {
        fn post(&mut self, ch: char) -> anyhow::Result<()> {
            self.posted.push(ch);
            if self.accepts {
                if let Some(value) = self.value.as_mut() {
                    value.push(ch);
                }
            }
            if self.lose_focus_after == Some(self.posted.len()) {
                self.focused = false;
            }
            if let Some((after, flag)) = &self.cancel_after {
                if *after == self.posted.len() {
                    flag.store(true, Ordering::Release);
                }
            }
            self.clock += Duration::from_millis(38);
            Ok(())
        }
        fn value(&mut self) -> Option<String> {
            self.value.clone()
        }
        fn focused(&mut self) -> bool {
            self.focused
        }
        fn now(&self) -> Instant {
            self.clock
        }
        fn sleep(&mut self, duration: Duration) {
            self.clock += duration;
        }
    }

    fn url(len: usize) -> String {
        "https://example.com/?q="
            .chars()
            .cycle()
            .take(len)
            .collect()
    }

    // ── 1. An unfocused or read-only target is refused before typing ──

    fn text_field() -> FieldFacts {
        FieldFacts {
            has_focus: true,
            is_requested: None,
            role: Some("AXTextField".into()),
            enabled: Some(true),
            value_settable: true,
            selected_text_settable: true,
            secure: false,
            key_driven: false,
        }
    }

    #[test]
    fn a_focused_editable_field_is_accepted() {
        assert_eq!(classify_field(&text_field()), Ok(()));
        assert_eq!(
            classify_field(&FieldFacts {
                is_requested: Some(true),
                ..text_field()
            }),
            Ok(())
        );
    }

    #[test]
    fn a_requested_element_that_is_not_focused_is_refused() {
        let refusal = classify_field(&FieldFacts {
            is_requested: Some(false),
            ..text_field()
        })
        .expect_err("keystrokes would go to another element");
        assert_eq!(refusal.code, "foreground_target_not_focused");
    }

    #[test]
    fn no_focused_element_is_refused() {
        let refusal = classify_field(&FieldFacts {
            has_focus: false,
            role: None,
            enabled: None,
            value_settable: false,
            selected_text_settable: false,
            ..text_field()
        })
        .expect_err("nothing would take the keystrokes");
        assert_eq!(refusal.code, "foreground_target_not_focused");
    }

    #[test]
    fn a_focused_non_text_element_is_refused() {
        // The terra incident: Chrome's focus was on the web area / window,
        // not the address bar.
        for role in ["AXWebArea", "AXWindow", "AXButton", "AXGroup"] {
            let refusal = classify_field(&FieldFacts {
                role: Some(role.into()),
                value_settable: false,
                selected_text_settable: false,
                ..text_field()
            })
            .expect_err(role);
            assert_eq!(refusal.code, "foreground_target_not_editable", "{role}");
        }
    }

    #[test]
    fn a_disabled_field_is_refused() {
        let disabled = classify_field(&FieldFacts {
            enabled: Some(false),
            ..text_field()
        })
        .expect_err("disabled field");
        assert_eq!(disabled.code, "foreground_target_not_editable");
    }

    #[test]
    fn text_fields_ax_calls_unsettable_still_get_the_acceptance_probe() {
        // Some editable fields (Catalyst) report no settable value; a truly
        // read-only one is stopped by the probe instead of refused up front.
        assert_eq!(
            classify_field(&FieldFacts {
                value_settable: false,
                selected_text_settable: false,
                ..text_field()
            }),
            Ok(())
        );
    }

    #[test]
    fn custom_editors_and_terminals_are_typable() {
        // A canvas or custom editor that exposes a settable text value.
        assert_eq!(
            classify_field(&FieldFacts {
                role: Some("AXGroup".into()),
                ..text_field()
            }),
            Ok(())
        );
        // A terminal without any AX text area.
        assert_eq!(
            classify_field(&FieldFacts {
                role: Some("AXGroup".into()),
                value_settable: false,
                selected_text_settable: false,
                key_driven: true,
                ..text_field()
            }),
            Ok(())
        );
    }

    #[test]
    fn secure_and_terminal_values_are_not_acceptance_evidence() {
        assert!(text_field().value_readable());
        assert!(!FieldFacts {
            secure: true,
            ..text_field()
        }
        .value_readable());
        assert!(!FieldFacts {
            key_driven: true,
            ..text_field()
        }
        .value_readable());
    }

    // ── 1. A field that doesn't take keystrokes stops early ──

    #[test]
    fn a_field_that_never_changes_stops_after_the_probe() {
        let mut field = Field::new("", false);
        let guard = RateGuard::default();
        let report = post_guarded(
            &url(1_067),
            Some(""),
            &mut field,
            &Cancellation::default(),
            &guard,
        )
        .unwrap();
        assert_eq!(report.stop, KeystrokeStop::FieldUnchanged);
        assert_eq!(report.posted, PROBE_KEYSTROKES);
        assert_eq!(field.posted.len(), PROBE_KEYSTROKES);
    }

    #[test]
    fn a_field_that_stops_taking_keys_midway_stops_at_the_next_check() {
        let mut field = Field::new("", true);
        struct Stalls(Field, usize);
        impl KeystrokeTarget for Stalls {
            fn post(&mut self, ch: char) -> anyhow::Result<()> {
                self.0.accepts = self.0.posted.len() < self.1;
                self.0.post(ch)
            }
            fn value(&mut self) -> Option<String> {
                self.0.value()
            }
            fn focused(&mut self) -> bool {
                self.0.focused()
            }
            fn now(&self) -> Instant {
                self.0.now()
            }
            fn sleep(&mut self, duration: Duration) {
                self.0.sleep(duration)
            }
        }
        field.value = Some(String::new());
        let mut stalls = Stalls(field, 5);
        let report = post_guarded(
            &url(40),
            Some(""),
            &mut stalls,
            &Cancellation::default(),
            &RateGuard::default(),
        )
        .unwrap();
        assert_eq!(report.stop, KeystrokeStop::FieldUnchanged);
        assert!(
            report.posted <= PROBE_KEYSTROKES + 2 * CHECK_INTERVAL,
            "posted {}",
            report.posted
        );
    }

    #[test]
    fn an_accepting_field_receives_the_whole_text() {
        // Long text that lands is never capped or rate limited.
        let mut field = Field::new("ab", true);
        let text = url(1_067);
        let report = post_guarded(
            &text,
            Some("ab"),
            &mut field,
            &Cancellation::default(),
            &RateGuard::default(),
        )
        .unwrap();
        assert_eq!(report.stop, KeystrokeStop::Completed);
        assert_eq!(report.posted, text.chars().count());
        assert_eq!(field.value.as_deref(), Some(format!("ab{text}").as_str()));
    }

    #[test]
    fn typing_stops_when_focus_moves_away() {
        let mut field = Field::new("", true);
        field.lose_focus_after = Some(6);
        let report = post_guarded(
            &url(30),
            Some(""),
            &mut field,
            &Cancellation::default(),
            &RateGuard::default(),
        )
        .unwrap();
        assert_eq!(report.stop, KeystrokeStop::FocusLost);
        assert!(report.posted <= PROBE_KEYSTROKES + CHECK_INTERVAL);
    }

    // ── 3. Long text tries one accessibility write first ──

    #[test]
    fn long_foreground_text_tries_one_accessibility_write_first() {
        assert_eq!(
            foreground_route(LONG_TEXT_CHARS, true),
            ForegroundRoute::Keystrokes
        );
        assert_eq!(
            foreground_route(LONG_TEXT_CHARS + 1, true),
            ForegroundRoute::AtomicAxFirst
        );
        assert_eq!(
            foreground_route(1_067, true),
            ForegroundRoute::AtomicAxFirst
        );
        // Without a readable value a write can't be confirmed, so it types.
        assert_eq!(foreground_route(1_067, false), ForegroundRoute::Keystrokes);
    }

    // ── 5. Cancellation stops typing before the next keystroke ──

    #[test]
    fn a_dropped_call_stops_typing_before_the_next_keystroke() {
        let (drop_guard, cancel) = Cancellation::for_call(&serde_json::json!({}));
        let mut field = Field::new("", true);
        let flag = Arc::new(AtomicBool::new(false));
        field.cancel_after = Some((4, flag.clone()));
        struct DropsAt(Field, Option<CancelOnDrop>, Arc<AtomicBool>);
        impl KeystrokeTarget for DropsAt {
            fn post(&mut self, ch: char) -> anyhow::Result<()> {
                self.0.post(ch)?;
                if self.2.load(Ordering::Acquire) {
                    self.1.take();
                }
                Ok(())
            }
            fn value(&mut self) -> Option<String> {
                self.0.value()
            }
            fn focused(&mut self) -> bool {
                self.0.focused()
            }
            fn now(&self) -> Instant {
                self.0.now()
            }
            fn sleep(&mut self, duration: Duration) {
                self.0.sleep(duration)
            }
        }
        let mut target = DropsAt(field, Some(drop_guard), flag);
        let report = post_guarded(
            &url(40),
            Some(""),
            &mut target,
            &cancel,
            &RateGuard::default(),
        )
        .unwrap();
        assert_eq!(report.stop, KeystrokeStop::Cancelled);
        assert_eq!(report.posted, 4);
    }

    #[test]
    fn an_ending_session_cancels_its_typing() {
        let session = format!("typing-cancel-{}", std::process::id());
        let (_guard, cancel) =
            Cancellation::for_call(&serde_json::json!({ "_session_id": session }));
        assert!(!cancel.is_cancelled());
        cua_driver_core::session::fire_session_end(&session);
        assert!(cancel.is_cancelled());
    }

    // ── 6. The rate guard stops bursts ──

    #[test]
    fn repeated_rejected_probes_close_the_guard_until_they_age_out() {
        let guard = RateGuard::default();
        let start = Instant::now();
        for _ in 0..(MAX_REJECTED_PER_WINDOW / PROBE_KEYSTROKES) {
            assert!(guard.open(start));
            guard.record_rejected(start, PROBE_KEYSTROKES);
        }
        assert!(
            !guard.open(start),
            "four rejected probes in a minute need the person"
        );
        assert!(guard.retry_after(start) > Duration::ZERO);
        assert!(guard.open(start + RATE_WINDOW));
    }

    #[test]
    fn keystrokes_that_land_never_close_the_guard() {
        let guard = RateGuard::default();
        for _ in 0..20 {
            let mut field = Field::new("", true);
            let report = post_guarded(
                &url(500),
                Some(""),
                &mut field,
                &Cancellation::default(),
                &guard,
            )
            .unwrap();
            assert_eq!(report.stop, KeystrokeStop::Completed);
        }
        assert!(guard.open(Instant::now()));
    }

    #[test]
    fn rejecting_fields_trip_the_guard_across_calls() {
        let guard = RateGuard::default();
        let mut stops = Vec::new();
        for _ in 0..5 {
            let mut field = Field::new("", false);
            let report = post_guarded(
                &url(40),
                Some(""),
                &mut field,
                &Cancellation::default(),
                &guard,
            )
            .unwrap();
            stops.push((report.stop, report.posted));
        }
        assert_eq!(stops[0], (KeystrokeStop::FieldUnchanged, PROBE_KEYSTROKES));
        assert_eq!(stops[4], (KeystrokeStop::RateLimited, 0));
    }

    // ── 2. Counts are reported separately and honestly ──

    #[test]
    fn nothing_verified_never_asks_for_a_retry() {
        let result = keystroke_result(
            "key_events_fg",
            1_067,
            KeystrokeReport {
                posted: 720,
                stop: KeystrokeStop::Completed,
            },
            Some(0),
            "",
            false,
        );
        assert_eq!(result.is_error, Some(true));
        let s = result.structured_content.unwrap();
        assert_eq!(s["posted_keystrokes"], 720);
        assert_eq!(s["verified_chars"], 0);
        assert_eq!(s["retryable"], false);
        assert!(s.get("retry_from_character").is_none());
        let text = format!("{:?}", result.content);
        assert!(!text.contains("retry only the remaining suffix"), "{text}");
    }

    #[test]
    fn an_early_stop_reports_posted_and_verified_counts() {
        let result = keystroke_result(
            "key_events_fg",
            40,
            KeystrokeReport {
                posted: PROBE_KEYSTROKES,
                stop: KeystrokeStop::FieldUnchanged,
            },
            Some(0),
            "",
            false,
        );
        let s = result.structured_content.unwrap();
        assert_eq!(s["code"], "foreground_field_not_accepting");
        assert_eq!(s["effect"], "refused");
        assert_eq!(s["posted_keystrokes"], PROBE_KEYSTROKES);
        assert_eq!(s["verified_chars"], 0);
        assert_eq!(s["retryable"], false);
    }

    #[test]
    fn unreadable_fields_report_no_verified_count() {
        let result = keystroke_result(
            "key_events_fg",
            10,
            KeystrokeReport {
                posted: 10,
                stop: KeystrokeStop::Completed,
            },
            None,
            "",
            false,
        );
        let s = result.structured_content.unwrap();
        assert_eq!(s["posted_keystrokes"], 10);
        assert!(s["verified_chars"].is_null());
        assert_eq!(s["effect"], "unverifiable");
        assert_eq!(s["retryable"], false);
    }

    #[test]
    fn a_verified_prefix_names_the_exact_suffix_to_retry() {
        let result = keystroke_result(
            "key_events_fg",
            40,
            KeystrokeReport {
                posted: 14,
                stop: KeystrokeStop::FocusLost,
            },
            Some(12),
            "",
            false,
        );
        let s = result.structured_content.unwrap();
        assert_eq!(s["code"], "foreground_focus_lost");
        assert_eq!(s["effect"], "partial");
        assert_eq!(s["posted_keystrokes"], 14);
        assert_eq!(s["verified_chars"], 12);
        assert_eq!(s["retryable"], true);
        assert_eq!(s["retry_from_character"], 12);
    }

    #[test]
    fn a_rate_limited_stop_escalates_to_the_person() {
        let result = keystroke_result(
            "key_events_fg",
            40,
            KeystrokeReport {
                posted: 0,
                stop: KeystrokeStop::RateLimited,
            },
            Some(0),
            "",
            false,
        );
        let s = result.structured_content.unwrap();
        assert_eq!(s["code"], "foreground_input_rate_limited");
        assert_eq!(s["escalation"]["recommended"], "needs_person");
        assert_eq!(s["retryable"], false);
    }
}
