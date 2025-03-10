/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cmp::max;
use std::collections::VecDeque;
use std::sync::Arc;
use std::{fmt, mem};

use app_units::{Au, MIN_AU};
use base::print_tree::PrintTree;
use bitflags::bitflags;
use euclid::default::{Point2D, Rect, Size2D};
use fonts::{FontContext, FontMetrics};
use log::debug;
use range::{Range, RangeIndex, int_range_index};
use script_layout_interface::wrapper_traits::PseudoElementType;
use serde::Serialize;
use servo_geometry::MaxRect;
use style::computed_values::display::T as Display;
use style::computed_values::overflow_x::T as StyleOverflow;
use style::computed_values::position::T as Position;
use style::computed_values::text_align::T as TextAlign;
use style::computed_values::text_justify::T as TextJustify;
use style::computed_values::text_wrap_mode::T as TextWrapMode;
use style::computed_values::white_space_collapse::T as WhiteSpaceCollapse;
use style::logical_geometry::{LogicalRect, LogicalSize, WritingMode};
use style::properties::ComputedValues;
use style::servo::restyle_damage::ServoRestyleDamage;
use style::values::computed::box_::VerticalAlign;
use style::values::generics::box_::VerticalAlignKeyword;
use style::values::specified::text::TextOverflowSide;
use unicode_bidi as bidi;

use crate::block::AbsoluteAssignBSizesTraversal;
use crate::context::LayoutContext;
use crate::display_list::items::{DisplayListSection, OpaqueNode};
use crate::display_list::{
    BorderPaintingMode, DisplayListBuildState, StackingContextCollectionState,
};
use crate::floats::{FloatKind, Floats, PlacementInfo};
use crate::flow::{
    BaseFlow, EarlyAbsolutePositionInfo, Flow, FlowClass, FlowFlags, ForceNonfloatedFlag,
    GetBaseFlow, OpaqueFlow,
};
use crate::flow_ref::FlowRef;
use crate::fragment::{
    CoordinateSystem, Fragment, FragmentBorderBoxIterator, FragmentFlags, Overflow,
    SpecificFragmentInfo,
};
use crate::model::IntrinsicISizesContribution;
use crate::traversal::PreorderFlowTraversal;
use crate::{ServoArc, text};

/// `Line`s are represented as offsets into the child list, rather than
/// as an object that "owns" fragments. Choosing a different set of line
/// breaks requires a new list of offsets, and possibly some splitting and
/// merging of TextFragments.
///
/// A similar list will keep track of the mapping between CSS fragments and
/// the corresponding fragments in the inline flow.
///
/// After line breaks are determined, render fragments in the inline flow may
/// overlap visually. For example, in the case of nested inline CSS fragments,
/// outer inlines must be at least as large as the inner inlines, for
/// purposes of drawing noninherited things like backgrounds, borders,
/// outlines.
///
/// N.B. roc has an alternative design where the list instead consists of
/// things like "start outer fragment, text, start inner fragment, text, end inner
/// fragment, text, end outer fragment, text". This seems a little complicated to
/// serve as the starting point, but the current design doesn't make it
/// hard to try out that alternative.
///
/// Line fragments also contain some metadata used during line breaking. The
/// green zone is the area that the line can expand to before it collides
/// with a float or a horizontal wall of the containing block. The block-start
/// inline-start corner of the green zone is the same as that of the line, but
/// the green zone can be taller and wider than the line itself.
#[derive(Clone, Debug, Serialize)]
pub struct Line {
    /// A range of line indices that describe line breaks.
    ///
    /// For example, consider the following HTML and rendered element with
    /// linebreaks:
    ///
    /// ~~~html
    /// <span>I <span>like truffles, <img></span> yes I do.</span>
    /// ~~~
    ///
    /// ~~~text
    /// +------------+
    /// | I like     |
    /// | truffles,  |
    /// | +----+     |
    /// | |    |     |
    /// | +----+ yes |
    /// | I do.      |
    /// +------------+
    /// ~~~
    ///
    /// The ranges that describe these lines would be:
    ///
    /// | [0, 2)   | [2, 3)      | [3, 5)      | [5, 6)   |
    /// |----------|-------------|-------------|----------|
    /// | 'I like' | 'truffles,' | '<img> yes' | 'I do.'  |
    pub range: Range<FragmentIndex>,

    /// The bidirectional embedding level runs for this line, in visual order.
    ///
    /// Can be set to `None` if the line is 100% left-to-right.
    pub visual_runs: Option<Vec<(Range<FragmentIndex>, bidi::Level)>>,

    /// The bounds are the exact position and extents of the line with respect
    /// to the parent box.
    ///
    /// For example, for the HTML below...
    ///
    /// ~~~html
    /// <div><span>I like      truffles, <img></span></div>
    /// ~~~
    ///
    /// ...the bounds would be:
    ///
    /// ~~~text
    /// +-----------------------------------------------------------+
    /// |               ^                                           |
    /// |               |                                           |
    /// |            origin.y                                       |
    /// |               |                                           |
    /// |               v                                           |
    /// |< - origin.x ->+ - - - - - - - - +---------+----           |
    /// |               |                 |         |   ^           |
    /// |               |                 |  <img>  |  size.block   |
    /// |               I like truffles,  |         |   v           |
    /// |               + - - - - - - - - +---------+----           |
    /// |               |                           |               |
    /// |               |<------ size.inline ------>|               |
    /// |                                                           |
    /// |                                                           |
    /// +-----------------------------------------------------------+
    /// ~~~
    pub bounds: LogicalRect<Au>,

    /// The green zone is the greatest extent from which a line can extend to
    /// before it collides with a float.
    ///
    /// ~~~text
    /// +-----------------------+
    /// |:::::::::::::::::      |
    /// |:::::::::::::::::FFFFFF|
    /// |============:::::FFFFFF|
    /// |:::::::::::::::::FFFFFF|
    /// |:::::::::::::::::FFFFFF|
    /// |:::::::::::::::::      |
    /// |    FFFFFFFFF          |
    /// |    FFFFFFFFF          |
    /// |    FFFFFFFFF          |
    /// |                       |
    /// +-----------------------+
    ///
    /// === line
    /// ::: green zone
    /// FFF float
    /// ~~~
    pub green_zone: LogicalSize<Au>,

    /// The minimum metrics for this line, as specified by the style.
    pub minimum_metrics: LineMetrics,

    /// The actual metrics for this line.
    pub metrics: LineMetrics,
}

impl Line {
    fn new(writing_mode: WritingMode, minimum_metrics: &LineMetrics) -> Line {
        Line {
            range: Range::empty(),
            visual_runs: None,
            bounds: LogicalRect::zero(writing_mode),
            green_zone: LogicalSize::zero(writing_mode),
            minimum_metrics: *minimum_metrics,
            metrics: *minimum_metrics,
        }
    }

    /// Returns the new metrics that this line would have if `new_fragment` were added to it.
    ///
    /// FIXME(pcwalton): this assumes that the tallest fragment in the line determines the line
    /// block-size. This might not be the case with some weird text fonts.
    fn new_metrics_for_fragment(
        &self,
        new_fragment: &Fragment,
        layout_context: &LayoutContext,
    ) -> LineMetrics {
        if !new_fragment.is_vertically_aligned_to_top_or_bottom() {
            let fragment_inline_metrics =
                new_fragment.aligned_inline_metrics(layout_context, &self.minimum_metrics, None);
            self.metrics
                .new_metrics_for_fragment(&fragment_inline_metrics)
        } else {
            self.metrics
        }
    }

    /// Returns the new block size that this line would have if `new_fragment` were added to it.
    /// `new_inline_metrics` represents the new inline metrics that this line would have; it can
    /// be computed with `new_inline_metrics()`.
    fn new_block_size_for_fragment(
        &self,
        new_fragment: &Fragment,
        new_line_metrics: &LineMetrics,
        layout_context: &LayoutContext,
    ) -> Au {
        let new_block_size = if new_fragment.is_vertically_aligned_to_top_or_bottom() {
            max(
                new_fragment
                    .aligned_inline_metrics(layout_context, &self.minimum_metrics, None)
                    .space_needed(),
                self.minimum_metrics.space_needed(),
            )
        } else {
            new_line_metrics.space_needed()
        };
        max(self.bounds.size.block, new_block_size)
    }
}

int_range_index! {
    #[derive(Serialize)]
    /// The index of a fragment in a flattened vector of DOM elements.
    struct FragmentIndex(isize)
}

/// Arranges fragments into lines, splitting them up as necessary.
struct LineBreaker {
    /// The floats we need to flow around.
    floats: Floats,
    /// The resulting fragment list for the flow, consisting of possibly-broken fragments.
    new_fragments: Vec<Fragment>,
    /// The next fragment or fragments that we need to work on.
    work_list: VecDeque<Fragment>,
    /// The line we're currently working on.
    pending_line: Line,
    /// The lines we've already committed.
    lines: Vec<Line>,
    /// The index of the last known good line breaking opportunity. The opportunity will either
    /// be inside this fragment (if it is splittable) or immediately prior to it.
    last_known_line_breaking_opportunity: Option<FragmentIndex>,
    /// The current position in the block direction.
    cur_b: Au,
    /// The computed value of the indentation for the first line (`text-indent`, CSS 2.1 § 16.1).
    first_line_indentation: Au,
    /// The minimum metrics for each line, as specified by the line height and font style.
    minimum_metrics: LineMetrics,
}

impl LineBreaker {
    /// Creates a new `LineBreaker` with a set of floats and the indentation of the first line.
    fn new(
        float_context: Floats,
        first_line_indentation: Au,
        minimum_line_metrics: &LineMetrics,
    ) -> LineBreaker {
        LineBreaker {
            new_fragments: Vec::new(),
            work_list: VecDeque::new(),
            pending_line: Line::new(float_context.writing_mode, minimum_line_metrics),
            floats: float_context,
            lines: Vec::new(),
            cur_b: Au(0),
            last_known_line_breaking_opportunity: None,
            first_line_indentation,
            minimum_metrics: *minimum_line_metrics,
        }
    }

