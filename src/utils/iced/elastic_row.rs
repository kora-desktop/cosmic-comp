//! A row that shares a width shortfall between its flexible children.
//!
//! Iced's flex pass hands every `Shrink` child the whole remaining width, in
//! order, so the first one that wants more than fits swallows everything and
//! leaves its siblings nothing at all. In the Halo pill that turns a long
//! window title into a header with no app name — and, before the controls were
//! pinned to a fixed width, no buttons either.
//!
//! This row measures what every child would like, and only when they do not
//! all fit does it hand out the width by max-min fairness: the smallest
//! appetite is served first, so a short label keeps all of its text and the
//! long one absorbs the shortfall. Children marked rigid (an icon, a control)
//! are never asked to give anything up.
//!
//! A child may also give way in an order of its own (it is cut to the floor
//! before a later one is touched) and may leave a smaller stand-in behind
//! when it is dropped.

use iced_core::{
    Element, Event, Layout, Length, Point, Rectangle, Shell, Size, Vector, layout, mouse, overlay,
    renderer,
    widget::{Operation, Tree, Widget, tree},
};

/// How one child of the row gives way.
#[derive(Debug, Clone, Copy, Default)]
struct Slot {
    /// The child may be squeezed.
    elastic: bool,
    /// `Some(rank)` if the child may be dropped once squeezing is not enough,
    /// lowest rank first. `None` never leaves.
    priority: Option<u8>,
    /// The child lines up on the shared baseline.
    baseline: bool,
    /// Elastic children give way in ascending order.
    order: u8,
    /// The child this one stands in for while it is dropped.
    stands_in_for: Option<usize>,
}

/// See the module documentation.
pub struct ElasticRow<'a, Message, Theme, Renderer> {
    children: Vec<Element<'a, Message, Theme, Renderer>>,
    /// Parallel to `children`: how each one gives way.
    slots: Vec<Slot>,
    spacing: f32,
    /// Width to leave for siblings laid out after this row.
    reserve: f32,
    /// Width the siblings get even if this row has to go below its floor.
    reserve_min: f32,
    /// Width below which squeezing an elastic child is pointless, so a
    /// droppable sibling leaves instead.
    floor: f32,
}

impl<Message, Theme, Renderer> Default for ElasticRow<'_, Message, Theme, Renderer> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a, Message, Theme, Renderer> ElasticRow<'a, Message, Theme, Renderer> {
    pub fn new() -> Self {
        Self {
            children: Vec::new(),
            slots: Vec::new(),
            spacing: 0.0,
            reserve: 0.0,
            reserve_min: 0.0,
            floor: 0.0,
        }
    }

    /// Add a child that always gets the width it asks for.
    #[must_use]
    pub fn push_rigid(mut self, child: impl Into<Element<'a, Message, Theme, Renderer>>) -> Self {
        self.children.push(child.into());
        self.slots.push(Slot {
            elastic: false,
            priority: None,
            ..Slot::default()
        });
        self
    }

    /// Add a child that gives way when the row runs out of width.
    #[must_use]
    pub fn push_elastic(mut self, child: impl Into<Element<'a, Message, Theme, Renderer>>) -> Self {
        self.children.push(child.into());
        self.slots.push(Slot {
            elastic: true,
            priority: None,
            ..Slot::default()
        });
        self
    }

    /// Add a child that leaves the row entirely once squeezing no longer frees
    /// enough width. Lower ranks go first.
    #[must_use]
    pub fn push_droppable(
        mut self,
        rank: u8,
        child: impl Into<Element<'a, Message, Theme, Renderer>>,
    ) -> Self {
        self.children.push(child.into());
        self.slots.push(Slot {
            elastic: false,
            priority: Some(rank),
            ..Slot::default()
        });
        self
    }

