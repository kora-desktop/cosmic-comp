// SPDX-License-Identifier: GPL-3.0-only

//! A failure the person may not have seen. Its toast always shows; it also goes
//! to the inbox when, from the failure until the toast leaves, the screen is
//! locked, every display is off, or no one has used the computer for a minute.

use std::time::Duration;

use calloop::timer::{TimeoutAction, Timer};
use tracing::warn;

use crate::{
    dbus::notifications::{Notification, Tone, toasts_enabled},
    state::State,
};

/// kora-notifications shows a system toast for 3.2 s, then lets it leave.
pub const TOAST_LIFETIME: Duration = Duration::from_secs(4);

/// No input for this long, with nothing such as a video holding the screen awake.
pub const AWAY_AFTER: Duration = Duration::from_secs(60);

/// Whether the person can see the screen right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Presence {
    pub locked: bool,
    pub display_on: bool,
    pub since_input: Duration,
    /// An idle inhibitor, such as a playing video, holds the screen awake.
    pub held_awake: bool,
}

impl Presence {
    pub fn of(state: &mut State) -> Self {
        let locked = state.common.shell.read().session_lock.is_some();
        Self {
            locked,
            display_on: crate::wayland::handlers::output_power::any_display_on(state),
            since_input: state.common.last_input.elapsed(),
            held_awake: state.common.idle_notifier_state.is_inhibited(),
        }
    }

    pub fn away(&self) -> bool {
        self.locked || !self.display_on || (!self.held_awake && self.since_input >= AWAY_AFTER)
    }
}

/// A failure's inbox copy, given up at most once.
#[derive(Debug)]
pub struct Missable(Option<Notification>);

impl Missable {
    pub fn new(inbox: Notification) -> Self {
        Self(Some(inbox))
    }

    /// The inbox copy, the first time the person is away.
    pub fn missed(&mut self, presence: Presence) -> Option<Notification> {
        if !presence.away() {
            return None;
        }
        self.0.take()
    }
}

/// Show `message` in the error toast, and post `inbox` too if the person misses it.
/// Without Kora's toasts `fallback` is posted instead.
pub fn fail(state: &mut State, message: String, fallback: Notification, inbox: Notification) {
    state
        .common
        .dbus_state
        .system_toast_or(message, Tone::Destructive, fallback);
    if !toasts_enabled() {
        return;
    }
    let mut missable = Missable::new(inbox);
    if let Some(inbox) = missable.missed(Presence::of(state)) {
        state.common.dbus_state.notify(inbox);
        return;
    }
    let watched = state.common.event_loop_handle.insert_source(
        Timer::from_duration(TOAST_LIFETIME),
        move |_, _, state| {
            if let Some(inbox) = missable.missed(Presence::of(state)) {
                state.common.dbus_state.notify(inbox);
            }
            TimeoutAction::Drop
        },
    );
    if let Err(err) = watched {
        warn!(?err, "Cannot watch a failure toast");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn here() -> Presence {
        Presence {
            locked: false,
            display_on: true,
            since_input: Duration::from_secs(2),
            held_awake: false,
        }
    }

    fn inbox() -> Notification {
        Notification {
            app_name: "Recording".into(),
            app_icon: String::new(),
            summary: "Recording failed".into(),
            body: "No space left".into(),
            expire_timeout: -1,
            transient: false,
            target: None,
        }
    }

    #[test]
    fn a_failure_seen_while_active_stays_out_of_the_inbox() {
        let mut failure = Missable::new(inbox());
        assert!(failure.missed(here()).is_none());
        // The toast left while the person was still there.
        assert!(failure.missed(here()).is_none());
    }

    #[test]
    fn a_failure_while_idle_locked_or_dark_goes_to_the_inbox() {
        let idle = Presence {
            since_input: AWAY_AFTER,
            ..here()
        };
        let locked = Presence {
            locked: true,
            ..here()
        };
        let dark = Presence {
            display_on: false,
            ..here()
        };
        for presence in [idle, locked, dark] {
            let posted = Missable::new(inbox()).missed(presence);
            assert_eq!(
                posted.map(|n| n.summary).as_deref(),
                Some("Recording failed")
            );
        }
    }

    #[test]
    fn a_toast_that_leaves_unseen_goes_to_the_inbox_once() {
        let mut failure = Missable::new(inbox());
        assert!(failure.missed(here()).is_none());
        let locked_meanwhile = Presence {
            locked: true,
            ..here()
        };
        assert!(failure.missed(locked_meanwhile).is_some());
        assert!(failure.missed(locked_meanwhile).is_none());
    }

    #[test]
    fn a_playing_video_keeps_the_person_present() {
        let watching = Presence {
            since_input: AWAY_AFTER * 5,
            held_awake: true,
            ..here()
        };
        assert!(!watching.away());
    }
}