    /// Resets the `LineBreaker` to the initial state it had after a call to `new`.
    fn reset_scanner(&mut self) {
        self.lines = Vec::new();
        self.new_fragments = Vec::new();
        self.cur_b = Au(0);
        self.reset_line();
    }

    /// Reinitializes the pending line to blank data.
    fn reset_line(&mut self) {
        self.last_known_line_breaking_opportunity = None;
        // https://github.com/rust-lang/rust/issues/49282
        self.pending_line = Line::new(self.floats.writing_mode, &self.minimum_metrics);
    }

    /// Reflows fragments for the given inline flow.
    fn scan_for_lines(&mut self, flow: &mut InlineFlow, layout_context: &LayoutContext) {
        self.reset_scanner();

        // Create our fragment iterator.
        debug!(
            "LineBreaker: scanning for lines, {} fragments",
            flow.fragments.len()
        );
        let mut old_fragments = mem::replace(&mut flow.fragments, InlineFragments::new());
        let old_fragment_iter = old_fragments.fragments.into_iter();

        // TODO(pcwalton): This would likely be better as a list of dirty line
        // indices. That way we could resynchronize if we discover during reflow
        // that all subsequent fragments must have the same position as they had
        // in the previous reflow. I don't know how common this case really is
        // in practice, but it's probably worth handling.
        self.lines = Vec::new();

        // Do the reflow.
        self.reflow_fragments(old_fragment_iter, flow, layout_context);

        // Perform unicode bidirectional layout.
        let para_level = flow.base.writing_mode.to_bidi_level();

        // The text within a fragment is at a single bidi embedding level
        // (because we split fragments on level run boundaries during flow
        // construction), so we can build a level array with just one entry per
        // fragment.
        let levels: Vec<bidi::Level> = self
            .new_fragments
            .iter()
            .map(|fragment| match fragment.specific {
                SpecificFragmentInfo::ScannedText(ref info) => info.run.bidi_level,
                _ => para_level,
            })
            .collect();

        let mut lines = mem::take(&mut self.lines);

        // If everything is LTR, don't bother with reordering.
        if bidi::level::has_rtl(&levels) {
            // Compute and store the visual ordering of the fragments within the
            // line.
            for line in &mut lines {
                let range = line.range.begin().to_usize()..line.range.end().to_usize();
                // FIXME: Update to use BidiInfo::visual_runs, as this algorithm needs access to
                // the original text and original BidiClass of its characters.
                #[allow(deprecated)]
                let runs = bidi::deprecated::visual_runs(range, &levels);
                line.visual_runs = Some(
                    runs.iter()
                        .map(|run| {
                            let start = FragmentIndex(run.start as isize);
                            let len = FragmentIndex(run.len() as isize);
                            (Range::new(start, len), levels[run.start])
                        })
                        .collect(),
                );
            }
        }

        // Place the fragments back into the flow.
        old_fragments.fragments = mem::take(&mut self.new_fragments);
        flow.fragments = old_fragments;
        flow.lines = lines;
    }

    /// Reflows the given fragments, which have been plucked out of the inline flow.
    fn reflow_fragments<I>(
        &mut self,
        mut old_fragment_iter: I,
        flow: &InlineFlow,
        layout_context: &LayoutContext,
    ) where
        I: Iterator<Item = Fragment>,
    {
        loop {
            // Acquire the next fragment to lay out from the work list or fragment list, as
            // appropriate.
            let fragment = match self.next_unbroken_fragment(&mut old_fragment_iter) {
                None => break,
                Some(fragment) => fragment,
            };

            // Do not reflow truncated fragments. Reflow the original fragment only.
            let fragment = if fragment.flags.contains(FragmentFlags::IS_ELLIPSIS) {
                continue;
            } else if let SpecificFragmentInfo::TruncatedFragment(info) = fragment.specific {
                info.full
            } else {
                fragment
            };

            // Try to append the fragment.
            self.reflow_fragment(fragment, flow, layout_context);
        }

        if !self.pending_line_is_empty() {
            debug!(
                "LineBreaker: partially full line {} at end of scanning; committing it",
                self.lines.len()
            );
            self.flush_current_line()
        }
    }

    /// Acquires a new fragment to lay out from the work list or fragment list as appropriate.
    /// Note that you probably don't want to call this method directly in order to be incremental-
    /// reflow-safe; try `next_unbroken_fragment` instead.
    fn next_fragment<I>(&mut self, old_fragment_iter: &mut I) -> Option<Fragment>
    where
        I: Iterator<Item = Fragment>,
    {
        self.work_list
            .pop_front()
            .or_else(|| old_fragment_iter.next())
    }

    /// Acquires a new fragment to lay out from the work list or fragment list,
    /// merging it with any subsequent fragments as appropriate. In effect, what
    /// this method does is to return the next fragment to lay out, undoing line
    /// break operations that any previous reflows may have performed. You
    /// probably want to be using this method instead of `next_fragment`.
    fn next_unbroken_fragment<I>(&mut self, old_fragment_iter: &mut I) -> Option<Fragment>
    where
        I: Iterator<Item = Fragment>,
    {
        let mut result = self.next_fragment(old_fragment_iter)?;

        loop {
            let candidate = match self.next_fragment(old_fragment_iter) {
                None => return Some(result),
                Some(fragment) => fragment,
            };

            let need_to_merge = match (&mut result.specific, &candidate.specific) {
                (
                    &mut SpecificFragmentInfo::ScannedText(ref mut result_info),
                    SpecificFragmentInfo::ScannedText(candidate_info),
                ) => {
                    result.margin.inline_end == Au(0) &&
                        candidate.margin.inline_start == Au(0) &&
                        result.border_padding.inline_end == Au(0) &&
                        candidate.border_padding.inline_start == Au(0) &&
                        result_info.selected() == candidate_info.selected() &&
                        Arc::ptr_eq(&result_info.run, &candidate_info.run) &&
                        inline_contexts_are_equal(
                            &result.inline_context,
                            &candidate.inline_context,
                        )
                },
                _ => false,
            };

            if need_to_merge {
                result.merge_with(candidate);
                continue;
            }

            self.work_list.push_front(candidate);
            return Some(result);
        }
    }

    /// Commits a line to the list.
    fn flush_current_line(&mut self) {
        debug!(
            "LineBreaker: flushing line {}: {:?}",
            self.lines.len(),
            self.pending_line
        );
        self.strip_trailing_whitespace_from_pending_line_if_necessary();
        self.lines.push(self.pending_line.clone());
        self.cur_b = self.pending_line.bounds.start.b + self.pending_line.bounds.size.block;
        self.reset_line();
    }

    /// Removes trailing whitespace from the pending line if necessary. This is done right before
    /// flushing it.
    fn strip_trailing_whitespace_from_pending_line_if_necessary(&mut self) {
        if self.pending_line.range.is_empty() {
            return;
        }
        let last_fragment_index = self.pending_line.range.end() - FragmentIndex(1);
        let fragment = &mut self.new_fragments[last_fragment_index.get() as usize];

        let old_fragment_inline_size = fragment.border_box.size.inline;

        fragment.strip_trailing_whitespace_if_necessary();

        self.pending_line.bounds.size.inline +=
            fragment.border_box.size.inline - old_fragment_inline_size;
    }

    /// Computes the position of a line that has only the provided fragment. Returns the bounding
    /// rect of the line's green zone (whose origin coincides with the line's origin) and the
    /// actual inline-size of the first fragment after splitting.
    fn initial_line_placement(
        &self,
        flow: &InlineFlow,
        first_fragment: &Fragment,
        ceiling: Au,
    ) -> (LogicalRect<Au>, Au) {
        debug!(
            "LineBreaker: trying to place first fragment of line {}; fragment size: {:?}, \
             splittable: {}",
            self.lines.len(),
            first_fragment.border_box.size,
            first_fragment.can_split()
        );

        // Initially, pretend a splittable fragment has zero inline-size. We will move it later if
        // it has nonzero inline-size and that causes problems.
        let placement_inline_size = if first_fragment.can_split() {
            first_fragment.minimum_splittable_inline_size()
        } else {
            first_fragment.margin_box_inline_size() + self.indentation_for_pending_fragment()
        };

        // Try to place the fragment between floats.
        let line_bounds = self.floats.place_between_floats(&PlacementInfo {
            size: LogicalSize::new(
                self.floats.writing_mode,
                placement_inline_size,
                first_fragment.border_box.size.block,
            ),
            ceiling,
            max_inline_size: flow.base.position.size.inline,
            kind: FloatKind::Left,
        });

        let fragment_margin_box_inline_size = first_fragment.margin_box_inline_size();

        // Simple case: if the fragment fits, then we can stop here.
        if line_bounds.size.inline > fragment_margin_box_inline_size {
            debug!("LineBreaker: fragment fits on line {}", self.lines.len());
            return (line_bounds, fragment_margin_box_inline_size);
        }

        // If not, but we can't split the fragment, then we'll place the line here and it will
        // overflow.
        if !first_fragment.can_split() {
            debug!("LineBreaker: line doesn't fit, but is unsplittable");
        }

        (line_bounds, fragment_margin_box_inline_size)
    }

