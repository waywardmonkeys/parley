// Copyright 2021 the Parley Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use crate::inline_box::InlineBox;
use crate::layout::{ContentWidths, Glyph, LineMetrics, RunMetrics, Style};
use crate::pipeline::{ShapeCluster, ShapeClusterGlyphs, ShapeRun, ShapeSink};
use crate::style::Brush;
use crate::util::nearly_zero;
use crate::{FontData, IndentOptions, InlineBoxKind, OverflowWrap, TextWrapMode};
use core::ops::Range;

use alloc::vec::Vec;

use crate::analysis::Boundary;
use crate::analysis::cluster::Whitespace;

#[derive(Copy, Clone, Debug, PartialEq)]
pub(crate) struct ClusterData {
    pub(crate) info: ClusterInfo,
    /// Cluster flags (see impl methods for details).
    pub(crate) flags: u16,
    /// Style index for this cluster.
    pub(crate) style_index: u16,
    /// Number of glyphs in this cluster (0xFF = single glyph stored inline)
    pub(crate) glyph_len: u8,
    /// Number of text bytes in this cluster
    pub(crate) text_len: u8,
    /// If `glyph_len == 0xFF`, then `glyph_offset` is a glyph identifier,
    /// otherwise, it's an offset into the glyph array with the base
    /// taken from the owning run.
    pub(crate) glyph_offset: u32,
    /// Offset into the text for this cluster
    pub(crate) text_offset: u16,
    /// Advance width for this cluster
    pub(crate) advance: f32,
}

impl ClusterData {
    pub(crate) const LIGATURE_START: u16 = 1;
    pub(crate) const LIGATURE_COMPONENT: u16 = 2;

    #[inline(always)]
    pub(crate) fn is_ligature_start(self) -> bool {
        self.flags & Self::LIGATURE_START != 0
    }

    #[inline(always)]
    pub(crate) fn is_ligature_component(self) -> bool {
        self.flags & Self::LIGATURE_COMPONENT != 0
    }

    #[inline(always)]
    pub(crate) fn text_range(self, run: &RunData) -> Range<usize> {
        let start = run.text_range.start + self.text_offset as usize;
        start..start + self.text_len as usize
    }
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub(crate) struct ClusterInfo {
    boundary: Boundary,
    source_char: char,
}

impl ClusterInfo {
    pub(crate) fn new(boundary: Boundary, source_char: char) -> Self {
        Self {
            boundary,
            source_char,
        }
    }

    // Returns the boundary type of the cluster.
    pub(crate) fn boundary(self) -> Boundary {
        self.boundary
    }

    // Returns the whitespace type of the cluster.
    pub(crate) fn whitespace(self) -> Whitespace {
        to_whitespace(self.source_char)
    }

    /// Returns if the cluster is a line boundary.
    pub(crate) fn is_boundary(self) -> bool {
        self.boundary != Boundary::None
    }

    /// Returns if the cluster is an emoji.
    pub(crate) fn is_emoji(self) -> bool {
        // TODO: Defer to ICU4X properties (see: https://docs.rs/icu/latest/icu/properties/props/struct.Emoji.html).
        matches!(self.source_char as u32, 0x1F600..=0x1F64F | 0x1F300..=0x1F5FF | 0x1F680..=0x1F6FF | 0x2600..=0x26FF | 0x2700..=0x27BF)
    }

    /// Returns if the cluster is any whitespace.
    pub(crate) fn is_whitespace(self) -> bool {
        self.source_char.is_whitespace()
    }

