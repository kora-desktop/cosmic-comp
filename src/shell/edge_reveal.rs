// SPDX-License-Identifier: GPL-3.0-only

//! The edge reveals over a fullscreen window: the Halo at the top and an
//! auto-hiding panel on any edge. They hold back over a game, which may
//! scroll when the pointer reaches an edge, and are reached by touch and at an
//! edge shared with another output, which the pointer would cross instead.

use crate::dbus::game_mode::LAUNCHER_APP_ID;
use smithay::utils::{Point, Rectangle};

/// Game mode shows a game, not its launcher, which is a fullscreen app like any other.
pub fn game_mode_shows_game(active: bool, app_id: Option<u32>) -> bool {
    active && app_id.is_some_and(|id| id != LAUNCHER_APP_ID)
}

/// Whether both edge reveals hold back over a fullscreen window.
pub fn reveals_held_back(game_mode_game: bool, pointer_constrained: bool) -> bool {
    game_mode_game || pointer_constrained
}

/// How far the pointer pushes past a shared edge before crossing, so a reveal
/// waiting there can be reached.
pub const EDGE_BARRIER: f64 = 100.0;
/// A touch starting this close to an edge may swipe a reveal in.
pub const TOUCH_EDGE: f64 = 16.0;
/// How far inward that touch travels before it counts as a swipe.
pub const TOUCH_SWIPE: f64 = 24.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Top,
    Bottom,
    Left,
    Right,
}

impl Edge {
    pub const ALL: [Edge; 4] = [Edge::Bottom, Edge::Top, Edge::Left, Edge::Right];

    /// Whether the edge runs top to bottom, so it is crossed along x.
    pub fn vertical(self) -> bool {
        matches!(self, Edge::Left | Edge::Right)
    }

    fn at_start(self) -> bool {
        matches!(self, Edge::Top | Edge::Left)
    }

    /// The `[start, end)` span of `area` on the axis that crosses this edge.
    pub fn span<K>(self, area: Rectangle<f64, K>) -> (f64, f64) {
        if self.vertical() {
            (area.loc.x, area.loc.x + area.size.w)
        } else {
            (area.loc.y, area.loc.y + area.size.h)
        }
    }

    /// The coordinate of `point` on the axis that crosses this edge.
    pub fn across<K>(self, point: Point<f64, K>) -> f64 {
        if self.vertical() { point.x } else { point.y }
    }

    /// The coordinate of `point` along this edge.
    pub fn along<K>(self, point: Point<f64, K>) -> f64 {
        if self.vertical() { point.y } else { point.x }
    }

    /// How far `from` → `to` goes past this edge of the span `[start, end)`;
    /// zero or less when it stays on this side.
    pub fn past(self, start: f64, end: f64, from: f64, to: f64) -> f64 {
        if self.at_start() {
            if from >= start { start - to } else { 0.0 }
        } else if from < end {
            to - end
        } else {
            0.0
        }
    }

    /// Where the pointer rests against this edge: inside the outermost physical pixel.
    pub fn rest(self, start: f64, end: f64, scale: f64) -> f64 {
        if self.at_start() {
            start
        } else {
            end - 0.5 / scale
        }
    }

    /// `to` held against this edge of `area`, still beside the output along it.
    pub fn rest_point<K>(
        self,
        area: Rectangle<f64, K>,
        to: Point<f64, K>,
        scale: f64,
    ) -> Point<f64, K> {
        let (start, end) = self.span(area);
        let rest = self.rest(start, end, scale);
        if self.vertical() {
            let bottom = (area.loc.y + area.size.h).next_down();
            Point::from((rest, to.y.clamp(area.loc.y, bottom)))
        } else {
            let right = (area.loc.x + area.size.w).next_down();
            Point::from((to.x.clamp(area.loc.x, right), rest))
        }
    }

    /// Distance from this edge to `at`, inward.
    pub fn inward(self, start: f64, end: f64, at: f64) -> f64 {
        if self.at_start() {
            at - start
        } else {
            end - at
        }
    }
}

/// Holds the pointer at an armed shared edge until it has pushed [`EDGE_BARRIER`] past it.
#[derive(Debug, Default)]
pub struct EdgeBarrier {
    pushed: f64,
}

impl EdgeBarrier {
    /// `past` is how far this motion goes beyond the armed edge. True holds the pointer.
    pub fn hold(&mut self, past: f64) -> bool {
        if past <= 0.0 {
            self.pushed = 0.0;
            return false;
        }
        self.pushed += past;
        if self.pushed < EDGE_BARRIER {
            true
        } else {
            self.pushed = 0.0;
            false
        }
    }
}

/// A touch that began at an armed edge, until it swipes inward or lifts.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EdgeSwipe {
    pub slot: i32,
    pub edge: Edge,
    pub start: f64,
}

impl EdgeSwipe {
    /// Whether a touch at `at` (across the edge) began close enough to `edge`.
    pub fn starts(edge: Edge, start: f64, end: f64, at: f64) -> bool {
        (0.0..TOUCH_EDGE).contains(&edge.inward(start, end, at))
    }