    /// Performs float collision avoidance. This is called when adding a fragment is going to
    /// increase the block-size, and because of that we will collide with some floats.
    ///
    /// We have two options here:
    /// 1) Move the entire line so that it doesn't collide any more.
    /// 2) Break the line and put the new fragment on the next line.
    ///
    /// The problem with option 1 is that we might move the line and then wind up breaking anyway,
    /// which violates the standard. But option 2 is going to look weird sometimes.
    ///
    /// So we'll try to move the line whenever we can, but break if we have to.
    ///
    /// Returns false if and only if we should break the line.
    fn avoid_floats(
        &mut self,
        flow: &InlineFlow,
        in_fragment: Fragment,
        new_block_size: Au,
    ) -> bool {
        debug!("LineBreaker: entering float collision avoider!");

        // First predict where the next line is going to be.
        let (next_line, first_fragment_inline_size) =
            self.initial_line_placement(flow, &in_fragment, self.pending_line.bounds.start.b);
        let next_green_zone = next_line.size;

        let new_inline_size = self.pending_line.bounds.size.inline + first_fragment_inline_size;

        // Now, see if everything can fit at the new location.
        if next_green_zone.inline >= new_inline_size && next_green_zone.block >= new_block_size {
            debug!(
                "LineBreaker: case=adding fragment collides vertically with floats: moving \
                 line"
            );

            self.pending_line.bounds.start = next_line.start;
            self.pending_line.green_zone = next_green_zone;

            debug_assert!(
                !self.pending_line_is_empty(),
                "Non-terminating line breaking"
            );
            self.work_list.push_front(in_fragment);
            return true;
        }

        debug!("LineBreaker: case=adding fragment collides vertically with floats: breaking line");
        self.work_list.push_front(in_fragment);
        false
    }

    /// Tries to append the given fragment to the line, splitting it if necessary. Commits the
    /// current line if needed.
    fn reflow_fragment(
        &mut self,
        mut fragment: Fragment,
        flow: &InlineFlow,
        layout_context: &LayoutContext,
    ) {
        // Undo any whitespace stripping from previous reflows.
        fragment.reset_text_range_and_inline_size();
        // Determine initial placement for the fragment if we need to.
        //
        // Also, determine whether we can legally break the line before, or
        // inside, this fragment.
        let fragment_is_line_break_opportunity = if self.pending_line_is_empty() {
            fragment.strip_leading_whitespace_if_necessary();
            let (line_bounds, _) = self.initial_line_placement(flow, &fragment, self.cur_b);
            self.pending_line.bounds.start = line_bounds.start;
            self.pending_line.green_zone = line_bounds.size;
            false
        } else {
            // In case of Foo<span style="...">bar</span>, the line breaker will
            // set the "suppress line break before" flag for the second fragment.
            //
            // In case of Foo<span>bar</span> the second fragment ("bar") will
            // start _within_ a glyph run, so we also avoid breaking there
            //
            // is_on_glyph_run_boundary does a binary search, but this is ok
            // because the result will be cached and reused in
            // `calculate_split_position` later
            if fragment.suppress_line_break_before() || !fragment.is_on_glyph_run_boundary() {
                false
            } else {
                fragment.text_wrap_mode() == TextWrapMode::Wrap
            }
        };

        debug!(
            "LineBreaker: trying to append to line {} \
             (fragment size: {:?}, green zone: {:?}): {:?}",
            self.lines.len(),
            fragment.border_box.size,
            self.pending_line.green_zone,
            fragment
        );

        // NB: At this point, if `green_zone.inline <
        // self.pending_line.bounds.size.inline` or `green_zone.block <
        // self.pending_line.bounds.size.block`, then we committed a line that
        // overlaps with floats.
        let green_zone = self.pending_line.green_zone;
        let new_line_metrics = self
            .pending_line
            .new_metrics_for_fragment(&fragment, layout_context);
        let new_block_size = self.pending_line.new_block_size_for_fragment(
            &fragment,
            &new_line_metrics,
            layout_context,
        );
        if new_block_size > green_zone.block {
            // Uh-oh. Float collision imminent. Enter the float collision avoider!
            if !self.avoid_floats(flow, fragment, new_block_size) {
                self.flush_current_line();
            }
            return;
        }

        // Record the last known good line break opportunity if this is one.
        if fragment_is_line_break_opportunity {
            self.last_known_line_breaking_opportunity = Some(self.pending_line.range.end())
        }

        // If we must flush the line after finishing this fragment due to `white-space: pre`,
        // detect that.
        let line_flush_mode = if fragment.white_space_collapse() != WhiteSpaceCollapse::Collapse {
            if fragment.requires_line_break_afterward_if_wrapping_on_newlines() {
                LineFlushMode::Flush
            } else {
                LineFlushMode::No
            }
        } else {
            LineFlushMode::No
        };

        // If we're not going to overflow the green zone vertically, we might still do so
        // horizontally. We'll try to place the whole fragment on this line and break somewhere if
        // it doesn't fit.
        let indentation = self.indentation_for_pending_fragment();
        let new_inline_size =
            self.pending_line.bounds.size.inline + fragment.margin_box_inline_size() + indentation;
        if new_inline_size <= green_zone.inline {
            debug!("LineBreaker: fragment fits without splitting");
            self.push_fragment_to_line(layout_context, fragment, line_flush_mode);
            return;
        }

        // If the wrapping mode prevents us from splitting, then back up and split at the last
        // known good split point.
        if fragment.text_wrap_mode() == TextWrapMode::Nowrap {
            debug!(
                "LineBreaker: fragment can't split; falling back to last known good split point"
            );
            self.split_line_at_last_known_good_position(layout_context, fragment, line_flush_mode);
            return;
        }

        // Split it up!
        let available_inline_size =
            green_zone.inline - self.pending_line.bounds.size.inline - indentation;
        let inline_start_fragment;
        let inline_end_fragment;
        let split_result = match fragment
            .calculate_split_position(available_inline_size, self.pending_line_is_empty())
        {
            None => {
                // We failed to split. Defer to the next line if we're allowed to; otherwise,
                // rewind to the last line breaking opportunity.
                if fragment_is_line_break_opportunity {
                    debug!("LineBreaker: fragment was unsplittable; deferring to next line");
                    self.work_list.push_front(fragment);
                    self.flush_current_line();
                } else {
                    self.split_line_at_last_known_good_position(
                        layout_context,
                        fragment,
                        LineFlushMode::No,
                    );
                }
                return;
            },
            Some(split_result) => split_result,
        };

        inline_start_fragment = split_result
            .inline_start
            .as_ref()
            .map(|x| fragment.transform_with_split_info(x, split_result.text_run.clone(), true));
        inline_end_fragment = split_result
            .inline_end
            .as_ref()
            .map(|x| fragment.transform_with_split_info(x, split_result.text_run.clone(), false));

        // Push the first fragment onto the line we're working on and start off the next line with
        // the second fragment. If there's no second fragment, the next line will start off empty.
        match (inline_start_fragment, inline_end_fragment) {
            (Some(mut inline_start_fragment), Some(mut inline_end_fragment)) => {
                inline_start_fragment.border_padding.inline_end = Au(0);
                if let Some(ref mut inline_context) = inline_start_fragment.inline_context {
                    for node in &mut inline_context.nodes {
                        node.flags
                            .remove(InlineFragmentNodeFlags::LAST_FRAGMENT_OF_ELEMENT);
                    }
                }
                inline_start_fragment.border_box.size.inline +=
                    inline_start_fragment.border_padding.inline_start;

                inline_end_fragment.border_padding.inline_start = Au(0);
                if let Some(ref mut inline_context) = inline_end_fragment.inline_context {
                    for node in &mut inline_context.nodes {
                        node.flags
                            .remove(InlineFragmentNodeFlags::FIRST_FRAGMENT_OF_ELEMENT);
                    }
                }
                inline_end_fragment.border_box.size.inline +=
                    inline_end_fragment.border_padding.inline_end;

                self.push_fragment_to_line(
                    layout_context,
                    inline_start_fragment,
                    LineFlushMode::Flush,
                );
                self.work_list.push_front(inline_end_fragment)
            },
            (Some(fragment), None) => {
                self.push_fragment_to_line(layout_context, fragment, line_flush_mode);
            },
            (None, Some(fragment)) => {
                // Yes, this can happen!
                self.flush_current_line();
                self.work_list.push_front(fragment)
            },
            (None, None) => {},
        }
    }

    /// Pushes a fragment to the current line unconditionally, possibly truncating it and placing
    /// an ellipsis based on the value of `text-overflow`. If `flush_line` is `Flush`, then flushes
    /// the line afterward;
    fn push_fragment_to_line(
        &mut self,
        layout_context: &LayoutContext,
        fragment: Fragment,
        line_flush_mode: LineFlushMode,
    ) {
        let indentation = self.indentation_for_pending_fragment();
        if self.pending_line_is_empty() {
            debug_assert!(self.new_fragments.len() <= (isize::MAX as usize));
            self.pending_line.range.reset(
                FragmentIndex(self.new_fragments.len() as isize),
                FragmentIndex(0),
            );
        }

        // Determine if an ellipsis will be necessary to account for `text-overflow`.
        let available_inline_size = self.pending_line.green_zone.inline -
            self.pending_line.bounds.size.inline -
            indentation;

        let ellipsis = match (
            &fragment.style().get_text().text_overflow.second,
            fragment.style().get_box().overflow_x,
        ) {
            (&TextOverflowSide::Clip, _) | (_, StyleOverflow::Visible) => None,
            (&TextOverflowSide::Ellipsis, _) => {
                if fragment.margin_box_inline_size() > available_inline_size {
                    Some("…".to_string())
                } else {
                    None
                }
            },
            (TextOverflowSide::String(string), _) => {
                if fragment.margin_box_inline_size() > available_inline_size {
                    Some(string.to_string())
                } else {
                    None
                }
            },
        };

        if let Some(string) = ellipsis {
            let ellipsis = fragment.transform_into_ellipsis(layout_context, string);
            let truncated = fragment
                .truncate_to_inline_size(available_inline_size - ellipsis.margin_box_inline_size());
            self.push_fragment_to_line_ignoring_text_overflow(truncated, layout_context);
            self.push_fragment_to_line_ignoring_text_overflow(ellipsis, layout_context);
        } else {
            self.push_fragment_to_line_ignoring_text_overflow(fragment, layout_context);
        }

        if line_flush_mode == LineFlushMode::Flush {
            self.flush_current_line()
        }
    }