    /// Add a child that gives way first and then leaves altogether: it shares
    /// the shortfall like an elastic child, but once squeezing stops helping
    /// it goes rather than sit there as an ellipsis.
    #[must_use]
    pub fn push_elastic_droppable(
        mut self,
        rank: u8,
        child: impl Into<Element<'a, Message, Theme, Renderer>>,
    ) -> Self {
        self.children.push(child.into());
        self.slots.push(Slot {
            elastic: true,
            priority: Some(rank),
            ..Slot::default()
        });
        self
    }

    /// Add a rigid stand-in for the child pushed just before, shown only while
    /// that child is dropped: a status that has no room for its words keeps its dot.
    #[must_use]
    pub fn push_stand_in(
        mut self,
        child: impl Into<Element<'a, Message, Theme, Renderer>>,
    ) -> Self {
        let owner = self.children.len().checked_sub(1);
        self = self.push_rigid(child);
        if let Some(last) = self.slots.last_mut() {
            last.stands_in_for = owner;
        }
        self
    }

    /// Have the elastic child just pushed give way only once every child of a
    /// lower order is down to the floor. Children of one order share the cut.
    #[must_use]
    pub fn gives_way_in_order(mut self, order: u8) -> Self {
        if let Some(last) = self.slots.last_mut() {
            last.order = order;
        }
        self
    }

    /// Line the child just pushed up on a baseline it shares with the others
    /// marked so, as CSS `align-items: baseline` does; the group is centred as one.
    #[must_use]
    pub fn on_baseline(mut self) -> Self {
        if let Some(last) = self.slots.last_mut() {
            last.baseline = true;
        }
        self
    }

    #[must_use]
    pub fn spacing(mut self, spacing: f32) -> Self {
        self.spacing = spacing;
        self
    }

    /// Keep `reserve` px for whatever is laid out after this row, so this row
    /// gives way before its siblings do.
    #[must_use]
    pub fn reserve(mut self, reserve: f32) -> Self {
        self.reserve = reserve.max(0.0);
        self
    }

    /// Width the siblings after this row keep whatever happens — the floor
    /// gives way to it, so the one control that must never leave still fits
    /// even when that costs the last of the text.
    #[must_use]
    pub fn reserve_min(mut self, reserve: f32) -> Self {
        self.reserve_min = reserve.max(0.0);
        self
    }

    /// Squeeze an elastic child no smaller than `floor`; past that a droppable
    /// child leaves instead, so the text that remains stays readable.
    #[must_use]
    pub fn floor(mut self, floor: f32) -> Self {
        self.floor = floor.max(0.0);
        self
    }

    /// Unwrap a row of exactly one child: with nothing to share it against,
    /// the row would only add a layer that lays out identically. Any other
    /// row is handed back unchanged — including one whose single child still
    /// has to give way, since the room this row keeps for its siblings and the
    /// floor it squeezes to are the row's own doing and would be lost with it.
    pub fn into_single(mut self) -> Result<Element<'a, Message, Theme, Renderer>, Self> {
        let constrained = self.reserve > 0.0 || self.reserve_min > 0.0 || self.floor > 0.0;
        if self.children.len() == 1 && !constrained {
            Ok(self.children.remove(0))
        } else {
            Err(self)
        }
    }
}

