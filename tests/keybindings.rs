// SPDX-License-Identifier: GPL-3.0-only

//! `data/keybindings.ron` ships as the shortcut `defaults` and gets no
//! compile-time checking. Worse, an action the config crate cannot parse is
//! silently swapped for `Disable`, so a stale variant name reads at runtime as
//! "that key does nothing". Parse the shipped file here instead.

use cosmic_settings_config::shortcuts::{
    Action, Binding, Shortcuts,
    action::{AppAction, Direction, FocusDirection, System},
};
use serde::de::{Deserializer, MapAccess, Visitor};

fn defaults() -> Shortcuts {
    ron::from_str(include_str!("../data/keybindings.ron"))
        .expect("data/keybindings.ron is not valid RON")
}

fn action_for(shortcuts: &Shortcuts, binding: &str) -> Action {
    let key = Binding::from_str_partial(binding).expect("invalid binding in test");
    shortcuts
        .0
        .get(&key)
        .unwrap_or_else(|| panic!("{binding} is unbound"))
        .clone()
}

#[test]
fn every_action_deserializes() {
    let shortcuts = defaults();

    assert!(!shortcuts.0.is_empty());
    assert!(
        !shortcuts
            .0
            .values()
            .any(|action| *action == Action::Disable),
        "an action failed to deserialize and was replaced with Disable"
    );
}

/// The laptop function row emits the Windows chords rather than dedicated
/// keysyms, so F7/F9/F10 are reachable only through these bindings.
#[test]
fn laptop_function_row() {
    let shortcuts = defaults();

    assert_eq!(
        action_for(&shortcuts, "Super+p"),
        Action::System(System::DisplayToggle)
    );
    assert_eq!(
        action_for(&shortcuts, "Super+i"),
        Action::System(System::Settings)
    );
    assert_eq!(
        action_for(&shortcuts, "Super+l"),
        Action::System(System::LockScreen)
    );
    assert_eq!(
        action_for(&shortcuts, "XF86Calculator"),
        Action::System(System::Calculator)
    );
}

/// Super alone opens the app library; the start menu keeps its X under Alt,
/// since Super+X is Cut.
#[test]
fn super_opens_the_app_library() {
    let shortcuts = defaults();

    assert_eq!(
        action_for(&shortcuts, "Super"),
        Action::System(System::AppLibrary)
    );
    assert_eq!(
        action_for(&shortcuts, "Super+Alt+x"),
        Action::System(System::StartMenu)
    );
    assert_eq!(action_for(&shortcuts, "Super+Shift+x"), Action::SwapWindow);
}

#[test]
fn input_source_switch_uses_space_without_replacing_spotlight() {
    let shortcuts = defaults();
    assert_eq!(
        action_for(&shortcuts, "Super+Shift+space"),
        Action::System(System::InputSourceSwitch)
    );
    assert_eq!(
        action_for(&shortcuts, "Super+space"),
        Action::System(System::Spotlight)
    );

    let context = xkbcommon::xkb::Context::new(xkbcommon::xkb::CONTEXT_NO_FLAGS);
    let keymap = xkbcommon::xkb::Keymap::new_from_names(
        &context,
        "",
        "",
        "us,jp",
        ",",
        None,
        xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
    )
    .expect("US/Japanese keymap must compile");
    let space = keymap.key_by_name("SPCE").expect("space key exists");
    let binding = Binding::from_str_partial("Super+Shift+space").unwrap();
    for layout in 0..keymap.num_layouts() {
        assert!(
            keymap
                .key_get_syms_by_level(space, layout, 0)
                .contains(&binding.key.unwrap()),
            "input-source shortcut must match the same key in layout {layout}"
        );
    }
}

/// Taking Super+i/Super+l for the function row must not cost a focus direction.
/// Which chord reaches a direction is free to move -- Super+arrows walk desktops
/// and realms now, so the tiling focus moves live under Super+Ctrl -- but every
/// direction has to stay reachable by something.
#[test]
fn every_focus_direction_is_reachable() {
    let shortcuts = defaults();

    for direction in [
        FocusDirection::Left,
        FocusDirection::Right,
        FocusDirection::Up,
        FocusDirection::Down,
        FocusDirection::In,
        FocusDirection::Out,
    ] {
        assert!(
            shortcuts
                .0
                .values()
                .any(|action| *action == Action::Focus(direction)),
            "no binding reaches Focus({direction:?})"
        );
    }

    // Super+l is the lock screen, Super+k opens Chord and Super+h minimizes, so
    // Right, Up and Left are reached by their arrows; the rest of the vim keys still work.
    assert_eq!(
        action_for(&shortcuts, "Super+Ctrl+Right"),
        Action::Focus(FocusDirection::Right)
    );
    assert_eq!(
        action_for(&shortcuts, "Super+Ctrl+Up"),
        Action::Focus(FocusDirection::Up)
    );
    assert_eq!(
        action_for(&shortcuts, "Super+Ctrl+Left"),
        Action::Focus(FocusDirection::Left)
    );
    for (binding, direction) in [
        ("Super+j", FocusDirection::Down),
        ("Super+u", FocusDirection::Out),
        ("Super+Shift+u", FocusDirection::In),
    ] {
        assert_eq!(action_for(&shortcuts, binding), Action::Focus(direction));
    }
}

