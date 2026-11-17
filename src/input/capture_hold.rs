// SPDX-License-Identifier: GPL-3.0-only

//! Input held between a capture shortcut and the capture overlay taking the pointer.
//!
//! The overlay needs a first frame and a focus refresh before a click reaches it, about a
//! third of a second, and a click in that window pressed the app under it.

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use calloop::{
    RegistrationToken,
    timer::{TimeoutAction, Timer},
};
use smithay::{
    backend::input::{InputBackend, InputEvent},
    desktop::{WindowSurfaceType, layer_map_for_output},
    wayland::shell::wlr_layer::{KeyboardInteractivity, Layer},
};
use tracing::{info, warn};

use crate::{
    shell::{SeatExt, focus::target::PointerFocusTarget},
    state::State,
};

/// The longest input is held for an overlay that has not come: a cold capture program
/// maps well inside it, and past it a person would think the input had died.
pub const TIMEOUT: Duration = Duration::from_secs(2);

/// How an event is treated while input is held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Never reaches the app under the overlay.
    Key,
    /// A pointer, touch or tablet press or scroll: never reaches the app, and keeps the
    /// motion after it in order behind it.
    Press,
    /// Moves the cursor, unless a held press must keep it in order.
    Motion,
}

pub fn kind<B: InputBackend>(event: &InputEvent<B>) -> Option<Kind> {
    Some(match event {
        InputEvent::Keyboard { .. } => Kind::Key,
        InputEvent::PointerButton { .. }
        | InputEvent::PointerAxis { .. }
        | InputEvent::TouchDown { .. }
        | InputEvent::TouchMotion { .. }
        | InputEvent::TouchUp { .. }
        | InputEvent::TouchCancel { .. }
        | InputEvent::TouchFrame { .. }
        | InputEvent::TabletToolTip { .. }
        | InputEvent::TabletToolButton { .. }
        | InputEvent::GestureSwipeBegin { .. }
        | InputEvent::GestureSwipeUpdate { .. }
        | InputEvent::GestureSwipeEnd { .. }
        | InputEvent::GesturePinchBegin { .. }
        | InputEvent::GesturePinchUpdate { .. }
        | InputEvent::GesturePinchEnd { .. }
        | InputEvent::GestureHoldBegin { .. }
        | InputEvent::GestureHoldEnd { .. } => Kind::Press,
        InputEvent::PointerMotion { .. }
        | InputEvent::PointerMotionAbsolute { .. }
        | InputEvent::TabletToolAxis { .. }
        | InputEvent::TabletToolProximity { .. } => Kind::Motion,
        _ => return None,
    })
}

/// Whether a layer surface is a capture overlay: above everything and taking the keyboard.
pub fn is_capture_overlay(layer: Layer, keyboard: KeyboardInteractivity) -> bool {
    layer == Layer::Overlay && keyboard == KeyboardInteractivity::Exclusive
}

/// Events held since a capture shortcut, replayed in order once the overlay has the pointer.
#[derive(Debug)]
pub struct Hold<T> {
    events: VecDeque<T>,
    pointer_held: bool,
}

impl<T> Default for Hold<T> {
    fn default() -> Self {
        Self {
            events: VecDeque::new(),
            pointer_held: false,
        }
    }
}

impl<T> Hold<T> {
    /// Whether an event of `kind` must wait for the overlay.
    pub fn holds(&mut self, kind: Kind) -> bool {
        // Motion after a held click would replay the click where the pointer went next.
        if kind == Kind::Motion && !self.pointer_held {
            return false;
        }
        self.pointer_held |= kind == Kind::Press;
        true
    }

    pub fn push(&mut self, event: T) {
        self.events.push_back(event);
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn into_events(self) -> VecDeque<T> {
        self.events
    }
}

type Held = Box<dyn FnOnce(&mut State)>;

/// The hold in progress, and the timer that ends it if no overlay comes.
pub struct Active {
    hold: Hold<Held>,
    timer: RegistrationToken,
    since: Instant,
}

impl std::fmt::Debug for Active {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Active")
            .field("held", &self.hold.len())
            .field("since", &self.since)
            .finish()
    }
}

impl State {
    /// Holds presses and keys from now until a capture overlay has the pointer.
    pub fn hold_input_for_capture(&mut self) {
        let timer = self.common.event_loop_handle.insert_source(
            Timer::from_duration(TIMEOUT),
            |_, _, state| {
                state.end_capture_hold(false);
                TimeoutAction::Drop
            },
        );
        let timer = match timer {
            Ok(timer) => timer,
            Err(err) => {
                warn!(?err, "cannot time a capture input hold, not holding");
                return;
            }
        };
        if let Some(active) = self.capture_hold.as_mut() {
            let old = std::mem::replace(&mut active.timer, timer);
            self.common.event_loop_handle.remove(old);
            return;
        }
        info!("capture shortcut: holding input until the capture overlay has the pointer");
        self.capture_hold = Some(Active {
            hold: Hold::default(),
            timer,
            since: Instant::now(),
        });
    }