/// How wide each child may be, given `budget` to share between them.
///
/// Elastic children give way by order: the lowest order is cut towards
/// `floor` before the next is touched. If cutting everything to the floor is
/// not enough, the order no longer matters and the width is shared evenly.
/// Returns `None` when everything fits, so the caller can keep the layout it
/// already has instead of measuring a second time.
fn shares(
    natural: &[f32],
    elastic: &[bool],
    order: &[u8],
    floor: f32,
    budget: f32,
) -> Option<Vec<f32>> {
    let total: f32 = natural.iter().sum();
    if total <= budget {
        return None;
    }
    let mut shares = natural.to_vec();
    let mut shortfall = total - budget;
    let spare = |index: usize| natural[index] - natural[index].min(floor);
    let squeezable: Vec<usize> = (0..natural.len()).filter(|index| elastic[*index]).collect();
    if squeezable.iter().map(|&index| spare(index)).sum::<f32>() < shortfall {
        let rigid: f32 = (0..natural.len())
            .filter(|index| !elastic[*index])
            .map(|index| natural[index])
            .sum();
        share_evenly(&mut shares, natural, &squeezable, (budget - rigid).max(0.0));
        return Some(shares);
    }
    let mut orders: Vec<u8> = squeezable.iter().map(|&index| order[index]).collect();
    orders.sort_unstable();
    orders.dedup();
    for current in orders {
        let group: Vec<usize> = squeezable
            .iter()
            .copied()
            .filter(|&index| order[index] == current)
            .collect();
        let cut = shortfall.min(group.iter().map(|&index| spare(index)).sum());
        if cut <= 0.0 {
            continue;
        }
        let wanted: f32 = group.iter().map(|&index| natural[index]).sum();
        share_evenly(&mut shares, natural, &group, wanted - cut);
        shortfall -= cut;
    }
    Some(shares)
}

/// Max-min fairness: the smallest appetite in `group` is served first.
fn share_evenly(shares: &mut [f32], natural: &[f32], group: &[usize], total: f32) {
    let mut group = group.to_vec();
    group.sort_by(|a, b| natural[*a].total_cmp(&natural[*b]));
    let mut left = total;
    let mut remaining = group.len();
    for index in group {
        // `remaining` is never zero: the loop runs exactly `group.len()` times.
        let share = natural[index].min(left / remaining as f32);
        shares[index] = share;
        left -= share;
        remaining -= 1;
    }
}

/// Which children must leave for the rest to fit `budget`.
///
/// Squeezing comes first: an elastic child is counted at `floor` (or at its
/// natural width, if that is already smaller). Only when even that does not
/// fit does the lowest-ranked droppable child leave, and the question is
/// asked again. A child with no rank never leaves, so the row always keeps
/// its essentials however narrow it gets. A stand-in is hidden until the
/// child it stands in for leaves.
fn drops(
    natural: &[f32],
    elastic: &[bool],
    priority: &[Option<u8>],
    stands_in_for: &[Option<usize>],
    spacing: f32,
    budget: f32,
    floor: f32,
) -> Vec<bool> {
    let mut dropped: Vec<bool> = stands_in_for.iter().map(Option::is_some).collect();
    loop {
        let alive: Vec<usize> = (0..natural.len())
            .filter(|index| !dropped[*index])
            .collect();
        if alive.is_empty() {
            break;
        }
        let needed: f32 = alive
            .iter()
            .map(|&index| {
                if elastic[index] {
                    natural[index].min(floor)
                } else {
                    natural[index]
                }
            })
            .sum::<f32>()
            + spacing * (alive.len() - 1) as f32;
        if needed <= budget {
            break;
        }
        let Some(victim) = alive
            .iter()
            .copied()
            .filter(|&index| priority[index].is_some())
            .min_by_key(|&index| priority[index])
        else {
            break;
        };
        dropped[victim] = true;
        for (index, owner) in stands_in_for.iter().enumerate() {
            if *owner == Some(victim) {
                dropped[index] = false;
            }
        }
    }
    dropped
}

/// Which children the last layout dropped; they are neither drawn nor sent events.
#[derive(Default)]
struct Hidden(Vec<bool>);

impl Hidden {
    fn of(tree: &Tree) -> &[bool] {
        &tree.state.downcast_ref::<Self>().0
    }
}

impl<Message, Theme, Renderer> Widget<Message, Theme, Renderer>
    for ElasticRow<'_, Message, Theme, Renderer>