    /// Pushes a fragment to the current line unconditionally, without placing an ellipsis in the
    /// case of `text-overflow: ellipsis`.
    fn push_fragment_to_line_ignoring_text_overflow(
        &mut self,
        fragment: Fragment,
        layout_context: &LayoutContext,
    ) {
        let indentation = self.indentation_for_pending_fragment();
        self.pending_line.range.extend_by(FragmentIndex(1));

        if !fragment.is_inline_absolute() && !fragment.is_hypothetical() {
            self.pending_line.bounds.size.inline = self.pending_line.bounds.size.inline +
                fragment.margin_box_inline_size() +
                indentation;
            self.pending_line.metrics = self
                .pending_line
                .new_metrics_for_fragment(&fragment, layout_context);
            self.pending_line.bounds.size.block = self.pending_line.new_block_size_for_fragment(
                &fragment,
                &self.pending_line.metrics,
                layout_context,
            );
        }

        self.new_fragments.push(fragment);
    }

    fn split_line_at_last_known_good_position(
        &mut self,
        layout_context: &LayoutContext,
        cur_fragment: Fragment,
        line_flush_mode: LineFlushMode,
    ) {
        let last_known_line_breaking_opportunity = match self.last_known_line_breaking_opportunity {
            None => {
                // No line breaking opportunity exists at all for this line. Overflow.
                self.push_fragment_to_line(layout_context, cur_fragment, line_flush_mode);
                return;
            },
            Some(last_known_line_breaking_opportunity) => last_known_line_breaking_opportunity,
        };

        self.work_list.push_front(cur_fragment);
        for fragment_index in
            (last_known_line_breaking_opportunity.get()..self.pending_line.range.end().get()).rev()
        {
            debug_assert_eq!(fragment_index, (self.new_fragments.len() as isize) - 1);
            self.work_list.push_front(self.new_fragments.pop().unwrap());
        }

        // FIXME(pcwalton): This should actually attempt to split the last fragment if
        // possible to do so, to handle cases like:
        //
        //     (available width)
        //     +-------------+
        //     The alphabet
        //     (<em>abcdefghijklmnopqrstuvwxyz</em>)
        //
        // Here, the last known-good split point is inside the fragment containing
        // "The alphabet (", which has already been committed by the time we get to this
        // point. Unfortunately, the existing splitting API (`calculate_split_position`)
        // has no concept of "split right before the last non-whitespace position". We'll
        // need to add that feature to the API to handle this case correctly.
        self.pending_line
            .range
            .extend_to(last_known_line_breaking_opportunity);
        self.flush_current_line();
    }

    /// Returns the indentation that needs to be applied before the fragment we're reflowing.
    fn indentation_for_pending_fragment(&self) -> Au {
        if self.pending_line_is_empty() && self.lines.is_empty() {
            self.first_line_indentation
        } else {
            Au(0)
        }
    }

    /// Returns true if the pending line is empty and false otherwise.
    fn pending_line_is_empty(&self) -> bool {
        self.pending_line.range.length() == FragmentIndex(0)
    }
}

/// Represents a list of inline fragments, including element ranges.
#[derive(Clone, Serialize)]
pub struct InlineFragments {
    /// The fragments themselves.
    pub fragments: Vec<Fragment>,
}

impl InlineFragments {
    /// Creates an empty set of inline fragments.
    pub fn new() -> InlineFragments {
        InlineFragments { fragments: vec![] }
    }

    /// Returns the number of inline fragments.
    pub fn len(&self) -> usize {
        self.fragments.len()
    }

    /// Returns true if this list contains no fragments and false if it contains at least one
    /// fragment.
    pub fn is_empty(&self) -> bool {
        self.fragments.is_empty()
    }

    /// A convenience function to return the fragment at a given index.
    pub fn get(&self, index: usize) -> &Fragment {
        &self.fragments[index]
    }

    /// A convenience function to return a mutable reference to the fragment at a given index.
    pub fn get_mut(&mut self, index: usize) -> &mut Fragment {
        &mut self.fragments[index]
    }
}

impl Default for InlineFragments {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(unsafe_code)]
unsafe impl crate::flow::HasBaseFlow for InlineFlow {}

/// Flows for inline layout.
#[derive(Serialize)]
#[repr(C)]
pub struct InlineFlow {
    /// Data common to all flows.
    pub base: BaseFlow,

    /// A vector of all inline fragments. Several fragments may correspond to one node/element.
    pub fragments: InlineFragments,

    /// A vector of ranges into fragments that represents line positions. These ranges are disjoint
    /// and are the result of inline layout. This also includes some metadata used for positioning
    /// lines.
    pub lines: Vec<Line>,

    /// The minimum metrics for each line, as specified by the line height and font style.
    pub minimum_line_metrics: LineMetrics,

    /// The amount of indentation to use on the first line. This is determined by our block parent
    /// (because percentages are relative to the containing block, and we aren't in a position to
    /// compute things relative to our parent's containing block).
    pub first_line_indentation: Au,
}

impl InlineFlow {
    pub fn from_fragments(fragments: InlineFragments, writing_mode: WritingMode) -> InlineFlow {
        let mut flow = InlineFlow {
            base: BaseFlow::new(None, writing_mode, ForceNonfloatedFlag::ForceNonfloated),
            fragments,
            lines: Vec::new(),
            minimum_line_metrics: LineMetrics::new(Au(0), Au(0)),
            first_line_indentation: Au(0),
        };

        if flow
            .fragments
            .fragments
            .iter()
            .any(Fragment::is_unscanned_generated_content)
        {
            flow.base
                .restyle_damage
                .insert(ServoRestyleDamage::RESOLVE_GENERATED_CONTENT);
        }

        flow
    }

    /// Sets fragment positions in the inline direction based on alignment for one line. This
    /// performs text justification if mandated by the style.
    fn set_inline_fragment_positions(
        fragments: &mut InlineFragments,
        line: &Line,
        line_align: TextAlign,
        indentation: Au,
        is_last_line: bool,
    ) {
        // Figure out how much inline-size we have.
        let slack_inline_size = max(Au(0), line.green_zone.inline - line.bounds.size.inline);

        // Compute the value we're going to use for `text-justify`.
        if fragments.fragments.is_empty() {
            return;
        }
        let text_justify = fragments.fragments[0]
            .style()
            .get_inherited_text()
            .text_justify;

        // Translate `left` and `right` to logical directions.
        let is_ltr = fragments.fragments[0].style().writing_mode.is_bidi_ltr();
        let line_align = match (line_align, is_ltr) {
            (TextAlign::Left, true) |
            (TextAlign::MozLeft, true) |
            (TextAlign::Right, false) |
            (TextAlign::MozRight, false) => TextAlign::Start,
            (TextAlign::Left, false) |
            (TextAlign::MozLeft, false) |
            (TextAlign::Right, true) |
            (TextAlign::MozRight, true) => TextAlign::End,
            _ => line_align,
        };

        // Set the fragment inline positions based on that alignment, and justify the text if
        // necessary.
        let mut inline_start_position_for_fragment = line.bounds.start.i + indentation;
        match line_align {
            TextAlign::Justify if !is_last_line && text_justify != TextJustify::None => {
                InlineFlow::justify_inline_fragments(fragments, line, slack_inline_size)
            },
            TextAlign::Justify | TextAlign::Start => {},
            TextAlign::Center | TextAlign::MozCenter => {
                inline_start_position_for_fragment += slack_inline_size.scale_by(0.5)
            },
            TextAlign::End => inline_start_position_for_fragment += slack_inline_size,
            TextAlign::Left | TextAlign::MozLeft | TextAlign::Right | TextAlign::MozRight => {
                unreachable!()
            },
        }

        // Lay out the fragments in visual order.
        let run_count = match line.visual_runs {
            Some(ref runs) => runs.len(),
            None => 1,
        };
        for run_idx in 0..run_count {
            let (range, level) = match line.visual_runs {
                Some(ref runs) if is_ltr => runs[run_idx],
                Some(ref runs) => runs[run_count - run_idx - 1], // reverse order for RTL runs
                None => (line.range, bidi::Level::ltr()),
            };

            struct MaybeReverse<I> {
                iter: I,
                reverse: bool,
            }

            impl<I: DoubleEndedIterator> Iterator for MaybeReverse<I> {
                type Item = I::Item;

                fn next(&mut self) -> Option<I::Item> {
                    if self.reverse {
                        self.iter.next_back()
                    } else {
                        self.iter.next()
                    }
                }

                fn size_hint(&self) -> (usize, Option<usize>) {
                    self.iter.size_hint()
                }
            }

            // If the bidi embedding direction is opposite the layout direction, lay out this
            // run in reverse order.
            let fragment_indices = MaybeReverse {
                iter: range.begin().get()..range.end().get(),
                reverse: level.is_ltr() != is_ltr,
            };

            for fragment_index in fragment_indices {
                let fragment = fragments.get_mut(fragment_index as usize);
                inline_start_position_for_fragment += fragment.margin.inline_start;

                let border_start = if fragment.style.writing_mode.is_bidi_ltr() == is_ltr {
                    inline_start_position_for_fragment
                } else {
                    line.green_zone.inline -
                        inline_start_position_for_fragment -
                        fragment.margin.inline_end -
                        fragment.border_box.size.inline
                };
                fragment.border_box = LogicalRect::new(
                    fragment.style.writing_mode,
                    border_start,
                    fragment.border_box.start.b,
                    fragment.border_box.size.inline,
                    fragment.border_box.size.block,
                );
                fragment.update_late_computed_inline_position_if_necessary();

                if !fragment.is_inline_absolute() {
                    inline_start_position_for_fragment = inline_start_position_for_fragment +
                        fragment.border_box.size.inline +
                        fragment.margin.inline_end;
                }
            }
        }
    }