/// Every row of the design's shortcut list that the compositor binds
/// (Settings > Shortcuts in the design). Super and Super+Space are the Kora
/// menu's and are checked above; the editing rows come with the Super remap.
#[test]
fn the_design_contract_resolves() {
    let shortcuts = defaults();

    for (binding, action) in [
        // System
        ("Ctrl+space", Action::SpeechToText),
        ("Super+k", Action::System(System::ChatPanel)),
        ("Super+e", Action::System(System::HomeFolder)),
        ("Print", Action::CaptureToolbar),
        ("Super+Shift+s", Action::CaptureToolbar),
        ("Super+Escape", Action::System(System::LockScreen)),
        // Windows
        ("Super+Tab", Action::System(System::WindowSwitcher)),
        ("Alt+Tab", Action::System(System::WindowSwitcherApp)),
        ("Super+n", Action::App(AppAction::NewWindow)),
        ("Super+t", Action::App(AppAction::NewTab)),
        ("Super+q", Action::Close),
        ("Super+Up", Action::Maximize),
        ("Super+h", Action::Minimize),
        ("Super+Return", Action::Fullscreen),
        // Navigation: workspaces vertical, desktops horizontal, Shift takes the window.
        ("Ctrl+Down", Action::NextRealm),
        ("Ctrl+Up", Action::PreviousRealm),
        ("Super+w", Action::System(System::WorkspaceOverview)),
        ("Ctrl+Right", Action::NextWorkspace),
        ("Ctrl+Left", Action::PreviousWorkspace),
        ("Ctrl+Shift+Right", Action::MoveToNextWorkspace),
        ("Ctrl+Shift+Left", Action::MoveToPreviousWorkspace),
        // Tiling halves (the prototype's Super+Shift+arrows).
        ("Super+Shift+Left", Action::Move(Direction::Left)),
        ("Super+Shift+Right", Action::Move(Direction::Right)),
    ] {
        assert_eq!(action_for(&shortcuts, binding), action, "{binding}");
    }
}

/// What the contract took a chord from still has one, and the old chords that
/// collided with it are gone.
#[test]
fn displaced_actions_stay_reachable() {
    let shortcuts = defaults();

    for (binding, action) in [
        ("Super+Shift+k", Action::WindowCommands),
        ("Super+Shift+Up", Action::Move(Direction::Up)),
        ("Super+Ctrl+Left", Action::Focus(FocusDirection::Left)),
        ("Super+Ctrl+k", Action::PreviousRealm),
        ("Super+Ctrl+j", Action::NextRealm),
        (
            "Super+Shift+Tab",
            Action::System(System::WindowSwitcherPrevious),
        ),
        (
            "Alt+Shift+Tab",
            Action::System(System::WindowSwitcherAppPrevious),
        ),
        ("Ctrl+Alt+t", Action::System(System::Terminal)),
        ("Super+Alt+x", Action::System(System::StartMenu)),
        // Old chords that did not collide stay.
        ("Super+m", Action::Maximize),
        ("Super+F11", Action::Fullscreen),
        ("Super+f", Action::System(System::HomeFolder)),
        ("Super+Left", Action::PreviousWorkspace),
        ("Super+Right", Action::NextWorkspace),
        ("Alt+F4", Action::Close),
    ] {
        assert_eq!(action_for(&shortcuts, binding), action, "{binding}");
    }

    // Super is the command modifier now: nothing of the compositor's own may
    // sit on the editing chords, or apps would never see them.
    for binding in [
        "Super+x",
        "Super+c",
        "Super+v",
        "Super+z",
        "Super+Shift+z",
        "Super+a",
    ] {
        let key = Binding::from_str_partial(binding).unwrap();
        assert!(
            shortcuts
                .0
                .get(&key)
                .is_none_or(|action| matches!(action, Action::App(_))),
            "{binding} is taken from the apps"
        );
    }
    assert!(
        !shortcuts
            .0
            .contains_key(&Binding::from_str_partial("Super+Down").unwrap()),
        "Super+Down is half of an axis whose other half is Maximize"
    );
}

/// RON maps keep the last of two equal keys without a word, so a chord bound
/// twice silently loses one of its actions. Read the file as a list instead.
#[test]
fn no_chord_is_bound_twice() {
    struct Entries;
    impl<'de> Visitor<'de> for Entries {
        type Value = Vec<Binding>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a map of bindings")
        }
        fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
            let mut keys = Vec::new();
            while let Some((key, _)) = map.next_entry::<Binding, ron::Value>()? {
                keys.push(key);
            }
            Ok(keys)
        }
    }

    let text = include_str!("../data/keybindings.ron");
    let keys = ron::Deserializer::from_str(text)
        .unwrap()
        .deserialize_map(Entries)
        .unwrap();
    let mut seen = std::collections::HashSet::new();
    for key in &keys {
        assert!(seen.insert(key), "{key:?} is bound twice");
    }
}
