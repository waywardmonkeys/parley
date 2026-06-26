// Copyright 2026 the Parley Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use alloc::vec::Vec;
use core::fmt::{self, Debug};

use parley::{Brush, FontContext, Layout, LayoutContext, StyleRunBuilder, TextStyle};
use styled_text::{
    SegmentStyle, StyleId, StyledSegmentsWorkspace, StyledText, TextRange, TextStorage,
};

use crate::{ParleyLayoutStyle, ParleyPaintStyle, ParleyStyledText};

/// Reusable allocation workspace for lowering styled text into Parley style runs.
///
/// Reuse this across layout builds to retain both the styled-segment workspace
/// and the temporary map from [`styled_text::StyleId`] to Parley `u16` style indices.
#[derive(Clone, Debug, Default)]
pub struct ParleyStyleRunWorkspace {
    segments: StyledSegmentsWorkspace,
    style_indices: Vec<u16>,
}

impl ParleyStyleRunWorkspace {
    /// Creates an empty workspace.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Clears retained style-index data while keeping allocations for reuse.
    ///
    /// Segment scratch data is rebuilt the next time the workspace is used.
    pub fn clear(&mut self) {
        self.style_indices.clear();
    }
}

/// Error returned when styled text cannot be lowered to Parley.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// The styled text is not backed by one contiguous string.
    ///
    /// Parley's current style-run builder accepts a single `&str`; chunked text
    /// storage should be flattened or handled by a future chunk-aware adapter.
    NonContiguousText,
    /// The styled text style table is too large for Parley's `u16` style
    /// indices.
    TooManyStyles {
        /// Number of interned styled-text styles.
        count: usize,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonContiguousText => {
                f.write_str("styled text is not backed by one contiguous string")
            }
            Self::TooManyStyles { count } => {
                write!(
                    f,
                    "styled text has {count} styles, but Parley supports at most \
                     {MAX_PARLEY_STYLES}"
                )
            }
        }
    }
}

impl core::error::Error for Error {}

/// Pushes resolved styled-text segments into an existing Parley style-run builder.
///
/// The `push_style` callback receives the Parley builder and each interned full
/// styled-text style in [`styled_text::StyleId`] table order. It must push a
/// Parley style and return the style-table index produced by
/// [`StyleRunBuilder::push_style`].
///
/// This pushes every interned styled-text style, including styles not currently
/// used by any resolved segment. That preserves the simple table-order lowering
/// contract without allocating a filtered style table. The temporary
/// styled-text-to-Parley style-index map is stored in `workspace` and reused
/// across calls.
pub fn push_style_runs<T, L, P, B, F>(
    builder: &mut StyleRunBuilder<'_, B>,
    styled: &StyledText<T, L, P>,
    workspace: &mut ParleyStyleRunWorkspace,
    mut push_style: F,
) -> Result<(), Error>
where
    T: Debug + TextStorage,
    B: Brush,
    F: FnMut(&mut StyleRunBuilder<'_, B>, SegmentStyle<'_, L, P>) -> u16,
{
    let style_count = styled.style_set().style_len();
    workspace.style_indices.clear();
    check_style_count(style_count)?;
    let max_runs = styled.style_spans_len().saturating_mul(2).saturating_add(1);
    builder.reserve(style_count, max_runs);
    workspace.style_indices.reserve(style_count);

    for style_id in styled.style_set().style_ids() {
        let style = styled.style_set().segment_style(style_id);
        workspace.style_indices.push(push_style(builder, style));
    }

    let mut pending: Option<(TextRange, StyleId)> = None;
    for segment in workspace.segments.segments(styled) {
        let range = segment.range();
        let style = segment.style();
        match pending.take() {
            Some((pending_range, pending_style))
                if pending_style == style && pending_range.end() == range.start() =>
            {
                pending = Some((
                    TextRange::new_unchecked(pending_range.start(), range.end()),
                    pending_style,
                ));
            }
            Some((pending_range, pending_style)) => {
                let style_index = workspace.style_indices[pending_style.index()];
                builder.push_style_run(style_index, pending_range.as_range());
                pending = Some((range, style));
            }
            None => {
                pending = Some((range, style));
            }
        }
    }
    if let Some((range, style)) = pending {
        let style_index = workspace.style_indices[style.index()];
        builder.push_style_run(style_index, range.as_range());
    }

    Ok(())
}

/// Pushes one default Parley style payload into a Parley style-run builder.
///
/// This is the [`push_style_runs`] callback for [`ParleyStyledText`].
pub fn push_parley_style<B: Brush>(
    builder: &mut StyleRunBuilder<'_, B>,
    style: SegmentStyle<'_, ParleyLayoutStyle, ParleyPaintStyle<B>>,
) -> u16 {
    let layout = style.layout();
    let paint = style.paint();
    builder.push_style(TextStyle {
        font_family: layout.font_family.clone(),
        font_size: layout.font_size,
        font_width: layout.font_width,
        font_style: layout.font_style,
        font_weight: layout.font_weight,
        font_variations: layout.font_variations.clone(),
        font_features: layout.font_features.clone(),
        locale: layout.locale,
        brush: paint.brush.clone(),
        has_underline: paint.has_underline,
        underline_offset: paint.underline_offset,
        underline_size: paint.underline_size,
        underline_brush: paint.underline_brush.clone(),
        has_strikethrough: paint.has_strikethrough,
        strikethrough_offset: paint.strikethrough_offset,
        strikethrough_size: paint.strikethrough_size,
        strikethrough_brush: paint.strikethrough_brush.clone(),
        line_height: layout.line_height,
        word_spacing: layout.word_spacing,
        letter_spacing: layout.letter_spacing,
        word_break: layout.word_break,
        overflow_wrap: layout.overflow_wrap,
        text_wrap_mode: layout.text_wrap_mode,
    })
}

