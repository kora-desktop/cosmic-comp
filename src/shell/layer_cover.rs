// SPDX-License-Identifier: GPL-3.0-only

//! Shell surfaces another one stands over, left out of the picture and of hit testing.
//!
//! The desktop overview stands in for the desktop, so what it covers goes as the windows
//! do. A capture indicator's preview sits in the corner a card may also use: the card
//! waits under it rather than showing through.

use std::collections::HashSet;

use smithay::{
    backend::renderer::utils::with_renderer_surface_state,
    desktop::layer_map_for_output,
    output::Output,
    reexports::wayland_server::{Resource, backend::ObjectId},
    utils::{Logical, Rectangle},
    wayland::{
        compositor::{RectangleKind, SurfaceAttributes, with_states},
        shell::wlr_layer::{KeyboardInteractivity, Layer},
    },
};

use crate::utils::quirks::WORKSPACE_OVERVIEW_NAMESPACE;

/// What decides whether a layer surface covers or is covered.
#[derive(Debug, Clone)]
pub struct Shown<'a> {
    pub layer: Layer,
    pub namespace: &'a str,
    pub keyboard: KeyboardInteractivity,
    /// It has a buffer: a surface that has drawn nothing covers nothing.
    pub drawn: bool,
    pub geometry: Rectangle<i32, Logical>,
    /// Where it takes input, in the same space as `geometry`; `None` for all of it.
    pub input: Option<Rectangle<i32, Logical>>,
}

/// The OSD's own namespace, or `osd-<what>` for another key's answer, as a capture's preview.
pub fn indicator_namespace(namespace: &str) -> bool {
    namespace == "osd" || namespace.starts_with("osd-")
}

fn rank(layer: Layer) -> u8 {
    match layer {
        Layer::Background => 0,
        Layer::Bottom => 1,
        Layer::Top => 2,
        Layer::Overlay => 3,
    }
}