    /// Justifies the given set of inline fragments, distributing the `slack_inline_size` among all
    /// of them according to the value of `text-justify`.
    fn justify_inline_fragments(
        fragments: &mut InlineFragments,
        line: &Line,
        slack_inline_size: Au,
    ) {
        // Fast path.
        if slack_inline_size == Au(0) {
            return;
        }

        // First, calculate the number of expansion opportunities (spaces, normally).
        let mut expansion_opportunities = 0;
        for fragment_index in line.range.each_index() {
            let fragment = fragments.get(fragment_index.to_usize());
            let scanned_text_fragment_info = match fragment.specific {
                SpecificFragmentInfo::ScannedText(ref info) if !info.range.is_empty() => info,
                _ => continue,
            };
            let fragment_range = scanned_text_fragment_info.range;

            for slice in scanned_text_fragment_info
                .run
                .character_slices_in_range(&fragment_range)
            {
                expansion_opportunities += slice.glyphs.word_separator_count_in_range(&slice.range)
            }
        }

        if expansion_opportunities == 0 {
            return;
        }

        // Then distribute all the space across the expansion opportunities.
        let space_per_expansion_opportunity = slack_inline_size / expansion_opportunities as i32;
        for fragment_index in line.range.each_index() {
            let fragment = fragments.get_mut(fragment_index.to_usize());
            let scanned_text_fragment_info = match fragment.specific {
                SpecificFragmentInfo::ScannedText(ref mut info) if !info.range.is_empty() => info,
                _ => continue,
            };
            let fragment_range = scanned_text_fragment_info.range;
            let run = Arc::make_mut(&mut scanned_text_fragment_info.run);
            run.extra_word_spacing = space_per_expansion_opportunity;

            // Recompute the fragment's border box size.
            let new_inline_size = run.advance_for_range(&fragment_range);
            let new_size = LogicalSize::new(
                fragment.style.writing_mode,
                new_inline_size,
                fragment.border_box.size.block,
            );
            fragment.border_box = LogicalRect::from_point_size(
                fragment.style.writing_mode,
                fragment.border_box.start,
                new_size,
            );
        }
    }

    /// Sets final fragment positions in the block direction for one line.
    fn set_block_fragment_positions(
        fragments: &mut InlineFragments,
        line: &Line,
        minimum_line_metrics: &LineMetrics,
        layout_context: &LayoutContext,
    ) {
        for fragment_index in line.range.each_index() {
            let fragment = fragments.get_mut(fragment_index.to_usize());
            let line_metrics = LineMetrics::for_line_and_fragment(line, fragment, layout_context);
            let inline_metrics = fragment.aligned_inline_metrics(
                layout_context,
                minimum_line_metrics,
                Some(&line_metrics),
            );

            // Align the top of the fragment's border box with its ascent above the baseline.
            fragment.border_box.start.b =
                line.bounds.start.b + line_metrics.space_above_baseline - inline_metrics.ascent;

            // CSS 2.1 § 10.8: "The height of each inline-level box in the line box is
            // calculated. For replaced elements, inline-block elements, and inline-table
            // elements, this is the height of their margin box; for inline boxes, this is their
            // 'line-height'."
            //
            // CSS 2.1 § 10.8.1: "Although margins, borders, and padding of non-replaced elements
            // do not enter into the line box calculation, they are still rendered around inline
            // boxes."
            //
            // Effectively, if the fragment is a non-replaced element (excluding inline-block), we
            // need to align its ascent above the baseline with the top of the *content box*, not
            // the border box. Since the code above has already aligned it to the border box, we
            // simply need to adjust it in this case.
            if !fragment.is_replaced_or_inline_block() {
                fragment.border_box.start.b -= fragment.border_padding.block_start
            }

            fragment.update_late_computed_block_position_if_necessary();
        }
    }

    /// Computes the minimum metrics for each line. This is done during flow construction.
    ///
    /// `style` is the style of the block.
    pub fn minimum_line_metrics(
        &self,
        font_context: &FontContext,
        style: &ComputedValues,
    ) -> LineMetrics {
        InlineFlow::minimum_line_metrics_for_fragments(
            &self.fragments.fragments,
            font_context,
            style,
        )
    }

    /// Computes the minimum line metrics for the given fragments. This is typically done during
    /// flow construction.
    ///
    /// `style` is the style of the block that these fragments belong to.
    pub fn minimum_line_metrics_for_fragments(
        fragments: &[Fragment],
        font_context: &FontContext,
        style: &ComputedValues,
    ) -> LineMetrics {
        // As a special case, if this flow contains only hypothetical fragments, then the entire
        // flow is hypothetical and takes up no space. See CSS 2.1 § 10.3.7.
        if fragments.iter().all(Fragment::is_hypothetical) {
            return LineMetrics::new(Au(0), Au(0));
        }

        let font_style = style.clone_font();
        let font_metrics = text::font_metrics_for_style(font_context, font_style);
        let line_height = text::line_height_from_style(style, &font_metrics);
        let inline_metrics = if fragments.iter().any(Fragment::is_text_or_replaced) {
            InlineMetrics::from_font_metrics(&font_metrics, line_height)
        } else {
            InlineMetrics::new(Au(0), Au(0), Au(0))
        };

        let mut line_metrics = LineMetrics::new(Au(0), MIN_AU);
        let mut largest_block_size_for_top_fragments = Au(0);
        let mut largest_block_size_for_bottom_fragments = Au(0);

        // We use `VerticalAlign::baseline()` here because `vertical-align` must
        // not apply to the inside of inline blocks.
        update_line_metrics_for_fragment(
            &mut line_metrics,
            &inline_metrics,
            style.get_box().display,
            &VerticalAlign::baseline(),
            &mut largest_block_size_for_top_fragments,
            &mut largest_block_size_for_bottom_fragments,
        );

        // According to CSS 2.1 § 10.8, `line-height` of any inline element specifies the minimal
        // height of line boxes within the element.
        for inline_context in fragments
            .iter()
            .filter_map(|fragment| fragment.inline_context.as_ref())
        {
            for node in &inline_context.nodes {
                let font_style = node.style.clone_font();
                let font_metrics = text::font_metrics_for_style(font_context, font_style);
                let line_height = text::line_height_from_style(&node.style, &font_metrics);
                let inline_metrics = InlineMetrics::from_font_metrics(&font_metrics, line_height);

                update_line_metrics_for_fragment(
                    &mut line_metrics,
                    &inline_metrics,
                    node.style.get_box().display,
                    &node.style.get_box().vertical_align,
                    &mut largest_block_size_for_top_fragments,
                    &mut largest_block_size_for_bottom_fragments,
                );
            }
        }

        line_metrics.space_above_baseline = max(
            line_metrics.space_above_baseline,
            largest_block_size_for_bottom_fragments - max(line_metrics.space_below_baseline, Au(0)),
        );
        line_metrics.space_below_baseline = max(
            line_metrics.space_below_baseline,
            largest_block_size_for_top_fragments - line_metrics.space_above_baseline,
        );

        return line_metrics;

        fn update_line_metrics_for_fragment(
            line_metrics: &mut LineMetrics,
            inline_metrics: &InlineMetrics,
            display_value: Display,
            vertical_align_value: &VerticalAlign,
            largest_block_size_for_top_fragments: &mut Au,
            largest_block_size_for_bottom_fragments: &mut Au,
        ) {
            // FIXME(emilio): This should probably be handled.
            let vertical_align_value = match vertical_align_value {
                VerticalAlign::Keyword(kw) => kw,
                VerticalAlign::Length(..) => {
                    *line_metrics = line_metrics.new_metrics_for_fragment(inline_metrics);
                    return;
                },
            };

            match (display_value, vertical_align_value) {
                (Display::Inline, VerticalAlignKeyword::Top) |
                (Display::Block, VerticalAlignKeyword::Top) |
                (Display::InlineFlex, VerticalAlignKeyword::Top) |
                (Display::InlineBlock, VerticalAlignKeyword::Top)
                    if inline_metrics.space_above_baseline >= Au(0) =>
                {
                    *largest_block_size_for_top_fragments = max(
                        *largest_block_size_for_top_fragments,
                        inline_metrics.space_above_baseline + inline_metrics.space_below_baseline,
                    )
                },
                (Display::Inline, VerticalAlignKeyword::Bottom) |
                (Display::Block, VerticalAlignKeyword::Bottom) |
                (Display::InlineFlex, VerticalAlignKeyword::Bottom) |
                (Display::InlineBlock, VerticalAlignKeyword::Bottom)
                    if inline_metrics.space_below_baseline >= Au(0) =>
                {
                    *largest_block_size_for_bottom_fragments = max(
                        *largest_block_size_for_bottom_fragments,
                        inline_metrics.space_above_baseline + inline_metrics.space_below_baseline,
                    )
                },
                _ => *line_metrics = line_metrics.new_metrics_for_fragment(inline_metrics),
            }
        }
    }

    fn update_restyle_damage(&mut self) {
        let mut damage = self.base.restyle_damage;

        for frag in &self.fragments.fragments {
            damage.insert(frag.restyle_damage());
        }

        self.base.restyle_damage = damage;
    }

    fn containing_block_range_for_flow_surrounding_fragment_at_index(
        &self,
        fragment_index: FragmentIndex,
    ) -> Range<FragmentIndex> {
        let mut start_index = fragment_index;
        while start_index > FragmentIndex(0) &&
            self.fragments.fragments[(start_index - FragmentIndex(1)).get() as usize]
                .is_positioned()
        {
            start_index = start_index - FragmentIndex(1)
        }

        let mut end_index = fragment_index + FragmentIndex(1);
        while end_index < FragmentIndex(self.fragments.fragments.len() as isize) &&
            self.fragments.fragments[end_index.get() as usize].is_positioned()
        {
            end_index = end_index + FragmentIndex(1)
        }

        Range::new(start_index, end_index - start_index)
    }