const MAX_PARLEY_STYLES: usize = u16::MAX as usize + 1;

fn check_style_count(count: usize) -> Result<(), Error> {
    if count > MAX_PARLEY_STYLES {
        return Err(Error::TooManyStyles { count });
    }
    Ok(())
}

/// Builds a Parley layout from styled text backed by a contiguous string.
///
/// The callback has the same contract as [`push_style_runs`]. This helper is
/// intentionally thin so callers remain in control of how their interned style
/// payloads become Parley [`parley::TextStyle`] values.
pub fn build_layout_from_styled_text<T, L, P, B, F>(
    layout_cx: &mut LayoutContext<B>,
    font_cx: &mut FontContext,
    styled: &StyledText<T, L, P>,
    workspace: &mut ParleyStyleRunWorkspace,
    scale: f32,
    quantize: bool,
    push_style: F,
) -> Result<Layout<B>, Error>
where
    T: Debug + TextStorage,
    B: Brush,
    F: FnMut(&mut StyleRunBuilder<'_, B>, SegmentStyle<'_, L, P>) -> u16,
{
    let text = styled.as_str().ok_or(Error::NonContiguousText)?;
    let mut builder = layout_cx.style_run_builder(font_cx, text, scale, quantize);
    push_style_runs(&mut builder, styled, workspace, push_style)?;
    Ok(builder.build(text))
}

/// Builds a Parley layout from styled text using the default Parley style
/// payloads.
pub fn build_layout_from_parley_styled_text<T, B>(
    layout_cx: &mut LayoutContext<B>,
    font_cx: &mut FontContext,
    styled: &ParleyStyledText<T, B>,
    workspace: &mut ParleyStyleRunWorkspace,
    scale: f32,
    quantize: bool,
) -> Result<Layout<B>, Error>
where
    T: Debug + TextStorage,
    B: Brush,
{
    build_layout_from_styled_text(
        layout_cx,
        font_cx,
        styled,
        workspace,
        scale,
        quantize,
        push_parley_style,
    )
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;
    use alloc::vec;
    use alloc::vec::Vec;

    use parley::TextStyle;
    use styled_text::{StyleSetBuilder, StyledText, StyledTextBuilder};

    use super::{
        Error, MAX_PARLEY_STYLES, ParleyStyleRunWorkspace, build_layout_from_parley_styled_text,
        check_style_count, push_style_runs,
    };
    use crate::{ParleyLayoutStyle, ParleyPaintStyle, ParleyStyleChange};

    #[test]
    fn generic_lowering_accepts_custom_style_payloads() {
        let mut style_builder = StyleSetBuilder::<u8, ()>::new();
        let base = style_builder.intern_style(12, ());
        let large = style_builder.intern_style(24, ());
        let styles = Arc::new(style_builder.finish());

        let mut styled = StyledText::new("abcd", styles, base);
        styled
            .apply_style_bytes(1..2, large)
            .expect("valid style range");
        styled
            .apply_style_bytes(2..3, large)
            .expect("valid style range");

        let mut font_cx = parley::FontContext::new();
        let mut layout_cx = parley::LayoutContext::<()>::new();
        let mut builder = layout_cx.style_run_builder(&mut font_cx, "abcd", 1.0, false);
        let mut workspace = ParleyStyleRunWorkspace::new();

        let mut pushed_font_sizes = Vec::new();
        push_style_runs(&mut builder, &styled, &mut workspace, |builder, style| {
            let font_size = f32::from(*style.layout());
            pushed_font_sizes.push(font_size);
            let parley_style = TextStyle::<()> {
                font_size,
                ..TextStyle::default()
            };
            builder.push_style(parley_style)
        })
        .expect("style count fits Parley");

        let _layout = builder.build("abcd");
        assert_eq!(pushed_font_sizes, vec![12.0, 24.0]);
    }

    #[test]
    fn rejects_style_tables_larger_than_parley_can_index() {
        let count = MAX_PARLEY_STYLES + 1;
        assert_eq!(
            check_style_count(count),
            Err(Error::TooManyStyles { count })
        );
    }

    #[test]
    fn builds_layout_from_default_parley_payloads() {
        let mut builder = StyledTextBuilder::<_, _, ParleyStyleChange<()>>::new(
            ParleyLayoutStyle::default(),
            ParleyPaintStyle::default(),
        );
        builder.push_with(
            "abcd",
            ParleyStyleChange::from(parley::StyleProperty::FontSize(24.0)),
        );
        let styled = builder.finish();

        let mut font_cx = parley::FontContext::new();
        let mut layout_cx = parley::LayoutContext::<()>::new();
        let mut workspace = ParleyStyleRunWorkspace::new();
        let layout = build_layout_from_parley_styled_text(
            &mut layout_cx,
            &mut font_cx,
            &styled,
            &mut workspace,
            1.0,
            false,
        )
        .expect("string storage is contiguous");

        assert_eq!(layout.styles().len(), 2);
    }
}
