// Copyright 2025 the Parley Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Low-level shaping interfaces.

use core::ops::Range;

use crate::FontData;
use crate::analysis::Boundary;
use crate::layout::{Glyph, RunMetrics};

/// Metadata for a shaped run.
pub(crate) struct ShapeRun<'a> {
    pub(crate) font: FontData,
    pub(crate) font_size: f32,
    pub(crate) font_attrs: fontique::Attributes,
    pub(crate) synthesis: fontique::Synthesis,
    pub(crate) coords: &'a [harfrust::NormalizedCoord],
    pub(crate) text_range: Range<usize>,
    pub(crate) bidi_level: u8,
    pub(crate) metrics: RunMetrics,
    pub(crate) word_spacing: f32,
    pub(crate) letter_spacing: f32,
}

/// Glyph storage for a shaped cluster.
pub(crate) enum ShapeClusterGlyphs {
    Inline(u32),
    Range { len: u8 },
    None,
}

/// Metadata for a shaped cluster.
pub(crate) struct ShapeCluster {
    pub(crate) boundary: Boundary,
    pub(crate) source_char: char,
    pub(crate) flags: u16,
    pub(crate) style_index: u16,
    pub(crate) text_len: u8,
    pub(crate) text_offset: u16,
    pub(crate) advance: f32,
    pub(crate) glyphs: ShapeClusterGlyphs,
}

/// Receives shaped paragraph output emitted by the pipeline.
///
/// Implementors should prefer writing directly into caller-owned storage and
/// reuse scratch buffers where possible to avoid extra allocations or copies.
pub(crate) trait ShapeSink {
    /// Push an inline box item into the shaped output stream.
    fn push_inline_box(&mut self, index: usize);

    /// Begin a shaped text run.
    fn begin_run(&mut self, run: ShapeRun<'_>);

    /// Push a glyph belonging to the current run.
    fn push_glyph(&mut self, glyph: Glyph);

    /// Push a cluster belonging to the current run.
    fn push_cluster(&mut self, cluster: ShapeCluster);

    /// Finish the current run with its total advance.
    fn end_run(&mut self, advance: f32);
}
