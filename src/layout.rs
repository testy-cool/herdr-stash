//! A tab's split tree, recovered from geometry.
//!
//! Herdr keeps the real tree — `{Split{direction, ratio, first, second}}` — in
//! `session.json`, and publishes something else over the socket: a flat list of
//! splits and a flat list of pane rectangles, per tab.
//!
//! ▲ Reading `session.json` instead was the obvious shortcut and is the wrong
//! one twice over. It names panes by an internal number that has to be mapped
//! back through `public_pane_numbers` to reach an id the API accepts, and it is
//! a persistence file Herdr rewrites on its own schedule — `version: 3` today,
//! with no compatibility promise to a plugin that reads it behind the server's
//! back. `session.snapshot` is the supported surface, so the tree is rebuilt
//! from it here.
//!
//! What makes that possible: a `split` publishes the **rectangle it divides**,
//! and its `ratio` is the first child's share of it. So the region is the key.
//! Look for a split whose rect is this region; if one exists, divide the region
//! by its ratio and recurse into both halves; if none does, the region is a
//! leaf and one pane's rect matches it.
//!
//! Measured against 0.7.5 on a three-pane tab, the arithmetic is exact — a
//! region 190 wide split at 0.5 yields 95, and the pane at x+95 starts there to
//! the cell. [`Rect::close`] still allows one cell of slack in each field,
//! because a rounding rule that holds on the widths observed is not a rounding
//! rule that has been promised.
//!
//! A region that matches neither a split nor a pane is a tree this module does
//! not understand, and it returns `None` rather than a partial shape. The caller
//! — [`crate::capture`] — treats that as a failed capture and leaves the
//! workspace alone, because the alternative is closing a workspace whose layout
//! was only mostly recorded.

use serde::{Deserialize, Serialize};

/// Herdr's two split directions. There is no `left` and no `up`: a pane is
/// always split rightwards or downwards, and the new pane is always the second
/// child.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Right,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(default)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

impl Rect {
    /// Whether two rectangles describe the same region, allowing one cell of
    /// slack per field. See the module note on why the slack exists even though
    /// the observed arithmetic is exact.
    fn close(self, other: Self) -> bool {
        let near = |a: u16, b: u16| a.abs_diff(b) <= 1;
        near(self.x, other.x)
            && near(self.y, other.y)
            && near(self.width, other.width)
            && near(self.height, other.height)
    }

    /// The two halves this region becomes when split at `ratio`.
    ///
    /// `ratio` is the **first** child's share, which is what Herdr publishes and
    /// what `pane.split` accepts — verified in both directions against 0.7.5:
    /// splitting a 190-column region at 0.3 left the original pane 57 columns
    /// and reported `ratio: 0.3` back.
    fn divide(self, direction: Direction, ratio: f32) -> (Self, Self) {
        match direction {
            Direction::Right => {
                let first = span(self.width, ratio);
                (
                    Self {
                        width: first,
                        ..self
                    },
                    Self {
                        x: self.x + first,
                        width: self.width - first,
                        ..self
                    },
                )
            }
            Direction::Down => {
                let first = span(self.height, ratio);
                (
                    Self {
                        height: first,
                        ..self
                    },
                    Self {
                        y: self.y + first,
                        height: self.height - first,
                        ..self
                    },
                )
            }
        }
    }
}

/// A child's cell count, kept inside the region even for a nonsense ratio: a
/// zero-width half would match no pane and fail a capture that could have
/// succeeded.
fn span(total: u16, ratio: f32) -> u16 {
    let cells = (f32::from(total) * ratio).round();
    (cells as u16).clamp(1, total.saturating_sub(1).max(1))
}

/// One entry of a tab's `splits` list.
#[derive(Debug, Clone, Deserialize)]
pub struct Split {
    pub direction: Direction,
    pub ratio: f32,
    pub rect: Rect,
}

/// One entry of a tab's `panes` list. The rect is the whole reason this shape is
/// read rather than [`herdr_sdk::model::Pane`], which has no geometry.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Placed {
    pub pane_id: String,
    pub focused: bool,
    pub rect: Rect,
}

/// A tab's layout, as `session.snapshot` publishes it.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Layout {
    pub workspace_id: String,
    pub tab_id: String,
    pub area: Rect,
    pub splits: Vec<Split>,
    pub panes: Vec<Placed>,
    pub zoomed: bool,
}

/// The recovered tree, in pane ids. [`crate::record`] carries the same shape
/// with the panes' recorded contents in place of their ids.
#[derive(Debug, Clone, PartialEq)]
pub enum Shape {
    Leaf(String),
    Split {
        direction: Direction,
        ratio: f32,
        first: Box<Shape>,
        second: Box<Shape>,
    },
}

impl Layout {
    /// Rebuild this tab's tree, or `None` if any region matches neither a split
    /// nor a pane.
    pub fn shape(&self) -> Option<Shape> {
        self.build(self.area, 0)
    }

