use super::*;
use smithay::{
    output::{Mode, PhysicalProperties, Subpixel},
    reexports::wayland_server::Display,
};

fn output(name: &str) -> Output {
    let output = Output::new(
        name.into(),
        PhysicalProperties {
            size: (300, 200).into(),
            subpixel: Subpixel::Unknown,
            make: "test".into(),
            model: "test".into(),
            serial_number: String::new(),
        },
    );
    output.change_current_state(
        Some(Mode {
            size: (1280, 800).into(),
            refresh: 60_000,
        }),
        None,
        None,
        None,
    );
    output
}

/// A built-in and an external output, `count` empty desktops each; the last
/// one is the spare a dynamic set keeps.
fn two_outputs(
    count: usize,
    test: impl FnOnce(&mut IndexMap<Output, WorkspaceSet>, &mut WorkspaceState<State>),
) {
    let display = Display::<State>::new().unwrap();
    let mut state = WorkspaceState::<State>::new(&display.handle(), |_| true);
    let mut sets = IndexMap::new();
    for name in ["eDP-test", "HDMI-test"] {
        let output = output(name);
        let mut set = WorkspaceSet::new(
            &mut state.update(),
            &output,
            false,
            &crate::comp_theme::CompTheme::default(),
            AppearanceConfig::default(),
        );
        for _ in 0..count {
            set.add_empty_workspace(&mut state.update());
        }
        sets.insert(output, set);
    }
    test(&mut sets, &mut state);
}

fn handles(set: &WorkspaceSet) -> Vec<WorkspaceHandle> {
    set.workspaces.iter().map(|w| w.handle).collect()
}

/// Remove the desktop at `idx` of the built-in output, which shows it.
fn remove_shown(
    sets: &mut IndexMap<Output, WorkspaceSet>,
    state: &mut WorkspaceState<State>,
    idx: usize,
) -> Vec<WorkspaceHandle> {
    sets[0].active = idx;
    let before = handles(&sets[0]);
    assert_eq!(
        remove_desktop_in(sets, true, &before[idx], &mut state.update(), &[]),
        Some(true)
    );
    before
}

#[test]
fn removing_a_middle_desktop_lands_on_the_next() {
    two_outputs(4, |sets, state| {
        let before = remove_shown(sets, state, 1);
        assert_eq!(sets[0].workspaces[sets[0].active].handle, before[2]);
        assert!(sets[0].previously_active.is_none(), "nothing to slide from");
    });
}

#[test]
fn removing_the_last_desktop_before_the_spare_lands_on_the_previous() {
    two_outputs(3, |sets, state| {
        let before = remove_shown(sets, state, 1);
        assert_eq!(sets[0].workspaces[sets[0].active].handle, before[0]);
    });
}

#[test]
fn removing_the_first_desktop_lands_on_the_next() {
    two_outputs(3, |sets, state| {
        let before = remove_shown(sets, state, 0);
        assert_eq!(sets[0].workspaces[sets[0].active].handle, before[1]);
    });
}

#[test]
fn removing_the_only_desktop_lands_on_the_spare() {
    two_outputs(2, |sets, state| {
        let before = remove_shown(sets, state, 0);
        assert_eq!(handles(&sets[0]), vec![before[1]]);
        assert_eq!(sets[0].active, 0);
    });
}

#[test]
fn the_last_desktop_of_a_set_stays() {
    two_outputs(1, |sets, state| {
        let only = sets[0].workspaces[0].handle;
        assert_eq!(
            remove_desktop_in(sets, true, &only, &mut state.update(), &[]),
            Some(false)
        );
        assert_eq!(handles(&sets[0]), vec![only]);
    });
}

#[test]
fn the_other_output_keeps_its_desktop() {
    two_outputs(4, |sets, state| {
        sets[1].active = 2;
        let external = handles(&sets[1]);
        remove_shown(sets, state, 1);
        assert_eq!(handles(&sets[1]), external);
        assert_eq!(sets[1].active, 2);
    });
}

#[test]
fn removing_a_desktop_not_on_screen_keeps_the_one_that_is() {
    two_outputs(4, |sets, state| {
        sets[0].active = 2;
        let before = handles(&sets[0]);
        remove_desktop_in(sets, true, &before[0], &mut state.update(), &[]);
        assert_eq!(sets[0].workspaces[sets[0].active].handle, before[2]);
    });
}

#[test]
fn a_games_desktop_with_only_the_spare_after_it_lands_on_its_home() {
    two_outputs(2, |sets, state| {
        let built_in = sets.get_index(0).unwrap().0.clone();
        let game = insert_desktop(
            sets,
            WorkspaceMode::OutputBound,
            &built_in,
            1,
            "Rooftop fight",
            &mut state.update(),
        )
        .unwrap();
        let home = sets[0].workspaces[0].handle;
        sets[0].active = 1;
        assert_eq!(
            remove_desktop_in(sets, true, &game, &mut state.update(), &[]),
            Some(true)
        );
        assert_eq!(sets[0].workspaces[sets[0].active].handle, home);
    });
}

#[test]
fn landing_order_is_next_then_previous_then_the_spare() {
    // Five left after removing index 2: the next is now at 2.
    assert_eq!(desktop_landing(5, 2, true), 2);
    // The next is the spare: the one before.
    assert_eq!(desktop_landing(3, 2, true), 1);
    // Without dynamic desktops an empty last desktop is a desktop like any other.
    assert_eq!(desktop_landing(3, 2, false), 2);
    // The removed one was last of all.
    assert_eq!(desktop_landing(3, 3, false), 2);
    // Only the spare is left.
    assert_eq!(desktop_landing(1, 0, true), 0);
}