    fn containing_block_range_for_flow(&self, opaque_flow: OpaqueFlow) -> Range<FragmentIndex> {
        match self
            .fragments
            .fragments
            .iter()
            .position(|fragment| match fragment.specific {
                SpecificFragmentInfo::InlineAbsolute(ref inline_absolute) => {
                    OpaqueFlow::from_flow(&*inline_absolute.flow_ref) == opaque_flow
                },
                SpecificFragmentInfo::InlineAbsoluteHypothetical(
                    ref inline_absolute_hypothetical,
                ) => OpaqueFlow::from_flow(&*inline_absolute_hypothetical.flow_ref) == opaque_flow,
                _ => false,
            }) {
            Some(index) => {
                let index = FragmentIndex(index as isize);
                self.containing_block_range_for_flow_surrounding_fragment_at_index(index)
            },
            None => {
                // FIXME(pcwalton): This is quite wrong. We should only return the range
                // surrounding the inline fragments that constitute the containing block. But this
                // suffices to get Google looking right.
                Range::new(
                    FragmentIndex(0),
                    FragmentIndex(self.fragments.fragments.len() as isize),
                )
            },
        }
    }

    pub fn baseline_offset_of_last_line(&self) -> Option<Au> {
        self.last_line_containing_real_fragments().map(|line| {
            line.bounds.start.b + line.bounds.size.block - line.metrics.space_below_baseline
        })
    }

    // Returns the last line that doesn't consist entirely of hypothetical boxes.
    fn last_line_containing_real_fragments(&self) -> Option<&Line> {
        self.lines.iter().rev().find(|&line| {
            (line.range.begin().get()..line.range.end().get())
                .any(|index| !self.fragments.fragments[index as usize].is_hypothetical())
        })
    }

    fn build_display_list_for_inline_fragment_at_index(
        &mut self,
        state: &mut DisplayListBuildState,
        index: usize,
    ) {
        let fragment = self.fragments.fragments.get_mut(index).unwrap();
        let stacking_relative_border_box = self
            .base
            .stacking_relative_border_box_for_display_list(fragment);
        fragment.build_display_list(
            state,
            stacking_relative_border_box,
            BorderPaintingMode::Separate,
            DisplayListSection::Content,
            self.base.clip,
            None,
        );
    }
}

impl Flow for InlineFlow {
    fn class(&self) -> FlowClass {
        FlowClass::Inline
    }

    fn as_inline(&self) -> &InlineFlow {
        self
    }

    fn as_mut_inline(&mut self) -> &mut InlineFlow {
        self
    }

    fn bubble_inline_sizes(&mut self) {
        self.update_restyle_damage();

        let writing_mode = self.base.writing_mode;
        for kid in self.base.child_iter_mut() {
            kid.mut_base().floats = Floats::new(writing_mode);
        }

        self.base
            .flags
            .remove(FlowFlags::CONTAINS_TEXT_OR_REPLACED_FRAGMENTS);

        let mut intrinsic_sizes_for_flow = IntrinsicISizesContribution::new();
        let mut intrinsic_sizes_for_inline_run = IntrinsicISizesContribution::new();
        let mut intrinsic_sizes_for_nonbroken_run = IntrinsicISizesContribution::new();
        for fragment in &mut self.fragments.fragments {
            let intrinsic_sizes_for_fragment = fragment.compute_intrinsic_inline_sizes().finish();
            let style_text = fragment.style.get_inherited_text();
            match (style_text.white_space_collapse, style_text.text_wrap_mode) {
                (WhiteSpaceCollapse::Collapse, TextWrapMode::Nowrap) => {
                    intrinsic_sizes_for_nonbroken_run
                        .union_nonbreaking_inline(&intrinsic_sizes_for_fragment)
                },
                (
                    WhiteSpaceCollapse::Preserve |
                    WhiteSpaceCollapse::PreserveBreaks |
                    WhiteSpaceCollapse::BreakSpaces,
                    TextWrapMode::Nowrap,
                ) => {
                    intrinsic_sizes_for_nonbroken_run
                        .union_nonbreaking_inline(&intrinsic_sizes_for_fragment);

                    // Flush the intrinsic sizes we've been gathering up in order to handle the
                    // line break, if necessary.
                    if fragment.requires_line_break_afterward_if_wrapping_on_newlines() {
                        intrinsic_sizes_for_inline_run
                            .union_inline(&intrinsic_sizes_for_nonbroken_run.finish());
                        intrinsic_sizes_for_nonbroken_run = IntrinsicISizesContribution::new();
                        intrinsic_sizes_for_flow
                            .union_block(&intrinsic_sizes_for_inline_run.finish());
                        intrinsic_sizes_for_inline_run = IntrinsicISizesContribution::new();
                    }
                },
                (
                    WhiteSpaceCollapse::Preserve |
                    WhiteSpaceCollapse::PreserveBreaks |
                    WhiteSpaceCollapse::BreakSpaces,
                    TextWrapMode::Wrap,
                ) => {
                    // Flush the intrinsic sizes we were gathering up for the nonbroken run, if
                    // necessary.
                    intrinsic_sizes_for_inline_run
                        .union_inline(&intrinsic_sizes_for_nonbroken_run.finish());
                    intrinsic_sizes_for_nonbroken_run = IntrinsicISizesContribution::new();

                    intrinsic_sizes_for_nonbroken_run.union_inline(&intrinsic_sizes_for_fragment);

                    // Flush the intrinsic sizes we've been gathering up in order to handle the
                    // line break, if necessary.
                    if fragment.requires_line_break_afterward_if_wrapping_on_newlines() {
                        intrinsic_sizes_for_inline_run
                            .union_inline(&intrinsic_sizes_for_nonbroken_run.finish());
                        intrinsic_sizes_for_nonbroken_run = IntrinsicISizesContribution::new();
                        intrinsic_sizes_for_flow
                            .union_block(&intrinsic_sizes_for_inline_run.finish());
                        intrinsic_sizes_for_inline_run = IntrinsicISizesContribution::new();
                    }
                },
                (WhiteSpaceCollapse::Collapse, TextWrapMode::Wrap) => {
                    // Flush the intrinsic sizes we were gathering up for the nonbroken run, if
                    // necessary.
                    intrinsic_sizes_for_inline_run
                        .union_inline(&intrinsic_sizes_for_nonbroken_run.finish());
                    intrinsic_sizes_for_nonbroken_run = IntrinsicISizesContribution::new();

                    intrinsic_sizes_for_nonbroken_run.union_inline(&intrinsic_sizes_for_fragment);
                },
            }

            fragment
                .restyle_damage
                .remove(ServoRestyleDamage::BUBBLE_ISIZES);

            if fragment.is_text_or_replaced() {
                self.base
                    .flags
                    .insert(FlowFlags::CONTAINS_TEXT_OR_REPLACED_FRAGMENTS);
            }
        }

        // Flush any remaining nonbroken-run and inline-run intrinsic sizes.
        intrinsic_sizes_for_inline_run.union_inline(&intrinsic_sizes_for_nonbroken_run.finish());
        intrinsic_sizes_for_flow.union_block(&intrinsic_sizes_for_inline_run.finish());

        // Finish up the computation.
        self.base.intrinsic_inline_sizes = intrinsic_sizes_for_flow.finish()
    }

    /// Recursively (top-down) determines the actual inline-size of child contexts and fragments.
    /// When called on this context, the context has had its inline-size set by the parent context.
    fn assign_inline_sizes(&mut self, _: &LayoutContext) {
        // Initialize content fragment inline-sizes if they haven't been initialized already.
        //
        // TODO: Combine this with `LineBreaker`'s walk in the fragment list, or put this into
        // `Fragment`.

        debug!(
            "InlineFlow::assign_inline_sizes: floats in: {:?}",
            self.base.floats
        );

        let inline_size = self.base.block_container_inline_size;
        let container_mode = self.base.block_container_writing_mode;
        let container_block_size = self.base.block_container_explicit_block_size;
        self.base.position.size.inline = inline_size;

        {
            let this = &mut *self;
            for fragment in this.fragments.fragments.iter_mut() {
                fragment.compute_border_and_padding(inline_size);
                fragment.compute_block_direction_margins(inline_size);
                fragment.compute_inline_direction_margins(inline_size);
                fragment
                    .assign_replaced_inline_size_if_necessary(inline_size, container_block_size);
            }
        }

        // If there are any inline-block kids, propagate explicit block and inline
        // sizes down to them.
        let block_container_explicit_block_size = self.base.block_container_explicit_block_size;
        for kid in self.base.child_iter_mut() {
            let kid_base = kid.mut_base();

            kid_base.block_container_inline_size = inline_size;
            kid_base.block_container_writing_mode = container_mode;
            kid_base.block_container_explicit_block_size = block_container_explicit_block_size;
        }
    }

