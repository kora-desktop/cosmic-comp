// SPDX-License-Identifier: GPL-3.0-only

use std::collections::HashSet;

use smithay::{
    backend::input::{KeyState, Keycode},
    input::{
        SeatHandler,
        keyboard::{KeyboardHandle, KeyboardInnerHandle, ModifiersState},
    },
    utils::Serial,
};

/// Keys the window saw go down before a compositor grab took the keyboard.
/// Their releases still reach it: the window repeats a key it thinks is held,
/// so a held shortcut that opened the grab would fire again each time it ends.
#[derive(Debug, Default)]
pub struct HeldByWindow(HashSet<Keycode>);

impl HeldByWindow {
    pub fn new<D: SeatHandler + 'static>(keyboard: Option<&KeyboardHandle<D>>) -> Self {
        Self(
            keyboard
                .map(KeyboardHandle::pressed_keys)
                .unwrap_or_default(),
        )
    }

    /// Hand the window the release of a key it saw pressed; anything else stays with the grab.
    #[allow(clippy::too_many_arguments)]
    pub fn release<D: SeatHandler + 'static>(
        &mut self,
        data: &mut D,
        handle: &mut KeyboardInnerHandle<'_, D>,
        keycode: Keycode,
        state: KeyState,
        modifiers: Option<ModifiersState>,
        serial: Serial,
        time: u32,
    ) {
        if state == KeyState::Released && self.0.remove(&keycode) {
            handle.input(data, keycode, state, modifiers, serial, time);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use smithay::{
        input::{
            Seat, SeatState,
            keyboard::{
                FilterResult, GrabStartData, KeyboardGrab, KeyboardTarget, KeysymHandle, XkbConfig,
            },
        },
        reexports::wayland_server::{Client, protocol::wl_surface::WlSurface},
        utils::{IsAlive, SERIAL_COUNTER},
        wayland::compositor::{CompositorClientState, CompositorHandler, CompositorState},
    };

    use super::*;

    /// What the window was told: each key with its state, and whether Ctrl was held.
    #[derive(Debug, Clone, Default)]
    struct Window(Arc<Mutex<Vec<(u32, KeyState)>>>, Arc<Mutex<bool>>);

    impl PartialEq for Window {
        fn eq(&self, other: &Self) -> bool {
            Arc::ptr_eq(&self.0, &other.0)
        }
    }

    impl IsAlive for Window {
        fn alive(&self) -> bool {
            true
        }
    }

    impl KeyboardTarget<Compositor> for Window {
        fn enter(
            &self,
            _: &Seat<Compositor>,
            _: &mut Compositor,
            _: Vec<KeysymHandle<'_>>,
            _: Serial,
        ) {
        }
        fn leave(&self, _: &Seat<Compositor>, _: &mut Compositor, _: Serial) {}
        fn key(
            &self,
            _: &Seat<Compositor>,
            _: &mut Compositor,
            key: KeysymHandle<'_>,
            state: KeyState,
            _: Serial,
            _: u32,
        ) {
            self.0.lock().unwrap().push((key.raw_code().raw(), state));
        }
        fn modifiers(
            &self,
            _: &Seat<Compositor>,
            _: &mut Compositor,
            modifiers: ModifiersState,
            _: Serial,
        ) {
            *self.1.lock().unwrap() = modifiers.ctrl;
        }
    }

    struct Compositor {
        seats: SeatState<Self>,
    }

    impl SeatHandler for Compositor {
        type KeyboardFocus = Window;
        type PointerFocus = WlSurface;
        type TouchFocus = WlSurface;

        fn seat_state(&mut self) -> &mut SeatState<Self> {
            &mut self.seats
        }
    }

    // A surface's touch focus needs one; these tests never have a client or a surface.
    impl CompositorHandler for Compositor {
        fn compositor_state(&mut self) -> &mut CompositorState {
            unreachable!("no surfaces in these tests")
        }
        fn client_compositor_state<'a>(&self, _: &'a Client) -> &'a CompositorClientState {
            unreachable!("no clients in these tests")
        }
        fn commit(&mut self, _: &WlSurface) {}
    }

    /// A grab that keeps every key from the window, as the palette's does.
    struct Palette {
        held: HeldByWindow,
        start: GrabStartData<Compositor>,
    }

    impl KeyboardGrab<Compositor> for Palette {
        fn input(
            &mut self,
            data: &mut Compositor,
            handle: &mut KeyboardInnerHandle<'_, Compositor>,
            keycode: Keycode,
            state: KeyState,
            modifiers: Option<ModifiersState>,
            serial: Serial,
            time: u32,
        ) {
            self.held
                .release(data, handle, keycode, state, modifiers, serial, time);
        }
        fn set_focus(
            &mut self,
            _: &mut Compositor,
            _: &mut KeyboardInnerHandle<'_, Compositor>,
            _: Option<Window>,
            _: Serial,
        ) {
        }
        fn start_data(&self) -> &GrabStartData<Compositor> {
            &self.start
        }
        fn unset(&mut self, _: &mut Compositor) {}
    }

    // evdev codes plus xkb's offset of 8.
    const CTRL: u32 = 29 + 8;
    const K: u32 = 37 + 8;
    const A: u32 = 30 + 8;

    fn key(
        compositor: &mut Compositor,
        keyboard: &KeyboardHandle<Compositor>,
        code: u32,
        state: KeyState,
    ) {
        keyboard.input::<(), _>(
            compositor,
            code.into(),
            state,
            SERIAL_COUNTER.next_serial(),
            0,
            |_, _, _| FilterResult::Forward,
        );
    }

    /// An app that opened the palette on Ctrl+K must still see Ctrl+K come up,
    /// or its key repeat reopens the palette each time it closes.
    #[test]
    fn the_window_sees_the_keys_that_opened_the_palette_come_up() {
        let mut seats = SeatState::new();
        let mut seat = seats.new_seat("test");
        let mut compositor = Compositor { seats };
        let keyboard = seat
            .add_keyboard(XkbConfig::default(), 600, 25)
            .expect("a keymap");
        let window = Window::default();
        keyboard.set_focus(
            &mut compositor,
            Some(window.clone()),
            SERIAL_COUNTER.next_serial(),
        );

        key(&mut compositor, &keyboard, CTRL, KeyState::Pressed);
        key(&mut compositor, &keyboard, K, KeyState::Pressed);
        assert!(*window.1.lock().unwrap(), "the window saw Ctrl go down");
        let start = GrabStartData {
            focus: keyboard.current_focus(),
        };
        let held = HeldByWindow::new(Some(&keyboard));
        keyboard.set_grab(
            &mut compositor,
            Palette { held, start },
            SERIAL_COUNTER.next_serial(),
        );
        window.0.lock().unwrap().clear();

        // Typed into the palette: the window sees neither half.
        key(&mut compositor, &keyboard, A, KeyState::Pressed);
        key(&mut compositor, &keyboard, A, KeyState::Released);
        key(&mut compositor, &keyboard, K, KeyState::Released);
        key(&mut compositor, &keyboard, CTRL, KeyState::Released);

        assert_eq!(
            *window.0.lock().unwrap(),
            [(K, KeyState::Released), (CTRL, KeyState::Released)]
        );
        assert!(!*window.1.lock().unwrap(), "the window saw Ctrl come up");
    }
}