    /// Whether the touch, now at `at` across its edge, has swiped inward far enough.
    pub fn swiped(&self, at: f64) -> bool {
        let inward = if self.edge.at_start() {
            at - self.start
        } else {
            self.start - at
        };
        inward >= TOUCH_SWIPE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_game_in_game_mode_or_a_held_pointer_holds_both_reveals_back() {
        let game = game_mode_shows_game(true, Some(1237320));
        assert!(game);
        assert!(reveals_held_back(game, false), "a game game mode shows");
        assert!(
            reveals_held_back(false, true),
            "a locked or confined pointer"
        );
        assert!(reveals_held_back(game, true));
    }

    /// The launcher is game mode's active app 769 between games: both reveals stay.
    #[test]
    fn the_launcher_keeps_both_reveals() {
        let launcher = game_mode_shows_game(true, Some(LAUNCHER_APP_ID));
        assert_eq!(LAUNCHER_APP_ID, 769);
        assert!(!launcher);
        assert!(!reveals_held_back(launcher, false));
        assert!(
            reveals_held_back(launcher, true),
            "unless it holds the pointer"
        );
    }

    /// Any other fullscreen window, a game started outside game mode included,
    /// keeps both reveals unless it holds the pointer.
    #[test]
    fn outside_game_mode_only_a_held_pointer_counts() {
        for app_id in [None, Some(LAUNCHER_APP_ID), Some(1237320)] {
            let game = game_mode_shows_game(false, app_id);
            assert!(!game, "{app_id:?}");
            assert!(!reveals_held_back(game, false));
        }
    }
}
#[cfg(test)]
mod reach_tests {
    use super::*;

    #[test]
    fn a_shared_edge_holds_the_pointer_until_it_pushes_through() {
        let mut barrier = EdgeBarrier::default();
        assert!(barrier.hold(30.0), "the first push stops at the edge");
        assert!(barrier.hold(60.0));
        assert!(!barrier.hold(20.0), "100px past it, the pointer crosses");
        assert!(barrier.hold(5.0), "the next arrival is held again");
        assert!(!barrier.hold(0.0), "moving back along or away releases it");
        assert!(barrier.hold(99.0), "and the push starts over");
    }

    #[test]
    fn crossing_is_measured_only_through_the_edge_it_left() {
        let (top, bottom) = (0.0, 1080.0);
        assert_eq!(Edge::Bottom.past(top, bottom, 1079.5, 1090.0), 10.0);
        assert!(Edge::Bottom.past(top, bottom, 1000.0, 1050.0) <= 0.0);
        assert!(
            Edge::Bottom.past(top, bottom, 1080.0, 1090.0) <= 0.0,
            "already beyond"
        );
        assert_eq!(Edge::Top.past(top, bottom, 0.0, -12.0), 12.0);
        assert!(Edge::Top.past(top, bottom, 20.0, 10.0) <= 0.0);
        for scale in [1.0, 1.5, 2.0] {
            let rest = Edge::Bottom.rest(top, bottom, scale);
            assert!(
                rest < bottom && bottom - rest < 1.0 / scale,
                "inside the last pixel"
            );
            assert_eq!(Edge::Top.rest(top, bottom, scale), top);
        }
    }

    #[test]
    fn a_touch_from_the_edge_swipes_inward() {
        let (top, bottom) = (0.0, 1080.0);
        assert!(EdgeSwipe::starts(Edge::Bottom, top, bottom, 1070.0));
        assert!(!EdgeSwipe::starts(Edge::Bottom, top, bottom, 1060.0));
        assert!(EdgeSwipe::starts(Edge::Top, top, bottom, 4.0));
        assert!(!EdgeSwipe::starts(Edge::Top, top, bottom, 1070.0));
        let up = EdgeSwipe {
            slot: 0,
            edge: Edge::Bottom,
            start: 1075.0,
        };
        assert!(!up.swiped(1060.0));
        assert!(up.swiped(1051.0));
        assert!(!up.swiped(1080.0), "sliding off the edge is no swipe");
        let down = EdgeSwipe {
            slot: 1,
            edge: Edge::Top,
            start: 2.0,
        };
        assert!(down.swiped(26.0));
        assert!(!down.swiped(20.0));
    }

    #[test]
    fn side_edges_cross_on_x_like_top_and_bottom_cross_on_y() {
        let output = Rectangle::<f64, smithay::utils::Logical>::new(
            (1920.0, 0.0).into(),
            (1280.0, 800.0).into(),
        );
        assert_eq!(Edge::Left.span(output), (1920.0, 3200.0));
        assert_eq!(Edge::Top.span(output), (0.0, 800.0));
        let (start, end) = Edge::Left.span(output);
        assert_eq!(Edge::Left.past(start, end, 1920.0, 1900.0), 20.0);
        assert!(Edge::Left.past(start, end, 1930.0, 1925.0) <= 0.0);
        assert_eq!(Edge::Right.past(start, end, 3199.5, 3210.0), 10.0);
        let point = Point::<f64, smithay::utils::Logical>::from((1925.0, 300.0));
        assert_eq!(
            (Edge::Left.across(point), Edge::Left.along(point)),
            (1925.0, 300.0)
        );
        assert_eq!(
            (Edge::Top.across(point), Edge::Top.along(point)),
            (300.0, 1925.0)
        );

        let held = Edge::Right.rest_point(output, Point::from((3300.0, 900.0)), 2.0);
        assert!(
            held.x < 3200.0 && 3200.0 - held.x < 0.5,
            "inside the last pixel"
        );
        assert!(held.y < 800.0 && held.y > 799.0, "kept beside the output");
        assert_eq!(
            Edge::Left.rest_point(output, Point::from((1800.0, 10.0)), 1.0),
            Point::from((1920.0, 10.0))
        );

        assert!(EdgeSwipe::starts(Edge::Left, start, end, 1930.0));
        assert!(!EdgeSwipe::starts(Edge::Left, start, end, 1940.0));
        assert!(EdgeSwipe::starts(Edge::Right, start, end, 3190.0));
        let right = EdgeSwipe {
            slot: 0,
            edge: Edge::Right,
            start: 3195.0,
        };
        assert!(right.swiped(3170.0));
        assert!(!right.swiped(3180.0));
        let left = EdgeSwipe {
            slot: 0,
            edge: Edge::Left,
            start: 1922.0,
        };
        assert!(left.swiped(1946.0));
    }
}