    /// Calculate and set the block-size of this flow. See CSS 2.1 § 10.6.1.
    /// Note that we do not need to do in-order traversal because the children
    /// are always block formatting context.
    fn assign_block_size(&mut self, layout_context: &LayoutContext) {
        // Divide the fragments into lines.
        //
        // TODO(pcwalton, #226): Get the CSS `line-height` property from the
        // style of the containing block to determine the minimum line block
        // size.
        //
        // TODO(pcwalton, #226): Get the CSS `line-height` property from each
        // non-replaced inline element to determine its block-size for computing
        // the line's own block-size.
        //
        // TODO(pcwalton): Cache the line scanner?
        debug!(
            "assign_block_size_inline: floats in: {:?}",
            self.base.floats
        );

        // Assign the block-size and late-computed inline-sizes for the inline fragments.
        for fragment in &mut self.fragments.fragments {
            fragment.update_late_computed_replaced_inline_size_if_necessary();
            fragment.assign_replaced_block_size_if_necessary();
        }

        // Reset our state, so that we handle incremental reflow correctly.
        //
        // TODO(pcwalton): Do something smarter, like Gecko and WebKit?
        self.lines.clear();

        // Determine how much indentation the first line wants.
        let mut indentation = if self.fragments.is_empty() {
            Au(0)
        } else {
            self.first_line_indentation
        };

        // Perform line breaking.
        let mut scanner = LineBreaker::new(
            self.base.floats.clone(),
            indentation,
            &self.minimum_line_metrics,
        );
        scanner.scan_for_lines(self, layout_context);

        // Now, go through each line and lay out the fragments inside.
        let line_count = self.lines.len();
        for (line_index, line) in self.lines.iter_mut().enumerate() {
            // Lay out fragments in the inline direction, and justify them if
            // necessary.
            InlineFlow::set_inline_fragment_positions(
                &mut self.fragments,
                line,
                self.base.text_align,
                indentation,
                line_index + 1 == line_count,
            );

            // Compute the final positions in the block direction of each fragment.
            InlineFlow::set_block_fragment_positions(
                &mut self.fragments,
                line,
                &self.minimum_line_metrics,
                layout_context,
            );

            // This is used to set the block-start position of the next line in
            // the next iteration of the loop. We're no longer on the first
            // line, so set indentation to zero.
            indentation = Au(0)
        }

        if self.is_absolute_containing_block() {
            // Assign block-sizes for all flows in this absolute flow tree.
            // This is preorder because the block-size of an absolute flow may depend on
            // the block-size of its containing block, which may also be an absolute flow.
            let assign_abs_b_sizes = AbsoluteAssignBSizesTraversal(layout_context.shared_context());
            assign_abs_b_sizes.traverse_absolute_flows(&mut *self);
        }

        self.base.position.size.block = match self.last_line_containing_real_fragments() {
            Some(last_line) => last_line.bounds.start.b + last_line.bounds.size.block,
            None => Au(0),
        };

        self.base.floats = scanner.floats;
        let writing_mode = self.base.floats.writing_mode;
        self.base.floats.translate(LogicalSize::new(
            writing_mode,
            Au(0),
            -self.base.position.size.block,
        ));

        let containing_block_size =
            LogicalSize::new(writing_mode, Au(0), self.base.position.size.block);
        self.mutate_fragments(&mut |f: &mut Fragment| match f.specific {
            SpecificFragmentInfo::InlineBlock(ref mut info) => {
                let block = FlowRef::deref_mut(&mut info.flow_ref);
                block.mut_base().early_absolute_position_info = EarlyAbsolutePositionInfo {
                    relative_containing_block_size: containing_block_size,
                    relative_containing_block_mode: writing_mode,
                };
            },
            SpecificFragmentInfo::InlineAbsolute(ref mut info) => {
                let block = FlowRef::deref_mut(&mut info.flow_ref);
                block.mut_base().early_absolute_position_info = EarlyAbsolutePositionInfo {
                    relative_containing_block_size: containing_block_size,
                    relative_containing_block_mode: writing_mode,
                };
            },
            _ => (),
        });

        self.base
            .restyle_damage
            .remove(ServoRestyleDamage::REFLOW_OUT_OF_FLOW | ServoRestyleDamage::REFLOW);
        for fragment in &mut self.fragments.fragments {
            fragment
                .restyle_damage
                .remove(ServoRestyleDamage::REFLOW_OUT_OF_FLOW | ServoRestyleDamage::REFLOW);
        }
    }

    fn compute_stacking_relative_position(&mut self, _: &LayoutContext) {
        // First, gather up the positions of all the containing blocks (if any).
        //
        // FIXME(pcwalton): This will get the absolute containing blocks inside `...` wrong in the
        // case of something like:
        //
        //      <span style="position: relative">
        //          Foo
        //          <span style="display: inline-block">...</span>
        //      </span>
        let mut containing_block_positions = Vec::new();
        let container_size = Size2D::new(self.base.block_container_inline_size, Au(0));
        for (fragment_index, fragment) in self.fragments.fragments.iter().enumerate() {
            match fragment.specific {
                SpecificFragmentInfo::InlineAbsolute(_) => {
                    let containing_block_range = self
                        .containing_block_range_for_flow_surrounding_fragment_at_index(
                            FragmentIndex(fragment_index as isize),
                        );
                    let first_fragment_index = containing_block_range.begin().get() as usize;
                    debug_assert!(first_fragment_index < self.fragments.fragments.len());
                    let first_fragment = &self.fragments.fragments[first_fragment_index];
                    let padding_box_origin = (first_fragment.border_box -
                        first_fragment.style.logical_border_width())
                    .start;
                    containing_block_positions.push(
                        padding_box_origin.to_physical(self.base.writing_mode, container_size),
                    );
                },
                SpecificFragmentInfo::InlineBlock(_) if fragment.is_positioned() => {
                    let containing_block_range = self
                        .containing_block_range_for_flow_surrounding_fragment_at_index(
                            FragmentIndex(fragment_index as isize),
                        );
                    let first_fragment_index = containing_block_range.begin().get() as usize;
                    debug_assert!(first_fragment_index < self.fragments.fragments.len());
                    let first_fragment = &self.fragments.fragments[first_fragment_index];
                    let padding_box_origin = (first_fragment.border_box -
                        first_fragment.style.logical_border_width())
                    .start;
                    containing_block_positions.push(
                        padding_box_origin.to_physical(self.base.writing_mode, container_size),
                    );
                },
                _ => {},
            }
        }

        // Then compute the positions of all of our fragments.
        let mut containing_block_positions = containing_block_positions.iter();
        for fragment in &mut self.fragments.fragments {
            let stacking_relative_border_box = fragment.stacking_relative_border_box(
                &self.base.stacking_relative_position,
                &self
                    .base
                    .early_absolute_position_info
                    .relative_containing_block_size,
                self.base
                    .early_absolute_position_info
                    .relative_containing_block_mode,
                CoordinateSystem::Parent,
            );
            let stacking_relative_content_box =
                fragment.stacking_relative_content_box(stacking_relative_border_box);

            let is_positioned = fragment.is_positioned();
            match fragment.specific {
                SpecificFragmentInfo::InlineBlock(ref mut info) => {
                    let flow = FlowRef::deref_mut(&mut info.flow_ref);
                    let block_flow = flow.as_mut_block();
                    block_flow.base.late_absolute_position_info =
                        self.base.late_absolute_position_info;

                    let stacking_relative_position = self.base.stacking_relative_position;
                    if is_positioned {
                        let padding_box_origin = containing_block_positions.next().unwrap();
                        block_flow
                            .base
                            .late_absolute_position_info
                            .stacking_relative_position_of_absolute_containing_block =
                            *padding_box_origin + stacking_relative_position;
                    }

                    block_flow.base.stacking_relative_position =
                        stacking_relative_content_box.origin.to_vector();

                    // Write the clip in our coordinate system into the child flow. (The kid will
                    // fix it up to be in its own coordinate system if necessary.)
                    block_flow.base.clip = self.base.clip
                },
                SpecificFragmentInfo::InlineAbsoluteHypothetical(ref mut info) => {
                    let flow = FlowRef::deref_mut(&mut info.flow_ref);
                    let block_flow = flow.as_mut_block();
                    block_flow.base.late_absolute_position_info =
                        self.base.late_absolute_position_info;

                    block_flow.base.stacking_relative_position =
                        stacking_relative_border_box.origin.to_vector();

                    // As above, this is in our coordinate system for now.
                    block_flow.base.clip = self.base.clip
                },
                SpecificFragmentInfo::InlineAbsolute(ref mut info) => {
                    let flow = FlowRef::deref_mut(&mut info.flow_ref);
                    let block_flow = flow.as_mut_block();
                    block_flow.base.late_absolute_position_info =
                        self.base.late_absolute_position_info;

                    let stacking_relative_position = self.base.stacking_relative_position;
                    let padding_box_origin = containing_block_positions.next().unwrap();
                    block_flow
                        .base
                        .late_absolute_position_info
                        .stacking_relative_position_of_absolute_containing_block =
                        *padding_box_origin + stacking_relative_position;

                    block_flow.base.stacking_relative_position =
                        stacking_relative_border_box.origin.to_vector();

                    // As above, this is in our coordinate system for now.
                    block_flow.base.clip = self.base.clip
                },
                _ => {},
            }
        }
    }

    fn update_late_computed_inline_position_if_necessary(&mut self, _: Au) {}

    fn update_late_computed_block_position_if_necessary(&mut self, _: Au) {}

    fn collect_stacking_contexts(&mut self, state: &mut StackingContextCollectionState) {
        self.base.stacking_context_id = state.current_stacking_context_id;
        self.base.clipping_and_scrolling = Some(state.current_clipping_and_scrolling);
        self.base.clip = state
            .clip_stack
            .last()
            .cloned()
            .unwrap_or_else(Rect::max_rect);

        let previous_cb_clipping_and_scrolling = state.containing_block_clipping_and_scrolling;

        for fragment in self.fragments.fragments.iter_mut() {
            // If a particular fragment would establish a stacking context but has a transform
            // applied that causes it to take up no space, we can skip it entirely.
            if fragment.has_non_invertible_transform_or_zero_scale() {
                continue;
            }
            state.containing_block_clipping_and_scrolling = previous_cb_clipping_and_scrolling;

            let abspos_containing_block = fragment.style.get_box().position != Position::Static;
            if abspos_containing_block {
                state.containing_block_clipping_and_scrolling =
                    state.current_clipping_and_scrolling;
            }

            // We clear this here, but it might be set again if we create a stacking context for
            // this fragment.
            fragment.established_reference_frame = None;

            if !fragment.collect_stacking_contexts_for_blocklike_fragment(state) {
                if !fragment.establishes_stacking_context() {
                    fragment.stacking_context_id = state.current_stacking_context_id;
                } else {
                    fragment.create_stacking_context_for_inline_block(&self.base, state);
                }
            }

            // Reset the containing block clipping and scrolling before each loop iteration,
            // so we don't pollute subsequent fragments.
            state.containing_block_clipping_and_scrolling = previous_cb_clipping_and_scrolling;
        }
    }

