// SPDX-License-Identifier: GPL-3.0-only

//! Input held between a capture shortcut and the capture overlay taking the pointer.
//!
//! The overlay needs a first frame and a focus refresh before a click reaches it, about a
//! third of a second, and a click in that window pressed the app under it. An Escape in
//! that window cancels the capture: it goes to the overlay alone, and the rest of the held
//! input to the app under it once the overlay is gone.

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use calloop::{
    RegistrationToken,
    timer::{TimeoutAction, Timer},
};
use smithay::{
    backend::input::{InputBackend, InputEvent, KeyState, KeyboardKeyEvent},
    desktop::{LayerSurface, WindowSurfaceType, layer_map_for_output},
    input::keyboard::Keysym,
    wayland::shell::wlr_layer::{KeyboardInteractivity, Layer},
};
use tracing::{info, warn};

use crate::{
    shell::{
        SeatExt,
        focus::target::{KeyboardFocusTarget, PointerFocusTarget},
    },
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

/// What a held Escape key event is, if it is one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Escape {
    No,
    Press(u32),
    Release(u32),
}

/// The Escape that cancelled the capture before its overlay came: its press and release
/// go to the overlay alone.
#[derive(Debug)]
struct Cancel<T> {
    key: u32,
    escape: Vec<T>,
    sent: bool,
}

/// Events held since a capture shortcut, replayed in order once the overlay has the pointer.
#[derive(Debug)]
pub struct Hold<T> {
    events: VecDeque<T>,
    pointer_held: bool,
    cancel: Option<Cancel<T>>,
}

impl<T> Default for Hold<T> {
    fn default() -> Self {
        Self {
            events: VecDeque::new(),
            pointer_held: false,
            cancel: None,
        }
    }
}

impl<T> Hold<T> {
    /// Keeps `event` while input is held; hands it back to be handled now otherwise.
    pub fn take(&mut self, kind: Kind, escape: Escape, event: T) -> Option<T> {
        // Motion after a held click would replay the click where the pointer went next.
        if kind == Kind::Motion && !self.pointer_held {
            return Some(event);
        }
        match (escape, self.cancel.as_mut()) {
            (Escape::Press(key), None) => {
                self.cancel = Some(Cancel {
                    key,
                    escape: vec![event],
                    sent: false,
                });
                return None;
            }
            (Escape::Release(key), Some(cancel)) if cancel.key == key => {
                // The press is the overlay's already: the release follows it there now.
                if cancel.sent {
                    return Some(event);
                }
                cancel.escape.push(event);
                return None;
            }
            _ => {}
        }
        self.pointer_held |= kind == Kind::Press;
        self.events.push_back(event);
        None
    }

    /// Whether an Escape cancelled the capture before its overlay came.
    pub fn cancelled(&self) -> bool {
        self.cancel.is_some()
    }

    /// The cancelling Escape, once, for the overlay that now has the keyboard.
    pub fn escape_for_overlay(&mut self) -> Vec<T> {
        match self.cancel.as_mut() {
            Some(cancel) if !cancel.sent => {
                cancel.sent = true;
                std::mem::take(&mut cancel.escape)
            }
            _ => Vec::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Everything held but the cancelling Escape, in the order it came.
    pub fn into_events(self) -> VecDeque<T> {
        self.events
    }
}

type Held = Box<dyn FnOnce(&mut State)>;

/// Why a hold ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum End {
    /// The overlay has the pointer: the held input is its.
    Overlay,
    /// The overlay an Escape cancelled is gone: the held input is what is under it.
    Cancelled,
    /// No overlay in time.
    Timeout,
}

/// The hold in progress, and the timer that ends it if no overlay comes.
pub struct Active {
    hold: Hold<Held>,
    timer: RegistrationToken,
    since: Instant,
    /// A capture overlay was up after the cancelling Escape.
    overlay_seen: bool,
}

impl std::fmt::Debug for Active {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Active")
            .field("held", &self.hold.len())
            .field("cancelled", &self.hold.cancelled())
            .field("since", &self.since)
            .finish()
    }
}

fn is_capture_layer(layer: &LayerSurface) -> bool {
    is_capture_overlay(layer.layer(), layer.cached_state().keyboard_interactivity)
}