    /// Returns the cluster's original character.
    pub(crate) fn source_char(self) -> char {
        self.source_char
    }
}

const fn to_whitespace(c: char) -> Whitespace {
    const LINE_SEPARATOR: char = '\u{2028}';
    const PARAGRAPH_SEPARATOR: char = '\u{2029}';

    match c {
        ' ' => Whitespace::Space,
        '\t' => Whitespace::Tab,
        '\n' | '\r' | LINE_SEPARATOR | PARAGRAPH_SEPARATOR => Whitespace::Newline,
        '\u{00A0}' => Whitespace::NoBreakSpace,
        _ => Whitespace::None,
    }
}

/// `HarfRust`-based run data
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RunData {
    /// Index of the font for the run.
    pub(crate) font_index: usize,
    /// Font size.
    pub(crate) font_size: f32,
    /// Font attributes, needed for accessibility.
    pub(crate) font_attrs: fontique::Attributes,
    /// Synthesis for rendering (contains variation settings)
    pub(crate) synthesis: fontique::Synthesis,
    /// Range of normalized coordinates in the layout data.
    pub(crate) coords_range: Range<usize>,
    /// Range of the source text.
    pub(crate) text_range: Range<usize>,
    /// Bidi level for the run.
    pub(crate) bidi_level: u8,
    /// Range of clusters.
    pub(crate) cluster_range: Range<usize>,
    /// Base for glyph indices.
    pub(crate) glyph_start: usize,
    /// Metrics for the run.
    pub(crate) metrics: RunMetrics,
    /// Additional word spacing.
    pub(crate) word_spacing: f32,
    /// Additional letter spacing.
    pub(crate) letter_spacing: f32,
    /// Total advance of the run.
    pub(crate) advance: f32,
}

#[derive(Copy, Clone, Default, PartialEq, Debug)]
pub enum BreakReason {
    #[default]
    None,
    Regular,
    Explicit,
    Emergency,
}

#[derive(Clone, Default, Debug, PartialEq)]
pub(crate) struct LineData {
    /// Range of the source text.
    pub(crate) text_range: Range<usize>,
    /// Range of line items.
    pub(crate) item_range: Range<usize>,
    /// Metrics for the line.
    pub(crate) metrics: LineMetrics,
    /// The cause of the line break.
    pub(crate) break_reason: BreakReason,
    /// Maximum advance for the line.
    pub(crate) max_advance: f32,
    /// Number of justified clusters on the line.
    pub(crate) num_spaces: usize,
    /// Text indent applied to this line.
    pub(crate) indent: f32,
}

impl LineData {
    pub(crate) fn size(&self) -> f32 {
        self.metrics.ascent + self.metrics.descent + self.metrics.leading
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LineItemData {
    /// Whether the item is a run or an inline box
    pub(crate) kind: LayoutItemKind,
    /// The index of the run or inline box in the runs or `inline_boxes` vec
    pub(crate) index: usize,
    /// Bidi level for the item (used for reordering)
    pub(crate) bidi_level: u8,
    /// Advance (size in direction of text flow) for the run.
    pub(crate) advance: f32,

    // Fields that only apply to text runs (Ignored for boxes)
    // TODO: factor this out?
    /// True if the run is composed entirely of whitespace.
    pub(crate) is_whitespace: bool,
    /// True if the run ends in whitespace.
    pub(crate) has_trailing_whitespace: bool,
    /// Range of the source text.
    pub(crate) text_range: Range<usize>,
    /// Range of clusters.
    pub(crate) cluster_range: Range<usize>,
}

impl LineItemData {
    pub(crate) fn is_text_run(&self) -> bool {
        self.kind == LayoutItemKind::TextRun
    }

    #[inline(always)]
    pub(crate) fn is_rtl(&self) -> bool {
        self.bidi_level & 1 != 0
    }

    /// If the item is a text run
    ///   - Determine if it consists entirely of whitespace (`is_whitespace` property)
    ///   - Determine if it has trailing whitespace (`has_trailing_whitespace` property)
    pub(crate) fn compute_whitespace_properties<B: Brush>(&mut self, layout_data: &LayoutData<B>) {
        // Skip items which are not text runs
        if self.kind != LayoutItemKind::TextRun {
            return;
        }

        self.is_whitespace = true;
        if self.is_rtl() {
            // RTL runs check for "trailing" whitespace at the front.
            for cluster in layout_data.paragraph.clusters[self.cluster_range.clone()].iter() {
                if cluster.info.is_whitespace() {
                    self.has_trailing_whitespace = true;
                } else {
                    self.is_whitespace = false;
                    break;
                }
            }
        } else {
            for cluster in layout_data.paragraph.clusters[self.cluster_range.clone()]
                .iter()
                .rev()
            {
                if cluster.info.is_whitespace() {
                    self.has_trailing_whitespace = true;
                } else {
                    self.is_whitespace = false;
                    break;
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LayoutItemKind {
    TextRun,
    InlineBox,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LayoutItem {
    /// Whether the item is a run or an inline box
    pub(crate) kind: LayoutItemKind,
    /// The index of the run or inline box in the runs or `inline_boxes` vec
    pub(crate) index: usize,
    /// Bidi level for the item (used for reordering)
    pub(crate) bidi_level: u8,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ShapedParagraph<B: Brush> {
    // General settings (directly from the "builder")
    /// The display scale factor
    pub(crate) scale: f32,
    /// Whether metrics should be quantized to pixel boundaries
    pub(crate) quantize: bool,
    /// The `BiDi` base level
    pub(crate) base_level: u8,
    /// The length of the text in the layout
    pub(crate) text_len: usize,

    // Output of style resolution (input to line breaking)
    pub(crate) styles: Vec<Style<B>>,
    pub(crate) inline_boxes: Vec<InlineBox>,

    // Output of shaping (input to line breaking)
    pub(crate) fonts: Vec<FontData>,
    pub(crate) coords: Vec<i16>,
    pub(crate) runs: Vec<RunData>,
    pub(crate) items: Vec<LayoutItem>,
    pub(crate) clusters: Vec<ClusterData>,
    pub(crate) glyphs: Vec<Glyph>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LayoutData<B: Brush> {
    pub(crate) paragraph: ShapedParagraph<B>,

    // Output of line breaking
    /// The lines in the
    pub(crate) lines: Vec<LineData>,
    /// Items within each line
    pub(crate) line_items: Vec<LineItemData>,
    /// The width constraint that was used to line break the layout
    pub(crate) layout_max_advance: f32,
    /// The computed width of the layout excluding trailing whitespace
    pub(crate) width: f32,
    /// The computed width of the layout including trailing whitespace
    pub(crate) full_width: f32,
    /// The computed height of the layout
    pub(crate) height: f32,

    // Output of alignment
    #[cfg(feature = "accesskit")]
    /// Directly store the alignment if accessibility is enabled so we can
    /// set the corresponding AccessKit property.
    pub(crate) alignment: Option<super::Alignment>,
    /// Whether the layout is aligned with [`crate::Alignment::Justify`].
    pub(crate) is_aligned_justified: bool,
    /// The text-indent amount in layout units.
    pub(crate) indent_amount: f32,
    /// Options controlling text-indent behavior (each-line, hanging).
    pub(crate) indent_options: IndentOptions,
}

impl<B: Brush> Default for ShapedParagraph<B> {
    fn default() -> Self {
        Self {
            scale: 1.,
            quantize: true,
            base_level: 0,
            text_len: 0,
            fonts: Vec::new(),
            coords: Vec::new(),
            styles: Vec::new(),
            inline_boxes: Vec::new(),
            runs: Vec::new(),
            items: Vec::new(),
            clusters: Vec::new(),
            glyphs: Vec::new(),
        }
    }
}

impl<B: Brush> Default for LayoutData<B> {
    fn default() -> Self {
        Self {
            paragraph: ShapedParagraph::default(),
            width: 0.,
            full_width: 0.,
            height: 0.,
            lines: Vec::new(),
            line_items: Vec::new(),
            #[cfg(feature = "accesskit")]
            alignment: None,
            is_aligned_justified: false,
            layout_max_advance: 0.0,
            indent_amount: 0.0,
            indent_options: IndentOptions::default(),
        }
    }
}

impl<B: Brush> ShapedParagraph<B> {
    pub(crate) fn clear(&mut self) {
        self.scale = 1.;
        self.quantize = true;
        self.base_level = 0;
        self.text_len = 0;
        self.fonts.clear();
        self.coords.clear();
        self.styles.clear();
        self.inline_boxes.clear();
        self.runs.clear();
        self.items.clear();
        self.clusters.clear();
        self.glyphs.clear();
    }

    /// Push an inline box to the list of items.
    fn push_inline_box_impl(&mut self, index: usize) {
        // Give the box the same bidi level as the preceding text run
        // (or else default to 0 if there is not yet a text run)
        let bidi_level = self.runs.last().map(|r| r.bidi_level).unwrap_or(0);

        self.items.push(LayoutItem {
            kind: LayoutItemKind::InlineBox,
            index,
            bidi_level,
        });
    }

    pub(crate) fn finish(&mut self) {
        for run in &self.runs {
            let word = run.word_spacing;
            let letter = run.letter_spacing;
            if nearly_zero(word) && nearly_zero(letter) {
                continue;
            }
            let clusters = &mut self.clusters[run.cluster_range.clone()];
            for cluster in clusters {
                let mut spacing = letter;
                if !nearly_zero(word) && cluster.info.whitespace().is_space_or_nbsp() {
                    spacing += word;
                }
                if !nearly_zero(spacing) {
                    cluster.advance += spacing;
                    if cluster.glyph_len != 0xFF {
                        let start = run.glyph_start + cluster.glyph_offset as usize;
                        let end = start + cluster.glyph_len as usize;
                        let glyphs = &mut self.glyphs[start..end];
                        if let Some(last) = glyphs.last_mut() {
                            last.advance += spacing;
                        }
                    }
                }
            }
        }
    }

    // TODO: this method does not handle mixed direction text at all.
    pub(crate) fn calculate_content_widths(&self) -> ContentWidths {
        fn whitespace_advance(cluster: Option<&ClusterData>) -> f32 {
            cluster
                .filter(|cluster| cluster.info.whitespace().is_space_or_nbsp())
                .map_or(0.0, |cluster| cluster.advance)
        }

        let mut min_width = 0.0_f32;
        let mut max_width = 0.0_f32;

        let mut running_min_width = 0.0;
        let mut running_max_width = 0.0;
        let mut text_wrap_mode = TextWrapMode::Wrap;
        let mut prev_cluster: Option<&ClusterData> = None;
        let is_rtl = self.base_level & 1 == 1;
        for item in &self.items {
            match item.kind {
                LayoutItemKind::TextRun => {
                    let run = &self.runs[item.index];
                    let clusters = &self.clusters[run.cluster_range.clone()];
                    if is_rtl {
                        prev_cluster = clusters.first();
                    }
                    for cluster in clusters {
                        let boundary = cluster.info.boundary();
                        let style = &self.styles[cluster.style_index as usize];
                        let prev_text_wrap_mode = text_wrap_mode;
                        text_wrap_mode = style.text_wrap_mode;
                        if boundary == Boundary::Mandatory
                            || (prev_text_wrap_mode == TextWrapMode::Wrap
                                && (boundary == Boundary::Line
                                    || style.overflow_wrap == OverflowWrap::Anywhere))
                        {
                            let trailing_whitespace = whitespace_advance(prev_cluster);
                            min_width = min_width.max(running_min_width - trailing_whitespace);
                            running_min_width = 0.0;
                            if boundary == Boundary::Mandatory {
                                max_width = max_width.max(running_max_width - trailing_whitespace);
                                running_max_width = 0.0;
                            }
                        }
                        running_min_width += cluster.advance;
                        running_max_width += cluster.advance;
                        if !is_rtl {
                            prev_cluster = Some(cluster);
                        }
                    }
                    let trailing_whitespace = whitespace_advance(prev_cluster);
                    min_width = min_width.max(running_min_width - trailing_whitespace);
                }
                LayoutItemKind::InlineBox => {
                    let ibox = &self.inline_boxes[item.index];
                    if ibox.kind == InlineBoxKind::InFlow {
                        running_max_width += ibox.width;
                        if text_wrap_mode == TextWrapMode::Wrap {
                            let trailing_whitespace = whitespace_advance(prev_cluster);
                            min_width = min_width.max(running_min_width - trailing_whitespace);
                            min_width = min_width.max(ibox.width);
                            running_min_width = 0.0;
                        } else {
                            running_min_width += ibox.width;
                        }
                    }
                    prev_cluster = None;
                }
            }
            let trailing_whitespace = whitespace_advance(prev_cluster);
            max_width = max_width.max(running_max_width - trailing_whitespace);
        }

        let trailing_whitespace = whitespace_advance(prev_cluster);
        min_width = min_width.max(running_min_width - trailing_whitespace);

        ContentWidths {
            min: min_width,
            max: max_width,
        }
    }
}

impl<B: Brush> LayoutData<B> {
    pub(crate) fn clear(&mut self) {
        self.paragraph.clear();
        self.width = 0.;
        self.full_width = 0.;
        self.height = 0.;
        self.lines.clear();
        self.line_items.clear();
    }

    // TODO: this method does not handle mixed direction text at all.
    pub(crate) fn calculate_content_widths(&self) -> ContentWidths {
        self.paragraph.calculate_content_widths()
    }
}

pub(crate) struct ShapedParagraphSink<'a, B: Brush> {
    paragraph: &'a mut ShapedParagraph<B>,
    pending_run: Option<PendingRun>,
}

struct PendingRun {
    run: RunData,
    cluster_start: usize,
}

impl<'a, B: Brush> ShapedParagraphSink<'a, B> {
    /// Sink that writes streamed shaping output directly into a shaped
    /// paragraph's owned buffers.
    pub(crate) fn new(paragraph: &'a mut ShapedParagraph<B>) -> Self {
        Self {
            paragraph,
            pending_run: None,
        }
    }
}

impl<B: Brush> ShapeSink for ShapedParagraphSink<'_, B> {
    fn push_inline_box(&mut self, index: usize) {
        self.paragraph.push_inline_box_impl(index);
    }

    fn begin_run(&mut self, run: ShapeRun<'_>) {
        let coords_start = self.paragraph.coords.len();
        self.paragraph
            .coords
            .extend(run.coords.iter().map(|c| c.to_bits()));
        let coords_end = self.paragraph.coords.len();

        let font_index = self
            .paragraph
            .fonts
            .iter()
            .position(|f| *f == run.font)
            .unwrap_or_else(|| {
                let index = self.paragraph.fonts.len();
                self.paragraph.fonts.push(run.font);
                index
            });

        self.pending_run = Some(PendingRun {
            run: RunData {
                font_index,
                font_size: run.font_size,
                font_attrs: run.font_attrs,
                synthesis: run.synthesis,
                coords_range: coords_start..coords_end,
                text_range: run.text_range,
                bidi_level: run.bidi_level,
                cluster_range: self.paragraph.clusters.len()..self.paragraph.clusters.len(),
                glyph_start: self.paragraph.glyphs.len(),
                metrics: run.metrics,
                word_spacing: run.word_spacing,
                letter_spacing: run.letter_spacing,
                advance: 0.0,
            },
            cluster_start: self.paragraph.clusters.len(),
        });
    }

    fn push_glyph(&mut self, glyph: Glyph) {
        self.paragraph.glyphs.push(glyph);
    }

    fn push_cluster(&mut self, cluster: ShapeCluster) {
        let run = &self.pending_run.as_ref().unwrap().run;
        let total_run_glyphs = self.paragraph.glyphs.len() - run.glyph_start;

        let (glyph_len, glyph_offset, advance) = match cluster.glyphs {
            ShapeClusterGlyphs::Inline(glyph_id) => (0xFF_u8, glyph_id, cluster.advance),
            ShapeClusterGlyphs::Range { len } => {
                // Cluster glyph offsets are stored relative to the start of
                // the owning run, matching the existing `LayoutData` encoding.
                let glyph_offset = total_run_glyphs.saturating_sub(len as usize) as u32;
                (len, glyph_offset, cluster.advance)
            }
            ShapeClusterGlyphs::None => (0_u8, 0_u32, cluster.advance),
        };

        self.paragraph.clusters.push(ClusterData {
            info: ClusterInfo::new(cluster.boundary, cluster.source_char),
            flags: cluster.flags,
            style_index: cluster.style_index,
            glyph_len,
            text_len: cluster.text_len,
            glyph_offset,
            text_offset: cluster.text_offset,
            advance,
        });
    }

    fn end_run(&mut self, advance: f32) {
        let mut pending_run = self.pending_run.take().unwrap();
        pending_run.run.advance = advance;
        pending_run.run.cluster_range = pending_run.cluster_start..self.paragraph.clusters.len();
        if pending_run.run.bidi_level & 1 == 1 {
            // Reverse clusters into logical order for RTL.
            //
            // Preserve the previous compatibility behavior by normalizing RTL
            // clusters into logical order here.
            //
            // TODO: if the streamed shaping path later guarantees logical-order
            // cluster emission for all sinks, this reversal may be removable.
            self.paragraph.clusters[pending_run.run.cluster_range.clone()].reverse();
        }
        if !pending_run.run.cluster_range.is_empty() {
            self.paragraph.runs.push(pending_run.run);
            self.paragraph.items.push(LayoutItem {
                kind: LayoutItemKind::TextRun,
                index: self.paragraph.runs.len() - 1,
                bidi_level: self.paragraph.runs.last().unwrap().bidi_level,
            });
        }
    }
}