    fn build_display_list(&mut self, state: &mut DisplayListBuildState) {
        debug!(
            "Flow: building display list for {} inline fragments",
            self.fragments.len()
        );

        // We iterate using an index here, because we want to avoid doing a doing
        // a double-borrow of self (one mutable for the method call and one immutable
        // for the self.fragments.fragment iterator itself).
        for index in 0..self.fragments.fragments.len() {
            let (establishes_stacking_context, stacking_context_id) = {
                let fragment = self.fragments.fragments.get(index).unwrap();
                (
                    self.base.stacking_context_id != fragment.stacking_context_id,
                    fragment.stacking_context_id,
                )
            };

            let parent_stacking_context_id = state.current_stacking_context_id;
            if establishes_stacking_context {
                state.current_stacking_context_id = stacking_context_id;
            }

            self.build_display_list_for_inline_fragment_at_index(state, index);

            if establishes_stacking_context {
                state.current_stacking_context_id = parent_stacking_context_id
            }
        }
    }

    fn repair_style(&mut self, _: &ServoArc<ComputedValues>) {}

    fn compute_overflow(&self) -> Overflow {
        let mut overflow = Overflow::new();
        let flow_size = self.base.position.size.to_physical(self.base.writing_mode);
        let relative_containing_block_size = &self
            .base
            .early_absolute_position_info
            .relative_containing_block_size;
        for fragment in &self.fragments.fragments {
            overflow.union(&fragment.compute_overflow(&flow_size, relative_containing_block_size))
        }
        overflow
    }

    fn iterate_through_fragment_border_boxes(
        &self,
        iterator: &mut dyn FragmentBorderBoxIterator,
        level: i32,
        stacking_context_position: &Point2D<Au>,
    ) {
        // FIXME(#2795): Get the real container size.
        for fragment in &self.fragments.fragments {
            if !iterator.should_process(fragment) {
                continue;
            }

            let stacking_relative_position = &self.base.stacking_relative_position;
            let relative_containing_block_size = &self
                .base
                .early_absolute_position_info
                .relative_containing_block_size;
            let relative_containing_block_mode = self
                .base
                .early_absolute_position_info
                .relative_containing_block_mode;
            iterator.process(
                fragment,
                level,
                &fragment
                    .stacking_relative_border_box(
                        stacking_relative_position,
                        relative_containing_block_size,
                        relative_containing_block_mode,
                        CoordinateSystem::Own,
                    )
                    .translate(stacking_context_position.to_vector()),
            )
        }
    }

    fn mutate_fragments(&mut self, mutator: &mut dyn FnMut(&mut Fragment)) {
        for fragment in &mut self.fragments.fragments {
            (*mutator)(fragment)
        }
    }

    fn contains_positioned_fragments(&self) -> bool {
        self.fragments
            .fragments
            .iter()
            .any(|fragment| fragment.is_positioned())
    }

    fn contains_relatively_positioned_fragments(&self) -> bool {
        self.fragments
            .fragments
            .iter()
            .any(|fragment| fragment.style.get_box().position == Position::Relative)
    }

    fn generated_containing_block_size(&self, for_flow: OpaqueFlow) -> LogicalSize<Au> {
        let mut containing_block_size = LogicalSize::new(self.base.writing_mode, Au(0), Au(0));
        for index in self.containing_block_range_for_flow(for_flow).each_index() {
            let fragment = &self.fragments.fragments[index.get() as usize];
            if fragment.is_absolutely_positioned() {
                continue;
            }
            containing_block_size.inline += fragment.border_box.size.inline;
            containing_block_size.block =
                max(containing_block_size.block, fragment.border_box.size.block);
        }
        containing_block_size
    }

    fn print_extra_flow_children(&self, print_tree: &mut PrintTree) {
        for fragment in &self.fragments.fragments {
            print_tree.add_item(format!("{:?}", fragment));
        }
    }
}

impl fmt::Debug for InlineFlow {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?} {:?}", self.class(), self.base())
    }
}

#[derive(Clone)]
pub struct InlineFragmentNodeInfo {
    pub address: OpaqueNode,
    pub style: ServoArc<ComputedValues>,
    pub selected_style: ServoArc<ComputedValues>,
    pub pseudo: PseudoElementType,
    pub flags: InlineFragmentNodeFlags,
}

bitflags! {
    #[derive(Clone)]
    pub struct InlineFragmentNodeFlags: u8 {
        const FIRST_FRAGMENT_OF_ELEMENT = 0x01;
        const LAST_FRAGMENT_OF_ELEMENT = 0x02;
    }
}

impl fmt::Debug for InlineFragmentNodeInfo {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self.flags.bits())
    }
}

#[derive(Clone)]
pub struct InlineFragmentContext {
    /// The list of nodes that this fragment will be inheriting styles from,
    /// from the most deeply-nested node out.
    pub nodes: Vec<InlineFragmentNodeInfo>,
}

impl InlineFragmentContext {
    pub fn new() -> InlineFragmentContext {
        InlineFragmentContext { nodes: vec![] }
    }

    #[inline]
    pub fn contains_node(&self, node_address: OpaqueNode) -> bool {
        self.nodes.iter().any(|node| node.address == node_address)
    }

    fn ptr_eq(&self, other: &InlineFragmentContext) -> bool {
        if self.nodes.len() != other.nodes.len() {
            return false;
        }
        for (this_node, other_node) in self.nodes.iter().zip(&other.nodes) {
            if this_node.address != other_node.address {
                return false;
            }
        }
        true
    }
}

impl Default for InlineFragmentContext {
    fn default() -> Self {
        Self::new()
    }
}

fn inline_contexts_are_equal(
    inline_context_a: &Option<InlineFragmentContext>,
    inline_context_b: &Option<InlineFragmentContext>,
) -> bool {
    match (inline_context_a, inline_context_b) {
        (Some(inline_context_a), Some(inline_context_b)) => {
            inline_context_a.ptr_eq(inline_context_b)
        },
        (&None, &None) => true,
        (&Some(_), &None) | (&None, &Some(_)) => false,
    }
}

/// Ascent and space needed above and below the baseline for a fragment. See CSS 2.1 § 10.8.1.
///
/// Descent is not included in this structure because it can be computed from the fragment's
/// border/content box and the ascent.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct InlineMetrics {
    /// The amount of space above the baseline needed for this fragment.
    pub space_above_baseline: Au,
    /// The amount of space below the baseline needed for this fragment.
    pub space_below_baseline: Au,
    /// The distance from the baseline to the top of this fragment. This can differ from
    /// `block_size_above_baseline` if the fragment needs some empty space above it due to
    /// line-height, etc.
    pub ascent: Au,
}

impl InlineMetrics {
    /// Creates a new set of inline metrics.
    pub fn new(space_above_baseline: Au, space_below_baseline: Au, ascent: Au) -> InlineMetrics {
        InlineMetrics {
            space_above_baseline,
            space_below_baseline,
            ascent,
        }
    }

    /// Calculates inline metrics from font metrics and line block-size per CSS 2.1 § 10.8.1.
    #[inline]
    pub fn from_font_metrics(font_metrics: &FontMetrics, line_height: Au) -> InlineMetrics {
        let leading = line_height - (font_metrics.ascent + font_metrics.descent);

        // Calculating the half leading here and then using leading - half_leading
        // below ensure that we don't introduce any rounding accuracy issues here.
        // The invariant is that the resulting total line height must exactly
        // equal the requested line_height.
        let half_leading = leading.scale_by(0.5);
        InlineMetrics {
            space_above_baseline: font_metrics.ascent + half_leading,
            space_below_baseline: font_metrics.descent + leading - half_leading,
            ascent: font_metrics.ascent,
        }
    }

    /// Returns the sum of the space needed above and below the baseline.
    fn space_needed(&self) -> Au {
        self.space_above_baseline + self.space_below_baseline
    }
}

#[derive(Clone, Copy, PartialEq)]
enum LineFlushMode {
    No,
    Flush,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct LineMetrics {
    pub space_above_baseline: Au,
    pub space_below_baseline: Au,
}

impl LineMetrics {
    pub fn new(space_above_baseline: Au, space_below_baseline: Au) -> LineMetrics {
        LineMetrics {
            space_above_baseline,
            space_below_baseline,
        }
    }

    /// Returns the line metrics that result from combining the line that these metrics represent
    /// with a fragment with the given metrics.
    fn new_metrics_for_fragment(&self, fragment_inline_metrics: &InlineMetrics) -> LineMetrics {
        LineMetrics {
            space_above_baseline: max(
                self.space_above_baseline,
                fragment_inline_metrics.space_above_baseline,
            ),
            space_below_baseline: max(
                self.space_below_baseline,
                fragment_inline_metrics.space_below_baseline,
            ),
        }
    }

    fn for_line_and_fragment(
        line: &Line,
        fragment: &Fragment,
        layout_context: &LayoutContext,
    ) -> LineMetrics {
        if !fragment.is_hypothetical() {
            let space_above_baseline = line.metrics.space_above_baseline;
            return LineMetrics {
                space_above_baseline,
                space_below_baseline: line.bounds.size.block - space_above_baseline,
            };
        }

        let hypothetical_line_metrics = line.new_metrics_for_fragment(fragment, layout_context);
        let hypothetical_block_size =
            line.new_block_size_for_fragment(fragment, &hypothetical_line_metrics, layout_context);
        let hypothetical_space_above_baseline = hypothetical_line_metrics.space_above_baseline;
        LineMetrics {
            space_above_baseline: hypothetical_space_above_baseline,
            space_below_baseline: hypothetical_block_size - hypothetical_space_above_baseline,
        }
    }

    /// Returns the sum of the space needed above and below the baseline.
    pub fn space_needed(&self) -> Au {
        self.space_above_baseline + self.space_below_baseline
    }
}
