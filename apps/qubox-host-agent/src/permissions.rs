//! Session permission matrix: pure decisions for input / clipboard / mic.

use qubox_proto::SessionPermissions;

/// Whether remote input events may be injected on the host.
#[inline]
pub fn allow_input(p: &SessionPermissions) -> bool {
    p.input
}

/// Whether host may apply client→host clipboard payloads.
#[inline]
pub fn allow_clipboard_apply(p: &SessionPermissions) -> bool {
    p.clipboard
}

/// Whether host may advertise host→client clipboard changes.
#[inline]
pub fn allow_clipboard_watch(p: &SessionPermissions) -> bool {
    p.clipboard
}

/// Whether host may create a virtual mic for client audio.
#[inline]
pub fn allow_mic(p: &SessionPermissions) -> bool {
    p.mic
}

/// Outcome of a mic start request under the current permission mask.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicStartDecision {
    Accept,
    Deny,
}

#[inline]
pub fn decide_mic_start(p: &SessionPermissions) -> MicStartDecision {
    if allow_mic(p) {
        MicStartDecision::Accept
    } else {
        MicStartDecision::Deny
    }
}

/// Outcome for an inbound clipboard change from the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardApplyDecision {
    Apply,
    DropDenied,
    DropDirectionOff,
}

/// `client_to_host` is true when host clipboard sync direction includes client→host.
#[inline]
pub fn decide_clipboard_apply(
    p: &SessionPermissions,
    client_to_host: bool,
) -> ClipboardApplyDecision {
    if !allow_clipboard_apply(p) {
        return ClipboardApplyDecision::DropDenied;
    }
    if !client_to_host {
        return ClipboardApplyDecision::DropDirectionOff;
    }
    ClipboardApplyDecision::Apply
}

/// Outcome for remote input events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputDecision {
    Inject,
    DropStream,
}

#[inline]
pub fn decide_input(p: &SessionPermissions) -> InputDecision {
    if allow_input(p) {
        InputDecision::Inject
    } else {
        InputDecision::DropStream
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn perms(input: bool, clipboard: bool, mic: bool) -> SessionPermissions {
        SessionPermissions {
            input,
            clipboard,
            mic,
        }
    }

    #[test]
    fn matrix_all_allowed() {
        let p = perms(true, true, true);
        assert_eq!(decide_input(&p), InputDecision::Inject);
        assert_eq!(
            decide_clipboard_apply(&p, true),
            ClipboardApplyDecision::Apply
        );
        assert_eq!(decide_mic_start(&p), MicStartDecision::Accept);
        assert!(allow_clipboard_watch(&p));
    }

    #[test]
    fn matrix_all_denied() {
        let p = perms(false, false, false);
        assert_eq!(decide_input(&p), InputDecision::DropStream);
        assert_eq!(
            decide_clipboard_apply(&p, true),
            ClipboardApplyDecision::DropDenied
        );
        assert_eq!(decide_mic_start(&p), MicStartDecision::Deny);
        assert!(!allow_clipboard_watch(&p));
    }

    #[test]
    fn clipboard_permission_ok_but_direction_off() {
        let p = perms(true, true, true);
        assert_eq!(
            decide_clipboard_apply(&p, false),
            ClipboardApplyDecision::DropDirectionOff
        );
    }

    #[test]
    fn input_only_session() {
        let p = perms(true, false, false);
        assert_eq!(decide_input(&p), InputDecision::Inject);
        assert_eq!(
            decide_clipboard_apply(&p, true),
            ClipboardApplyDecision::DropDenied
        );
        assert_eq!(decide_mic_start(&p), MicStartDecision::Deny);
    }

    #[test]
    fn mic_only_session() {
        let p = perms(false, false, true);
        assert_eq!(decide_input(&p), InputDecision::DropStream);
        assert_eq!(decide_mic_start(&p), MicStartDecision::Accept);
    }

    #[test]
    fn default_permissions_allow_all() {
        let p = SessionPermissions::default();
        assert_eq!(decide_input(&p), InputDecision::Inject);
        assert_eq!(
            decide_clipboard_apply(&p, true),
            ClipboardApplyDecision::Apply
        );
        assert_eq!(decide_mic_start(&p), MicStartDecision::Accept);
    }
}