/// Indexes into `layers`, bottom to top as the layer map keeps them, of the covered ones.
pub fn covered(layers: &[Shown<'_>]) -> HashSet<usize> {
    let mut out = HashSet::new();
    for (i, over) in layers.iter().enumerate() {
        if !over.drawn {
            continue;
        }
        if over.namespace == WORKSPACE_OVERVIEW_NAMESPACE {
            // The wallpaper stays: the overview's grid is drawn on it.
            out.extend(layers.iter().enumerate().filter_map(|(j, under)| {
                let below =
                    rank(under.layer) < rank(over.layer) || (under.layer == over.layer && j < i);
                (under.layer != Layer::Background && below).then_some(j)
            }));
        } else if over.layer == Layer::Overlay
            && indicator_namespace(over.namespace)
            && over.keyboard == KeyboardInteractivity::None
        {
            // Where it draws is where it takes input; the rest of its surface is see-through.
            let area = over.input.unwrap_or(over.geometry);
            out.extend(layers.iter().enumerate().filter_map(|(j, under)| {
                (under.layer == Layer::Top && under.geometry.overlaps(area)).then_some(j)
            }));
        }
    }
    out
}

/// The bounding box of a surface's input region, clipped to its `size`; `None` when it set none.
fn input_bounds(
    surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    geometry: Rectangle<i32, Logical>,
) -> Option<Rectangle<i32, Logical>> {
    let region = with_states(surface, |states| {
        states
            .cached_state
            .get::<SurfaceAttributes>()
            .current()
            .input_region
            .clone()
    })?;
    let whole = Rectangle::from_size(geometry.size);
    let bounds = region
        .rects
        .iter()
        .filter(|(kind, _)| matches!(kind, RectangleKind::Add))
        .filter_map(|(_, rect)| rect.intersection(whole))
        .reduce(|a, b| a.merge(b))
        // An empty region takes no input and so draws nowhere that counts.
        .unwrap_or_default();
    Some(Rectangle::new(geometry.loc + bounds.loc, bounds.size))
}

/// The layer surfaces on `output` another one covers.
pub fn covered_on(output: &Output) -> HashSet<ObjectId> {
    let map = layer_map_for_output(output);
    let surfaces = map.layers().cloned().collect::<Vec<_>>();
    let namespaces = surfaces
        .iter()
        .map(|s| s.namespace().to_owned())
        .collect::<Vec<_>>();
    let shown = surfaces
        .iter()
        .zip(&namespaces)
        .map(|(s, namespace)| {
            let geometry = map.layer_geometry(s).unwrap_or_default();
            Shown {
                layer: s.layer(),
                namespace,
                keyboard: s.cached_state().keyboard_interactivity,
                drawn: with_renderer_surface_state(s.wl_surface(), |state| {
                    state.buffer().is_some()
                })
                .unwrap_or(false),
                geometry,
                input: input_bounds(s.wl_surface(), geometry),
            }
        })
        .collect::<Vec<_>>();
    covered(&shown)
        .into_iter()
        .map(|i| surfaces[i].wl_surface().id())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shown(layer: Layer, namespace: &str, rect: (i32, i32, i32, i32)) -> Shown<'_> {
        Shown {
            layer,
            namespace,
            keyboard: KeyboardInteractivity::None,
            drawn: true,
            geometry: Rectangle::new((rect.0, rect.1).into(), (rect.2, rect.3).into()),
            input: None,
        }
    }

    const SCREEN: (i32, i32, i32, i32) = (0, 0, 1920, 1080);
    const CARD: (i32, i32, i32, i32) = (1480, 900, 420, 90);

    /// The Get Started card showed through the All desktops overview.
    #[test]
    fn the_overview_covers_the_desktop_s_layer_surfaces_under_it() {
        let layers = [
            shown(Layer::Background, "wallpaper", SCREEN),
            shown(Layer::Bottom, "widget", (0, 0, 200, 200)),
            shown(Layer::Top, "card", CARD),
            shown(Layer::Top, WORKSPACE_OVERVIEW_NAMESPACE, SCREEN),
            shown(Layer::Top, "menu over the overview", (100, 100, 200, 300)),
            shown(Layer::Overlay, "taskbar", (400, 1020, 1100, 60)),
        ];
        assert_eq!(covered(&layers), HashSet::from([1, 2]));
    }

    #[test]
    fn an_overview_not_drawn_yet_covers_nothing() {
        let mut overview = shown(Layer::Top, WORKSPACE_OVERVIEW_NAMESPACE, SCREEN);
        overview.drawn = false;
        let layers = [shown(Layer::Top, "card", CARD), overview];
        assert!(covered(&layers).is_empty());
    }

    /// The capture's thumbnail sat on top of the Get Started card.
    #[test]
    fn a_capture_preview_covers_the_card_where_it_draws() {
        let mut preview = shown(Layer::Overlay, "osd-capture", (1560, 0, 360, 1080));
        preview.input = Some(Rectangle::new((1580, 850).into(), (320, 200).into()));
        let layers = [shown(Layer::Top, "card", CARD), preview.clone()];
        assert_eq!(covered(&layers), HashSet::from([0]));

        // Thumbnails drawn higher up leave the card alone.
        preview.input = Some(Rectangle::new((1580, 200).into(), (320, 200).into()));
        assert!(covered(&[shown(Layer::Top, "card", CARD), preview.clone()]).is_empty());

        // An empty input region: nothing drawn that counts.
        preview.input = Some(Rectangle::default());
        assert!(covered(&[shown(Layer::Top, "card", CARD), preview]).is_empty());
    }

    #[test]
    fn only_a_keyboardless_indicator_covers_and_only_the_top_layer() {
        let mut toolbar = shown(Layer::Overlay, "osd-capture", SCREEN);
        toolbar.keyboard = KeyboardInteractivity::Exclusive;
        let layers = [shown(Layer::Top, "card", CARD), toolbar];
        assert!(covered(&layers).is_empty());

        let layers = [
            shown(Layer::Bottom, "widget", CARD),
            shown(Layer::Overlay, "taskbar", CARD),
            shown(Layer::Overlay, "notifications", SCREEN),
            shown(Layer::Overlay, "osd", SCREEN),
        ];
        assert!(covered(&layers).is_empty());
    }
}
