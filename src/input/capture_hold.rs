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

/// How long a cancelled capture's overlay may still come and take its Escape.
const OWED: Duration = Duration::from_secs(8);

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

/// Which key a held key event moves, which way, and whether it is Escape in the active layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    No,
    Down { code: u32, escape: bool },
    Up { code: u32, escape: bool },
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
    /// Keys pressed since the hold began.
    pressed: Vec<u32>,
}

impl<T> Default for Hold<T> {
    fn default() -> Self {
        Self {
            events: VecDeque::new(),
            pointer_held: false,
            cancel: None,
            pressed: Vec::new(),
        }
    }
}

impl<T> Hold<T> {
    /// Keeps `event` while input is held; hands it back to be handled now otherwise.
    pub fn take(&mut self, kind: Kind, key: Key, event: T) -> Option<T> {
        // Motion after a held click would replay the click where the pointer went next.
        if kind == Kind::Motion && !self.pointer_held {
            return Some(event);
        }
        match (key, self.cancel.as_mut()) {
            (Key::Down { code, escape: true }, None) => {
                self.cancel = Some(Cancel {
                    key: code,
                    escape: vec![event],
                    sent: false,
                });
                return None;
            }
            (Key::Up { code, .. }, Some(cancel)) if cancel.key == code => {
                // The press is the overlay's already: the release follows it there now.
                if cancel.sent {
                    return Some(event);
                }
                cancel.escape.push(event);
                return None;
            }
            // A key down before the hold, such as the shortcut's Super and Shift: held, its
            // release would turn a cancelling Escape into Super+Shift+Escape, a shortcut.
            (Key::Up { code, .. }, _) if !self.pressed.contains(&code) => return Some(event),
            (Key::Down { code, .. }, _) if !self.pressed.contains(&code) => {
                self.pressed.push(code);
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

    /// Everything held so far, leaving the cancelling Escape owed to the overlay.
    pub fn take_events(&mut self) -> VecDeque<T> {
        std::mem::take(&mut self.events)
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
    /// Cancelled with no overlay in time: input reaches the app until the overlay comes.
    passing: bool,
    /// The timeout was already extended for a cancelled capture.
    extended: bool,
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
            passing: false,
            extended: false,
        });
    }

    fn capture_hold_timer(&mut self) -> Option<RegistrationToken> {
        self.capture_hold_timer_for(TIMEOUT)
    }

    fn capture_hold_timer_for(&mut self, after: Duration) -> Option<RegistrationToken> {
        self.common
            .event_loop_handle
            .insert_source(Timer::from_duration(after), |_, _, state| {
                state.capture_hold_timed_out();
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
        if self
            .capture_hold
            .as_ref()
            .is_some_and(|active| active.passing)
        {
            return Some(event);
        }
        let key = self.key_of(&event);
        let active = self.capture_hold.as_mut()?;
        let was_cancelled = active.hold.cancelled();
        let now = active.hold.take(
            kind,
            key,
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

    /// The key `event` presses or lets go of, and whether it is Escape in the active layout.
    fn key_of<B: InputBackend>(&mut self, event: &InputEvent<B>) -> Key {
        let InputEvent::Keyboard { event } = event else {
            return Key::No;
        };
        let keycode = event.key_code();
        let keyboard = self.common.shell.read().seats.last_active().get_keyboard();
        let escape = keyboard.is_some_and(|keyboard| {
            keyboard.with_xkb_state(self, |context| {
                let xkb = context.xkb().lock().unwrap();
                xkb.raw_syms_for_key_in_layout(keycode, xkb.active_layout())
                    .contains(&Keysym::Escape)
            })
        });
        let code = keycode.raw();
        match event.state() {
            KeyState::Pressed => Key::Down { code, escape },
            KeyState::Released => Key::Up { code, escape },
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
        let up = self.capture_overlay_up();
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
        // The late overlay takes the keyboard: hold again until it is gone.
        active.passing = false;
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

    fn capture_overlay_up(&self) -> bool {
        self.common
            .shell
            .read()
            .outputs()
            .any(|output| layer_map_for_output(output).layers().any(is_capture_layer))
    }

    /// A cancelled capture whose overlay is late still owes it the Escape, once: the held
    /// input goes to the app now unless the overlay is up, where it would be lost.
    fn capture_hold_timed_out(&mut self) {
        let up = self.capture_overlay_up();
        let owed = self
            .capture_hold
            .as_ref()
            .is_some_and(|active| active.hold.cancelled() && !active.extended);
        if !owed {
            self.end_capture_hold(End::Timeout);
            return;
        }
        let Some(timer) = self.capture_hold_timer_for(OWED) else {
            self.end_capture_hold(End::Timeout);
            return;
        };
        let Some(active) = self.capture_hold.as_mut() else {
            return;
        };
        active.timer = timer;
        active.extended = true;
        if up {
            info!("cancelled capture overlay still up: waiting for it to go");
            return;
        }
        active.passing = true;
        let held = active.hold.take_events();
        info!(
            held = held.len(),
            "no capture overlay yet: delivering held input, Escape kept for it"
        );
        for event in held {
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

    const ESC: u32 = 9;
    const SHIFT: u32 = 50;
    const SUPER: u32 = 133;

    fn down(code: u32) -> Key {
        Key::Down {
            code,
            escape: code == ESC,
        }
    }

    fn up(code: u32) -> Key {
        Key::Up {
            code,
            escape: code == ESC,
        }
    }

    /// Holds a key typed after the shortcut: `code` stands for the key, `name` for its event.
    fn typed(hold: &mut Hold<&'static str>, code: u32, name: &'static str) -> bool {
        let key = if name.ends_with("up") {
            up(code)
        } else {
            down(code)
        };
        hold.take(Kind::Key, key, name).is_none()
    }

    /// The bug: a click right after Super+Shift+6 pressed the button under the colour picker.
    #[test]
    fn a_click_after_the_shortcut_waits_for_the_overlay() {
        let mut hold = Hold::default();
        assert!(hold.take(Kind::Press, Key::No, "press").is_none());
        assert!(hold.take(Kind::Press, Key::No, "release").is_none());
        assert_eq!(Vec::from(hold.into_events()), ["press", "release"]);
    }

    #[test]
    fn keys_wait_but_the_cursor_moves() {
        let mut hold = Hold::default();
        assert!(typed(&mut hold, 38, "a down"));
        assert_eq!(
            hold.take(Kind::Motion, Key::No, "move"),
            Some("move"),
            "motion goes through"
        );
        assert!(typed(&mut hold, 38, "a up"));
        assert_eq!(Vec::from(hold.into_events()), ["a down", "a up"]);
    }

    /// A selection dragged before the overlay maps must start where the button went down.
    #[test]
    fn motion_after_a_held_click_stays_behind_it() {
        let mut hold = Hold::default();
        assert!(hold.take(Kind::Motion, Key::No, "hover").is_some());
        assert!(hold.take(Kind::Press, Key::No, "press").is_none());
        assert!(hold.take(Kind::Motion, Key::No, "drag").is_none());
        assert!(hold.take(Kind::Press, Key::No, "release").is_none());
        assert_eq!(Vec::from(hold.into_events()), ["press", "drag", "release"]);
    }

    /// The bug: Super+Shift+6, Escape, then typing before the picker showed: every key went
    /// to the overlay behind the Escape, and only one typed after it closed reached the app.
    #[test]
    fn escape_before_the_overlay_cancels_it_and_the_typing_goes_under_it() {
        let mut hold = Hold::default();
        assert!(hold.take(Kind::Key, down(ESC), "esc down").is_none());
        assert!(hold.cancelled());
        assert!(hold.take(Kind::Key, up(ESC), "esc up").is_none());
        for (code, name) in [
            (24, "q down"),
            (24, "q up"),
            (38, "a down"),
            (38, "a up"),
            (40, "d down"),
            (40, "d up"),
        ] {
            assert!(
                typed(&mut hold, code, name),
                "{name} held until the overlay is gone"
            );
        }
        assert!(hold.take(Kind::Press, Key::No, "click").is_none());
        assert_eq!(hold.escape_for_overlay(), ["esc down", "esc up"]);
        assert!(hold.escape_for_overlay().is_empty(), "the Escape goes once");
        assert_eq!(
            Vec::from(hold.into_events()),
            [
                "q down", "q up", "a down", "a up", "d down", "d up", "click"
            ],
            "everything but the Escape, in order"
        );
    }

    /// The bug: Super+Shift+6 let go, then Escape: the held Super and Shift releases came after
    /// the Escape, which became Super+Shift+Escape, so the picker stayed up and took the typing.
    #[test]
    fn the_shortcut_keys_let_go_before_the_escape_go_through() {
        let mut hold = Hold::default();
        assert_eq!(
            hold.take(Kind::Key, up(SHIFT), "shift up"),
            Some("shift up")
        );
        assert_eq!(
            hold.take(Kind::Key, up(SUPER), "super up"),
            Some("super up")
        );
        assert!(hold.take(Kind::Key, down(ESC), "esc down").is_none());
        assert!(hold.take(Kind::Key, up(ESC), "esc up").is_none());
        assert!(typed(&mut hold, 24, "q down"));
        assert!(typed(&mut hold, 24, "q up"));
        assert_eq!(hold.escape_for_overlay(), ["esc down", "esc up"]);
        assert_eq!(Vec::from(hold.into_events()), ["q down", "q up"]);
    }

    /// A key pressed after the shortcut keeps its release behind its press.
    #[test]
    fn a_key_pressed_while_held_lets_go_in_order() {
        let mut hold = Hold::default();
        assert!(typed(&mut hold, SHIFT, "shift down"));
        assert!(typed(&mut hold, 38, "a down"));
        assert!(typed(&mut hold, SHIFT, "shift up"));
        assert_eq!(
            Vec::from(hold.into_events()),
            ["shift down", "a down", "shift up"]
        );
    }

    /// Letting go of Escape after the overlay got the press goes where the press went.
    #[test]
    fn an_escape_let_go_late_follows_its_press() {
        let mut hold = Hold::default();
        assert!(hold.take(Kind::Key, down(ESC), "esc down").is_none());
        assert!(typed(&mut hold, 53, "x down"));
        assert_eq!(hold.escape_for_overlay(), ["esc down"]);
        assert_eq!(hold.take(Kind::Key, up(ESC), "esc up"), Some("esc up"));
        assert_eq!(Vec::from(hold.into_events()), ["x down"]);
    }

    /// No overlay in time: every key and click is delivered in order; an Escape that never
    /// reached an overlay is dropped rather than sent to the app.
    #[test]
    fn a_timeout_delivers_all_but_the_cancelling_escape() {
        let mut hold = Hold::default();
        assert!(typed(&mut hold, 38, "a down"));
        assert!(typed(&mut hold, 38, "a up"));
        assert!(hold.take(Kind::Press, Key::No, "click").is_none());
        assert_eq!(Vec::from(hold.into_events()), ["a down", "a up", "click"]);

        let mut hold = Hold::default();
        assert!(typed(&mut hold, 38, "a down"));
        assert!(hold.take(Kind::Key, down(ESC), "esc down").is_none());
        assert!(hold.take(Kind::Key, up(ESC), "esc up").is_none());
        assert!(typed(&mut hold, 56, "b down"));
        assert_eq!(Vec::from(hold.into_events()), ["a down", "b down"]);
    }

    /// An overlay later than the timeout: the typing goes to the app at once, and the Escape
    /// waits for the overlay so it closes when it comes.
    #[test]
    fn a_late_overlay_still_gets_its_escape() {
        let mut hold = Hold::default();
        assert!(hold.take(Kind::Key, down(ESC), "esc down").is_none());
        assert!(hold.take(Kind::Key, up(ESC), "esc up").is_none());
        assert!(typed(&mut hold, 24, "q down"));
        assert_eq!(Vec::from(hold.take_events()), ["q down"]);
        assert_eq!(hold.escape_for_overlay(), ["esc down", "esc up"]);
        assert!(hold.into_events().is_empty());
    }

    /// Typing while the overlay maps, with no Escape, is the overlay's once it has the
    /// pointer: nothing is cancelled and nothing is lost.
    #[test]
    fn typing_without_escape_waits_for_the_overlay() {
        let mut hold = Hold::default();
        for (code, name) in [(38, "a down"), (38, "a up"), (56, "b down"), (56, "b up")] {
            assert!(typed(&mut hold, code, name));
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