    /// `depth` is a fuse, not a feature: a malformed splits list where a child
    /// region resolves back to its own parent would otherwise recurse until the
    /// stack ends. Herdr's own layouts are shallow — the deepest observed is 2.
    fn build(&self, region: Rect, depth: usize) -> Option<Shape> {
        const MAX_DEPTH: usize = 32;
        if depth > MAX_DEPTH {
            return None;
        }
        if let Some(split) = self.splits.iter().find(|split| split.rect.close(region)) {
            let (first, second) = split.rect.divide(split.direction, split.ratio);
            return Some(Shape::Split {
                direction: split.direction,
                ratio: split.ratio,
                first: Box::new(self.build(first, depth + 1)?),
                second: Box::new(self.build(second, depth + 1)?),
            });
        }
        self.panes
            .iter()
            .find(|pane| pane.rect.close(region))
            .map(|pane| Shape::Leaf(pane.pane_id.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: u16, y: u16, width: u16, height: u16) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    fn placed(pane_id: &str, rect: Rect) -> Placed {
        Placed {
            pane_id: pane_id.into(),
            focused: false,
            rect,
        }
    }

    /// One pane, no splits.
    #[test]
    fn a_single_pane_tab_is_a_leaf() {
        let layout = Layout {
            area: rect(29, 0, 190, 60),
            panes: vec![placed("w1:p1", rect(29, 0, 190, 60))],
            ..Layout::default()
        };
        assert_eq!(layout.shape(), Some(Shape::Leaf("w1:p1".into())));
    }

    /// The probe that established the ratio's meaning: `pane split --ratio 0.3`
    /// on a 190-column region, transcribed from `session.snapshot`.
    #[test]
    fn the_ratio_is_the_first_childs_share() {
        let layout = Layout {
            area: rect(29, 0, 190, 60),
            splits: vec![Split {
                direction: Direction::Right,
                ratio: 0.3,
                rect: rect(29, 0, 190, 60),
            }],
            panes: vec![
                placed("w14:p1", rect(29, 0, 57, 60)),
                placed("w14:p2", rect(86, 0, 133, 60)),
            ],
            ..Layout::default()
        };
        let Some(Shape::Split {
            ratio,
            first,
            second,
            ..
        }) = layout.shape()
        else {
            panic!("a two-pane tab is a split");
        };
        assert_eq!(ratio, 0.3);
        assert_eq!(*first, Shape::Leaf("w14:p1".into()));
        assert_eq!(*second, Shape::Leaf("w14:p2".into()));
    }

    /// A real three-pane tab, transcribed from `session.snapshot` — and the
    /// nesting cross-checked against the same tab in `session.json`, which
    /// records `Split{0.5, Split{0.32, 7, 8}, 9}`. The flat list has to yield
    /// that tree and not the other association.
    #[test]
    fn nested_splits_recover_the_tree_herdr_persisted() {
        let layout = Layout {
            area: rect(29, 0, 190, 60),
            splits: vec![
                Split {
                    direction: Direction::Right,
                    ratio: 0.5,
                    rect: rect(29, 0, 190, 60),
                },
                Split {
                    direction: Direction::Right,
                    ratio: 0.32,
                    rect: rect(29, 0, 95, 60),
                },
            ],
            panes: vec![
                placed("w6:pF", rect(29, 0, 30, 60)),
                placed("w6:p1", rect(59, 0, 65, 60)),
                placed("w6:pE", rect(124, 0, 95, 60)),
            ],
            ..Layout::default()
        };
        let expected = Shape::Split {
            direction: Direction::Right,
            ratio: 0.5,
            first: Box::new(Shape::Split {
                direction: Direction::Right,
                ratio: 0.32,
                first: Box::new(Shape::Leaf("w6:pF".into())),
                second: Box::new(Shape::Leaf("w6:p1".into())),
            }),
            second: Box::new(Shape::Leaf("w6:pE".into())),
        };
        assert_eq!(layout.shape(), Some(expected));
    }

    #[test]
    fn a_vertical_split_divides_the_height() {
        let layout = Layout {
            area: rect(0, 0, 100, 40),
            splits: vec![Split {
                direction: Direction::Down,
                ratio: 0.25,
                rect: rect(0, 0, 100, 40),
            }],
            panes: vec![
                placed("w1:p1", rect(0, 0, 100, 10)),
                placed("w1:p2", rect(0, 10, 100, 30)),
            ],
            ..Layout::default()
        };
        let Some(Shape::Split {
            direction, second, ..
        }) = layout.shape()
        else {
            panic!("a stacked tab is a split");
        };
        assert_eq!(direction, Direction::Down);
        assert_eq!(*second, Shape::Leaf("w1:p2".into()));
    }

    /// The case that must fail loudly: a pane list that does not cover the
    /// regions the splits describe. Capture refuses rather than recording half a
    /// tab and closing the workspace.
    #[test]
    fn an_unmatched_region_yields_no_shape() {
        let layout = Layout {
            area: rect(0, 0, 100, 40),
            splits: vec![Split {
                direction: Direction::Right,
                ratio: 0.5,
                rect: rect(0, 0, 100, 40),
            }],
            panes: vec![placed("w1:p1", rect(0, 0, 50, 40))],
            ..Layout::default()
        };
        assert_eq!(layout.shape(), None);
    }

    /// Herdr's rounding is exact on every width observed; this is the promise it
    /// has not made. One cell of drift in a reported rect must not lose a tab.
    #[test]
    fn a_cell_of_drift_still_matches() {
        let layout = Layout {
            area: rect(0, 0, 101, 40),
            splits: vec![Split {
                direction: Direction::Right,
                ratio: 0.5,
                rect: rect(0, 0, 101, 40),
            }],
            panes: vec![
                placed("w1:p1", rect(0, 0, 50, 40)),
                placed("w1:p2", rect(51, 0, 50, 40)),
            ],
            ..Layout::default()
        };
        assert!(layout.shape().is_some());
    }
}