where
    Renderer: iced_core::Renderer,
{
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<Hidden>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(Hidden::default())
    }

    fn children(&self) -> Vec<Tree> {
        self.children.iter().map(Tree::new).collect()
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(&self.children);
    }

    fn size(&self) -> Size<Length> {
        Size {
            width: Length::Shrink,
            height: Length::Shrink,
        }
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let limits = limits.width(Length::Shrink).height(Length::Shrink);
        let max = limits.max();
        let count = self.children.len();
        if count == 0 {
            return layout::Node::new(limits.resolve(Length::Shrink, Length::Shrink, Size::ZERO));
        }

        // Whatever this row leaves unused stays with the siblings after it —
        // but never below its own floor, or the row would squeeze itself away
        // while those siblings still sat at their full width. Past that point
        // the shortfall belongs to them, and they give way in their turn.
        // A row can be handed a negative width by a parent that has already
        // overflowed, so clamp against a floor of zero rather than against a
        // bound that may sit below it.
        let room = max.width.max(0.0);
        let outer = (room - self.reserve)
            .max(self.floor)
            .min(room - self.reserve_min)
            .clamp(0.0, room);
        let total_spacing = self.spacing * (count - 1) as f32;
        let budget = (outer - total_spacing).max(0.0);

        // What every child would take if it were alone with the whole budget.
        let loose = layout::Limits::new(Size::ZERO, Size::new(budget, max.height));
        let mut nodes: Vec<layout::Node> = (0..count)
            .map(|index| {
                self.children[index].as_widget_mut().layout(
                    &mut tree.children[index],
                    renderer,
                    &loose,
                )
            })
            .collect();
        let natural: Vec<f32> = nodes.iter().map(|node| node.size().width).collect();
        let elastic: Vec<bool> = self.slots.iter().map(|slot| slot.elastic).collect();
        let priority: Vec<Option<u8>> = self.slots.iter().map(|slot| slot.priority).collect();
        let stands_in_for: Vec<Option<usize>> =
            self.slots.iter().map(|slot| slot.stands_in_for).collect();

        let dropped = drops(
            &natural,
            &elastic,
            &priority,
            &stands_in_for,
            self.spacing,
            outer,
            self.floor,
        );
        let alive = dropped.iter().filter(|gone| !**gone).count();
        // A dropped child takes no width and no spacing with it.
        let kept_spacing = self.spacing * (alive.saturating_sub(1)) as f32;
        let budget = (outer - kept_spacing).max(0.0);

        let kept: Vec<usize> = (0..count).filter(|index| !dropped[*index]).collect();
        let kept_natural: Vec<f32> = kept.iter().map(|&index| natural[index]).collect();
        let kept_elastic: Vec<bool> = kept.iter().map(|&index| elastic[index]).collect();
        let kept_order: Vec<u8> = kept.iter().map(|&index| self.slots[index].order).collect();
        if let Some(shares) = shares(
            &kept_natural,
            &kept_elastic,
            &kept_order,
            self.floor,
            budget,
        ) {
            for (slot, &index) in kept.iter().enumerate() {
                if shares[slot] >= natural[index] {
                    continue;
                }
                let limits =
                    layout::Limits::new(Size::ZERO, Size::new(shares[slot].max(0.0), max.height));
                nodes[index] = self.children[index].as_widget_mut().layout(
                    &mut tree.children[index],
                    renderer,
                    &limits,
                );
            }
        }
        tree.state.downcast_mut::<Hidden>().0.clone_from(&dropped);
        let zero = layout::Limits::new(Size::ZERO, Size::ZERO);
        for index in (0..count).filter(|index| dropped[*index]) {
            nodes[index] = self.children[index].as_widget_mut().layout(
                &mut tree.children[index],
                renderer,
                &zero,
            );
        }

        // Each shown child on the shared baseline: its baseline, or its bottom edge.
        let baselines: Vec<Option<f32>> = nodes
            .iter()
            .enumerate()
            .map(|(index, node)| {
                (self.slots[index].baseline && !dropped[index])
                    .then(|| node.baseline().unwrap_or(node.size().height))
            })
            .collect();
        let above = baselines.iter().flatten().copied().fold(0.0_f32, f32::max);
        let below = nodes
            .iter()
            .zip(&baselines)
            .filter_map(|(node, baseline)| baseline.map(|b| node.size().height - b))
            .fold(0.0_f32, f32::max);

        let height = nodes
            .iter()
            .enumerate()
            .filter(|(index, _)| !dropped[*index] && !self.slots[*index].baseline)
            .map(|(_, node)| node.size().height)
            .fold(above + below, f32::max);
        let group_top = (height - above - below) / 2.0;
        let mut x = 0.0_f32;
        for (index, node) in nodes.iter_mut().enumerate() {
            if dropped[index] {
                // Park it where the row ends; it has no size to show.
                node.move_to_mut(Point::new(x, height / 2.0));
                continue;
            }
            let size = node.size();
            let y = match baselines[index] {
                Some(baseline) => group_top + above - baseline,
                None => (height - size.height) / 2.0,
            };
            node.move_to_mut(Point::new(x, y));
            x += size.width + self.spacing;
        }
        let width = (x - self.spacing).max(0.0);
        let row_baseline = baselines
            .iter()
            .any(Option::is_some)
            .then_some(group_top + above);

        layout::Node::with_children(
            limits.resolve(Length::Shrink, Length::Shrink, Size::new(width, height)),
            nodes,
        )
        .with_baseline(row_baseline)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        operation.container(None, layout.bounds());
        let hidden = Hidden::of(tree).to_vec();
        operation.traverse(&mut |operation| {
            self.children
                .iter_mut()
                .zip(&mut tree.children)
                .zip(layout.children())
                .enumerate()
                .filter(|(index, _)| !hidden.get(*index).copied().unwrap_or(false))
                .for_each(|(_, ((child, state), layout))| {
                    child
                        .as_widget_mut()
                        .operate(state, layout, renderer, operation);
                });
        });
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        let hidden = Hidden::of(tree).to_vec();
        for (_, ((child, tree), layout)) in self
            .children
            .iter_mut()
            .zip(&mut tree.children)
            .zip(layout.children())
            .enumerate()
            .filter(|(index, _)| !hidden.get(*index).copied().unwrap_or(false))
        {
            child
                .as_widget_mut()
                .update(tree, event, layout, cursor, renderer, shell, viewport);
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        let hidden = Hidden::of(tree);
        self.children
            .iter()
            .zip(&tree.children)
            .zip(layout.children())
            .enumerate()
            .filter(|(index, _)| !hidden.get(*index).copied().unwrap_or(false))
            .map(|(_, ((child, tree), layout))| {
                child
                    .as_widget()
                    .mouse_interaction(tree, layout, cursor, viewport, renderer)
            })
            .max()
            .unwrap_or_default()
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let hidden = Hidden::of(tree);
        for (_, ((child, tree), layout)) in self
            .children
            .iter()
            .zip(&tree.children)
            .zip(layout.children())
            .enumerate()
            .filter(|(index, (_, layout))| {
                !hidden.get(*index).copied().unwrap_or(false)
                    && layout.bounds().intersects(viewport)
            })
        {
            child
                .as_widget()
                .draw(tree, renderer, theme, style, layout, cursor, viewport);
        }
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        overlay::from_children(
            &mut self.children,
            tree,
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

impl<'a, Message, Theme, Renderer> From<ElasticRow<'a, Message, Theme, Renderer>>
    for Element<'a, Message, Theme, Renderer>
where
    Message: 'a,
    Theme: 'a,
    Renderer: iced_core::Renderer + 'a,
{
    fn from(row: ElasticRow<'a, Message, Theme, Renderer>) -> Self {
        Self::new(row)
    }
}

#[cfg(test)]
mod tests {
    use super::{drops, shares};

    /// One order and no floor: plain max-min fairness.
    fn even(natural: &[f32], elastic: &[bool], budget: f32) -> Option<Vec<f32>> {
        shares(natural, elastic, &vec![0; natural.len()], 0.0, budget)
    }

    #[test]
    fn everything_that_fits_is_left_alone() {
        assert!(even(&[10.0, 20.0], &[true, true], 100.0).is_none());
        assert!(even(&[50.0, 50.0], &[true, true], 100.0).is_none());
    }

    #[test]
    fn a_short_label_keeps_its_text_while_the_long_one_gives_way() {
        // A 500px title beside a 20px app name in 200px: the app name is cheap
        // enough to serve in full, so only the title is cut.
        let shares = even(&[500.0, 20.0], &[true, true], 200.0).unwrap();
        assert_eq!(shares, vec![180.0, 20.0]);
    }

    #[test]
    fn two_greedy_children_split_the_width_evenly() {
        let shares = even(&[500.0, 400.0], &[true, true], 200.0).unwrap();
        assert_eq!(shares, vec![100.0, 100.0]);
    }

    #[test]
    fn a_rigid_child_is_never_squeezed() {
        // The icon is rigid: it keeps 20px and the texts share the other 80.
        let shares = even(&[20.0, 500.0, 400.0], &[false, true, true], 100.0).unwrap();
        assert_eq!(shares, vec![20.0, 40.0, 40.0]);
    }

    #[test]
    fn a_budget_swallowed_by_rigid_children_never_goes_negative() {
        let shares = even(&[300.0, 50.0], &[false, true], 100.0).unwrap();
        assert_eq!(shares, vec![300.0, 0.0]);
        assert!(shares.iter().all(|share| *share >= 0.0));
    }

    #[test]
    fn no_elastic_child_leaves_every_width_alone() {
        let shares = even(&[300.0, 50.0], &[false, false], 100.0).unwrap();
        assert_eq!(shares, vec![300.0, 50.0]);
    }

    #[test]
    fn a_lower_order_is_cut_to_the_floor_before_a_higher_one_gives_anything() {
        // A 300px subtitle and a 200px status in 400px, floor 80: the subtitle
        // goes first, all of the 100px shortfall from it.
        let cut = shares(&[300.0, 200.0], &[true, true], &[0, 1], 80.0, 400.0).unwrap();
        assert_eq!(cut, vec![200.0, 200.0]);
        // 260px short: the subtitle stops at 80, the status gives the other 40.
        let cut = shares(&[300.0, 200.0], &[true, true], &[0, 1], 80.0, 240.0).unwrap();
        assert_eq!(cut, vec![80.0, 160.0]);
    }

    #[test]
    fn past_every_floor_the_order_no_longer_matters() {
        let cut = shares(&[300.0, 200.0], &[true, true], &[0, 1], 80.0, 100.0).unwrap();
        assert_eq!(cut, vec![50.0, 50.0]);
    }

    #[test]
    fn a_stand_in_shows_only_once_its_child_has_left() {
        // Name, status (droppable, with a 6px dot standing in), subtitle (droppable last).
        let natural = [40.0, 200.0, 6.0, 150.0];
        let elastic = [false, true, false, true];
        let priority = [None, Some(0), None, Some(1)];
        let stands_in_for = [None, None, Some(1), None];
        let at = |budget| {
            drops(
                &natural,
                &elastic,
                &priority,
                &stands_in_for,
                0.0,
                budget,
                80.0,
            )
        };
        assert_eq!(at(400.0), vec![false, false, true, false]);
        assert_eq!(
            at(200.0),
            vec![false, false, true, false],
            "both at the floor fit"
        );
        assert_eq!(
            at(150.0),
            vec![false, true, false, false],
            "the status keeps its dot"
        );
        assert_eq!(
            at(60.0),
            vec![false, true, false, true],
            "the dot stays to the end"
        );
    }
}