impl State {
    /// Holds presses and keys from now until a capture overlay has the pointer.
    pub fn hold_input_for_capture(&mut self) {
        let Some(timer) = self.capture_hold_timer() else {
            warn!("cannot time a capture input hold, not holding");
            return;
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
            overlay_seen: false,
        });
    }

    fn capture_hold_timer(&mut self) -> Option<RegistrationToken> {
        self.common
            .event_loop_handle
            .insert_source(Timer::from_duration(TIMEOUT), |_, _, state| {
                state.end_capture_hold(End::Timeout);
                TimeoutAction::Drop
            })
            .map_err(|err| warn!(?err, "cannot add a capture hold timer"))
            .ok()
    }

    /// Keeps `event` while input is held; hands it back to be handled now otherwise.
    pub fn hold_capture_input<B>(&mut self, event: InputEvent<B>) -> Option<InputEvent<B>>
    where
        B: InputBackend + 'static,
        <B as InputBackend>::Device: 'static,
    {
        let Some(kind) = self.capture_hold.as_ref().and(kind(&event)) else {
            return Some(event);
        };
        let escape = self.escape_of(&event);
        let active = self.capture_hold.as_mut()?;
        let was_cancelled = active.hold.cancelled();
        let now = active.hold.take(
            kind,
            escape,
            Box::new(move |state: &mut State| state.handle_input_event(event)),
        );
        let cancelled = !was_cancelled && active.hold.cancelled();
        let held = active.hold.len();
        if let Some(now) = now {
            now(self);
            return None;
        }
        if cancelled {
            info!(held, "Escape before the capture overlay: cancelling it");
            if let Some(timer) = self.capture_hold_timer()
                && let Some(active) = self.capture_hold.as_mut()
            {
                let old = std::mem::replace(&mut active.timer, timer);
                self.common.event_loop_handle.remove(old);
            }
            self.end_capture_hold_if_ready(false);
        }
        None
    }

    /// Whether `event` presses or lets go of Escape, by its key in the active layout.
    fn escape_of<B: InputBackend>(&mut self, event: &InputEvent<B>) -> Escape {
        let InputEvent::Keyboard { event } = event else {
            return Escape::No;
        };
        let keycode = event.key_code();
        let keyboard = self.common.shell.read().seats.last_active().get_keyboard();
        let Some(keyboard) = keyboard else {
            return Escape::No;
        };
        let escape = keyboard.with_xkb_state(self, |context| {
            let xkb = context.xkb().lock().unwrap();
            xkb.raw_syms_for_key_in_layout(keycode, xkb.active_layout())
                .contains(&Keysym::Escape)
        });
        match (escape, event.state()) {
            (false, _) => Escape::No,
            (true, KeyState::Pressed) => Escape::Press(keycode.raw()),
            (true, KeyState::Released) => Escape::Release(keycode.raw()),
        }
    }

    /// A layer surface mapped or went, or focus moved: once a capture overlay has the
    /// pointer and the keyboard, it gets the held input, or only the Escape that cancelled
    /// it; once a cancelled overlay is gone, the rest goes to what is under it. `now` moves
    /// the pointer focus without waiting for the next refresh.
    pub fn end_capture_hold_if_ready(&mut self, now: bool) {
        if self.capture_hold.is_none() {
            return;
        }
        let seat = self.common.shell.read().seats.last_active().clone();
        if now {
            crate::shell::focus::update_pointer_focus(self, &seat);
        }
        let output = seat.active_output();
        let pointer = match seat.get_pointer().and_then(|p| p.current_focus()) {
            Some(PointerFocusTarget::WlSurface { surface, .. }) => layer_map_for_output(&output)
                .layer_for_surface(&surface, WindowSurfaceType::ALL)
                .is_some_and(is_capture_layer),
            _ => false,
        };
        let keyboard = seat
            .get_keyboard()
            .and_then(|keyboard| keyboard.current_focus())
            .is_some_and(|focus| {
                matches!(focus, KeyboardFocusTarget::LayerSurface(layer) if is_capture_layer(&layer))
            });
        let up = self
            .common
            .shell
            .read()
            .outputs()
            .any(|output| layer_map_for_output(output).layers().any(is_capture_layer));
        let Some(active) = self.capture_hold.as_mut() else {
            return;
        };
        if !active.hold.cancelled() {
            if pointer {
                self.end_capture_hold(End::Overlay);
            }
            return;
        }
        if !up {
            if active.overlay_seen {
                self.end_capture_hold(End::Cancelled);
            }
            return;
        }
        active.overlay_seen = true;
        // Keys sent before the overlay has drawn and taken the pointer are lost on it.
        if !(pointer && keyboard) {
            return;
        }
        let escape = active.hold.escape_for_overlay();
        if !escape.is_empty() {
            info!("cancelled capture overlay has the pointer: giving it the Escape");
        }
        for event in escape {
            event(self);
        }
    }

    /// Replays the held input in order to whatever has the focus: the overlay once it has
    /// the pointer, else what is under it, so a capture cancelled or never shown loses no
    /// input. A cancelling Escape the overlay never got is dropped.
    fn end_capture_hold(&mut self, end: End) {
        let Some(active) = self.capture_hold.take() else {
            return;
        };
        self.common.event_loop_handle.remove(active.timer);
        let held = active.hold.len();
        let after_ms = active.since.elapsed().as_millis();
        match end {
            End::Overlay => info!(
                held,
                after_ms, "capture overlay has the pointer: replaying held input"
            ),
            End::Cancelled => info!(
                held,
                after_ms, "cancelled capture overlay is gone: replaying held input"
            ),
            End::Timeout => warn!(
                held,
                after_ms,
                cancelled = active.hold.cancelled(),
                "no capture overlay in time: delivering held input"
            ),
        }
        for event in active.hold.into_events() {
            event(self);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(hold: &mut Hold<&'static str>, name: &'static str) -> bool {
        hold.take(Kind::Key, Escape::No, name).is_none()
    }

    const ESC: u32 = 9;

    /// The bug: a click right after Super+Shift+6 pressed the button under the colour picker.
    #[test]
    fn a_click_after_the_shortcut_waits_for_the_overlay() {
        let mut hold = Hold::default();
        assert!(hold.take(Kind::Press, Escape::No, "press").is_none());
        assert!(hold.take(Kind::Press, Escape::No, "release").is_none());
        assert_eq!(Vec::from(hold.into_events()), ["press", "release"]);
    }

    #[test]
    fn keys_wait_but_the_cursor_moves() {
        let mut hold = Hold::default();
        assert!(key(&mut hold, "shift up"));
        assert_eq!(
            hold.take(Kind::Motion, Escape::No, "move"),
            Some("move"),
            "motion goes through"
        );
        assert!(key(&mut hold, "a"));
        assert_eq!(Vec::from(hold.into_events()), ["shift up", "a"]);
    }

    /// A selection dragged before the overlay maps must start where the button went down.
    #[test]
    fn motion_after_a_held_click_stays_behind_it() {
        let mut hold = Hold::default();
        assert!(hold.take(Kind::Motion, Escape::No, "hover").is_some());
        assert!(hold.take(Kind::Press, Escape::No, "press").is_none());
        assert!(hold.take(Kind::Motion, Escape::No, "drag").is_none());
        assert!(hold.take(Kind::Press, Escape::No, "release").is_none());
        assert_eq!(Vec::from(hold.into_events()), ["press", "drag", "release"]);
    }

    /// The bug: Super+Shift+6, Escape, then typing before the picker showed: every key went
    /// to the overlay behind the Escape, and only one typed after it closed reached the app.
    #[test]
    fn escape_before_the_overlay_cancels_it_and_the_typing_goes_under_it() {
        let mut hold = Hold::default();
        assert!(key(&mut hold, "shift up"));
        assert!(
            hold.take(Kind::Key, Escape::Press(ESC), "esc down")
                .is_none()
        );
        assert!(hold.cancelled());
        assert!(
            hold.take(Kind::Key, Escape::Release(ESC), "esc up")
                .is_none()
        );
        for name in ["q down", "q up", "a down", "a up", "d down", "d up"] {
            assert!(
                key(&mut hold, name),
                "{name} held until the overlay is gone"
            );
        }
        assert!(hold.take(Kind::Press, Escape::No, "click").is_none());
        assert_eq!(hold.escape_for_overlay(), ["esc down", "esc up"]);
        assert!(hold.escape_for_overlay().is_empty(), "the Escape goes once");
        assert_eq!(
            Vec::from(hold.into_events()),
            [
                "shift up", "q down", "q up", "a down", "a up", "d down", "d up", "click"
            ],
            "everything but the Escape, in order"
        );
    }

    /// Letting go of Escape after the overlay got the press goes where the press went.
    #[test]
    fn an_escape_let_go_late_follows_its_press() {
        let mut hold = Hold::default();
        assert!(
            hold.take(Kind::Key, Escape::Press(ESC), "esc down")
                .is_none()
        );
        assert!(key(&mut hold, "x down"));
        assert_eq!(hold.escape_for_overlay(), ["esc down"]);
        assert_eq!(
            hold.take(Kind::Key, Escape::Release(ESC), "esc up"),
            Some("esc up")
        );
        assert_eq!(Vec::from(hold.into_events()), ["x down"]);
    }

    /// No overlay in time: every key and click is delivered in order; an Escape that never
    /// reached an overlay is dropped rather than sent to the app.
    #[test]
    fn a_timeout_delivers_all_but_the_cancelling_escape() {
        let mut hold = Hold::default();
        for name in ["a down", "a up"] {
            assert!(key(&mut hold, name));
        }
        assert!(hold.take(Kind::Press, Escape::No, "click").is_none());
        assert_eq!(Vec::from(hold.into_events()), ["a down", "a up", "click"]);

        let mut hold = Hold::default();
        assert!(key(&mut hold, "a down"));
        assert!(
            hold.take(Kind::Key, Escape::Press(ESC), "esc down")
                .is_none()
        );
        assert!(
            hold.take(Kind::Key, Escape::Release(ESC), "esc up")
                .is_none()
        );
        assert!(key(&mut hold, "b down"));
        assert_eq!(Vec::from(hold.into_events()), ["a down", "b down"]);
    }

    /// Typing while the overlay maps, with no Escape, is the overlay's once it has the
    /// pointer: nothing is cancelled and nothing is lost.
    #[test]
    fn typing_without_escape_waits_for_the_overlay() {
        let mut hold = Hold::default();
        for name in ["a down", "a up", "b down", "b up"] {
            assert!(key(&mut hold, name));
        }
        assert!(!hold.cancelled());
        assert!(hold.escape_for_overlay().is_empty());
        assert_eq!(
            Vec::from(hold.into_events()),
            ["a down", "a up", "b down", "b up"]
        );
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