    /// Keeps `event` while input is held; hands it back to be handled now otherwise.
    pub fn hold_capture_input<B>(&mut self, event: InputEvent<B>) -> Option<InputEvent<B>>
    where
        B: InputBackend + 'static,
        <B as InputBackend>::Device: 'static,
    {
        let (Some(active), Some(kind)) = (self.capture_hold.as_mut(), kind(&event)) else {
            return Some(event);
        };
        if !active.hold.holds(kind) {
            return Some(event);
        }
        active.hold.push(Box::new(move |state: &mut State| {
            state.process_input_event(event)
        }));
        None
    }

    /// A layer surface mapped or focus was refreshed: if a capture overlay now has the
    /// pointer, give it the held input. `now` moves the pointer focus without waiting
    /// for the next refresh.
    pub fn end_capture_hold_if_ready(&mut self, now: bool) {
        if self.capture_hold.is_none() {
            return;
        }
        let seat = self.common.shell.read().seats.last_active().clone();
        if now {
            crate::shell::focus::update_pointer_focus(self, &seat);
        }
        let Some(PointerFocusTarget::WlSurface { surface, .. }) = seat
            .get_pointer()
            .and_then(|pointer| pointer.current_focus())
        else {
            return;
        };
        let output = seat.active_output();
        let on_overlay = layer_map_for_output(&output)
            .layer_for_surface(&surface, WindowSurfaceType::ALL)
            .is_some_and(|layer| {
                is_capture_overlay(layer.layer(), layer.cached_state().keyboard_interactivity)
            });
        if on_overlay {
            self.end_capture_hold(true);
        }
    }

    /// Replays the held input in order: to the overlay once it has the pointer, else
    /// to whatever is under it, so a capture program that never shows loses no input.
    fn end_capture_hold(&mut self, overlay: bool) {
        let Some(active) = self.capture_hold.take() else {
            return;
        };
        self.common.event_loop_handle.remove(active.timer);
        let held = active.hold.len();
        let after_ms = active.since.elapsed().as_millis();
        if overlay {
            info!(
                held,
                after_ms, "capture overlay has the pointer: replaying held input"
            );
        } else {
            warn!(
                held,
                after_ms, "no capture overlay in time: delivering held input"
            );
        }
        for event in active.hold.into_events() {
            event(self);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn admit(hold: &mut Hold<&'static str>, kind: Kind, name: &'static str) -> bool {
        let held = hold.holds(kind);
        if held {
            hold.push(name);
        }
        held
    }

    /// The bug: a click right after Super+Shift+6 pressed the button under the colour picker.
    #[test]
    fn a_click_after_the_shortcut_waits_for_the_overlay() {
        let mut hold = Hold::default();
        assert!(admit(&mut hold, Kind::Press, "press"), "press held");
        assert!(admit(&mut hold, Kind::Press, "release"), "release held");
        assert_eq!(Vec::from(hold.into_events()), ["press", "release"]);
    }

    #[test]
    fn keys_wait_but_the_cursor_moves() {
        let mut hold = Hold::default();
        assert!(admit(&mut hold, Kind::Key, "shift up"));
        assert!(
            !admit(&mut hold, Kind::Motion, "move"),
            "motion goes through"
        );
        assert!(admit(&mut hold, Kind::Key, "escape"));
        assert_eq!(Vec::from(hold.into_events()), ["shift up", "escape"]);
    }

    /// A selection dragged before the overlay maps must start where the button went down.
    #[test]
    fn motion_after_a_held_click_stays_behind_it() {
        let mut hold = Hold::default();
        assert!(!admit(&mut hold, Kind::Motion, "hover"));
        assert!(admit(&mut hold, Kind::Press, "press"));
        assert!(admit(&mut hold, Kind::Motion, "drag"));
        assert!(admit(&mut hold, Kind::Press, "release"));
        assert_eq!(Vec::from(hold.into_events()), ["press", "drag", "release"]);
    }

    #[test]
    fn only_a_keyboard_taking_overlay_ends_the_hold() {
        use KeyboardInteractivity::{Exclusive, None, OnDemand};
        assert!(is_capture_overlay(Layer::Overlay, Exclusive));
        assert!(
            !is_capture_overlay(Layer::Overlay, None),
            "a toast or an OSD"
        );
        assert!(!is_capture_overlay(Layer::Overlay, OnDemand));
        assert!(!is_capture_overlay(Layer::Top, Exclusive), "a launcher");
    }
}
