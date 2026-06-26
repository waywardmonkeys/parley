// Copyright 2026 the Parley Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use alloc::vec::Vec;

use parley::{
    Brush, FontFamily, FontFeatures, FontStyle, FontVariations, FontWeight, FontWidth, Language,
    LineHeight, OverflowWrap, StyleProperty, TextStyle, TextWrapMode, WordBreak,
};
use styled_text::StylePatch;

/// Layout-affecting style payload for the default Parley integration.
///
/// These fields are separated from [`ParleyPaintStyle`] so paint-only changes
/// can share layout payloads and avoid invalidating shaping or line layout.
#[derive(Clone, Debug, PartialEq)]
pub struct ParleyLayoutStyle {
    /// CSS `font-family` property value.
    pub font_family: FontFamily<'static>,
    /// Font size.
    pub font_size: f32,
    /// Font width.
    pub font_width: FontWidth,
    /// Font style.
    pub font_style: FontStyle,
    /// Font weight.
    pub font_weight: FontWeight,
    /// Font variation settings.
    pub font_variations: FontVariations<'static>,
    /// Font feature settings.
    pub font_features: FontFeatures<'static>,
    /// Locale.
    pub locale: Option<Language>,
    /// Line height.
    pub line_height: LineHeight,
    /// Extra spacing between words.
    pub word_spacing: f32,
    /// Extra spacing between letters.
    pub letter_spacing: f32,
    /// Control over where words can wrap.
    pub word_break: WordBreak,
    /// Control over emergency line breaking.
    pub overflow_wrap: OverflowWrap,
    /// Control over non-emergency line breaking.
    pub text_wrap_mode: TextWrapMode,
}

impl Default for ParleyLayoutStyle {
    fn default() -> Self {
        let style = TextStyle::<()>::default();
        Self {
            font_family: style.font_family,
            font_size: style.font_size,
            font_width: style.font_width,
            font_style: style.font_style,
            font_weight: style.font_weight,
            font_variations: style.font_variations,
            font_features: style.font_features,
            locale: style.locale,
            line_height: style.line_height,
            word_spacing: style.word_spacing,
            letter_spacing: style.letter_spacing,
            word_break: style.word_break,
            overflow_wrap: style.overflow_wrap,
            text_wrap_mode: style.text_wrap_mode,
        }
    }
}

/// Paint-only style payload for the default Parley integration.
///
/// These fields affect rendered glyphs and decorations, but not shaping or line
/// layout.
#[derive(Clone, Debug, PartialEq)]
pub struct ParleyPaintStyle<B: Brush> {
    /// Brush for rendering text.
    pub brush: B,
    /// Underline decoration.
    pub has_underline: bool,
    /// Offset of the underline decoration.
    pub underline_offset: Option<f32>,
    /// Size of the underline decoration.
    pub underline_size: Option<f32>,
    /// Brush for rendering the underline decoration.
    pub underline_brush: Option<B>,
    /// Strikethrough decoration.
    pub has_strikethrough: bool,
    /// Offset of the strikethrough decoration.
    pub strikethrough_offset: Option<f32>,
    /// Size of the strikethrough decoration.
    pub strikethrough_size: Option<f32>,
    /// Brush for rendering the strikethrough decoration.
    pub strikethrough_brush: Option<B>,
}

impl<B: Brush> ParleyPaintStyle<B> {
    /// Creates a paint style with the given text brush and default decorations.
    #[must_use]
    pub fn new(brush: B) -> Self {
        Self {
            brush,
            ..Self::default()
        }
    }
}

impl<B: Brush> Default for ParleyPaintStyle<B> {
    fn default() -> Self {
        let style = TextStyle::<B>::default();
        Self {
            brush: style.brush,
            has_underline: style.has_underline,
            underline_offset: style.underline_offset,
            underline_size: style.underline_size,
            underline_brush: style.underline_brush,
            has_strikethrough: style.has_strikethrough,
            strikethrough_offset: style.strikethrough_offset,
            strikethrough_size: style.strikethrough_size,
            strikethrough_brush: style.strikethrough_brush,
        }
    }
}

/// Partial style patch for the default Parley style payloads.
///
/// A patch is an ordered list of Parley [`StyleProperty`] values. Applying it
/// replays each property in order over the current full style, routing every
/// property to the [`ParleyLayoutStyle`] or [`ParleyPaintStyle`] payload as
/// appropriate. Later properties override earlier ones for the same field.
///
/// Reusing Parley's own [`StyleProperty`] vocabulary means new Parley properties
/// are supported automatically, and properties whose value is itself optional are
/// expressed directly: `StyleProperty::UnderlineOffset(None)` clears the offset,
/// while `StyleProperty::UnderlineOffset(Some(2.0))` sets it.
///
/// Callers that need inheritance, cascading, or a smaller domain-specific patch
/// type can implement [`StylePatch`] directly instead.
///
/// ```
/// use parley::{FontWeight, StyleProperty};
/// use styled_text_parley::ParleyStyleChange;
///
/// let bold_big = ParleyStyleChange::<[u8; 4]>::new()
///     .with(StyleProperty::FontSize(24.0))
///     .with(StyleProperty::FontWeight(FontWeight::BOLD));
/// ```
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ParleyStyleChange<B: Brush> {
    properties: Vec<StyleProperty<'static, B>>,
}

impl<B: Brush> ParleyStyleChange<B> {
    /// Creates an empty patch that changes nothing.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            properties: Vec::new(),
        }
    }

    /// Appends a property and returns the patch, for fluent construction.
    ///
    /// Later properties override earlier ones for the same field.
    #[must_use]
    pub fn with(mut self, property: impl Into<StyleProperty<'static, B>>) -> Self {
        self.properties.push(property.into());
        self
    }

    /// Appends a property in place.
    ///
    /// Later properties override earlier ones for the same field.
    pub fn push(&mut self, property: impl Into<StyleProperty<'static, B>>) -> &mut Self {
        self.properties.push(property.into());
        self
    }

    /// Returns the properties recorded in this patch, in application order.
    #[must_use]
    pub fn properties(&self) -> &[StyleProperty<'static, B>] {
        &self.properties
    }
}

impl<B: Brush> From<StyleProperty<'static, B>> for ParleyStyleChange<B> {
    fn from(property: StyleProperty<'static, B>) -> Self {
        Self::new().with(property)
    }
}

impl<B: Brush> FromIterator<StyleProperty<'static, B>> for ParleyStyleChange<B> {
    fn from_iter<I: IntoIterator<Item = StyleProperty<'static, B>>>(iter: I) -> Self {
        Self {
            properties: iter.into_iter().collect(),
        }
    }
}

impl<B: Brush> StylePatch<ParleyLayoutStyle, ParleyPaintStyle<B>> for ParleyStyleChange<B> {
    fn apply_to(&self, layout: &mut ParleyLayoutStyle, paint: &mut ParleyPaintStyle<B>) {
        for property in &self.properties {
            route_property(property, layout, paint);
        }
    }
}

/// Routes a single [`StyleProperty`] into the layout or paint payload.
///
/// This match is the single source of truth for how Parley's style vocabulary is
/// partitioned into layout-affecting and paint-only fields. [`StyleProperty`] is
/// not `#[non_exhaustive]` and this match has no catch-all arm, so adding a
/// property to Parley becomes a compile error here until it is routed.
fn route_property<B: Brush>(
    property: &StyleProperty<'static, B>,
    layout: &mut ParleyLayoutStyle,
    paint: &mut ParleyPaintStyle<B>,
) {
    match property {
        StyleProperty::FontFamily(font_family) => layout.font_family = font_family.clone(),
        StyleProperty::FontSize(font_size) => layout.font_size = *font_size,
        StyleProperty::FontWidth(font_width) => layout.font_width = *font_width,
        StyleProperty::FontStyle(font_style) => layout.font_style = *font_style,
        StyleProperty::FontWeight(font_weight) => layout.font_weight = *font_weight,
        StyleProperty::FontVariations(font_variations) => {
            layout.font_variations = font_variations.clone();
        }
        StyleProperty::FontFeatures(font_features) => {
            layout.font_features = font_features.clone();
        }
        StyleProperty::Locale(locale) => layout.locale = *locale,
        StyleProperty::LineHeight(line_height) => layout.line_height = *line_height,
        StyleProperty::WordSpacing(word_spacing) => layout.word_spacing = *word_spacing,
        StyleProperty::LetterSpacing(letter_spacing) => layout.letter_spacing = *letter_spacing,
        StyleProperty::WordBreak(word_break) => layout.word_break = *word_break,
        StyleProperty::OverflowWrap(overflow_wrap) => layout.overflow_wrap = *overflow_wrap,
        StyleProperty::TextWrapMode(text_wrap_mode) => layout.text_wrap_mode = *text_wrap_mode,
        StyleProperty::Brush(brush) => paint.brush = brush.clone(),
        StyleProperty::Underline(underline) => paint.has_underline = *underline,
        StyleProperty::UnderlineOffset(underline_offset) => {
            paint.underline_offset = *underline_offset;
        }
        StyleProperty::UnderlineSize(underline_size) => paint.underline_size = *underline_size,
        StyleProperty::UnderlineBrush(underline_brush) => {
            paint.underline_brush = underline_brush.clone();
        }
        StyleProperty::Strikethrough(strikethrough) => paint.has_strikethrough = *strikethrough,
        StyleProperty::StrikethroughOffset(strikethrough_offset) => {
            paint.strikethrough_offset = *strikethrough_offset;
        }
        StyleProperty::StrikethroughSize(strikethrough_size) => {
            paint.strikethrough_size = *strikethrough_size;
        }
        StyleProperty::StrikethroughBrush(strikethrough_brush) => {
            paint.strikethrough_brush = strikethrough_brush.clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use parley::{FontWeight, StyleProperty};
    use styled_text::{StyledSegmentsWorkspace, StyledTextBuilder};

    use super::{ParleyLayoutStyle, ParleyPaintStyle, ParleyStyleChange};

    #[test]
    fn parley_style_change_resolves_default_payloads_independently() {
        let mut builder = StyledTextBuilder::<_, _, ParleyStyleChange<[u8; 4]>>::new(
            ParleyLayoutStyle::default(),
            ParleyPaintStyle::new([0, 0, 0, 255]),
        );
        let all = builder.push("abcd");
        builder.apply(
            all,
            ParleyStyleChange::new()
                .with(StyleProperty::FontSize(24.0))
                .with(StyleProperty::FontWeight(FontWeight::BOLD)),
        );
        builder
            .apply_bytes(
                1..3,
                ParleyStyleChange::from(StyleProperty::Brush([255, 0, 0, 255])),
            )
            .expect("valid range");

        let styled = builder.finish();
        assert_eq!(styled.style_set().style_len(), 3);
        assert_eq!(styled.style_set().layout_len(), 2);
        assert_eq!(styled.style_set().paint_len(), 2);

        let mut workspace = StyledSegmentsWorkspace::new();
        let segments = workspace.segments(&styled).collect::<Vec<_>>();
        assert_eq!(segments.len(), 3);

        let first = styled
            .style_set()
            .get_style(segments[0].style())
            .expect("segment style is interned");
        let second = styled
            .style_set()
            .get_style(segments[1].style())
            .expect("segment style is interned");
        assert_eq!(first.layout_id(), second.layout_id());
        assert_ne!(first.paint_id(), second.paint_id());

        let style = styled.style_set().segment_style(segments[1].style());
        assert_eq!(style.layout().font_size, 24.0);
        assert_eq!(style.layout().font_weight, FontWeight::BOLD);
        assert_eq!(style.paint().brush, [255, 0, 0, 255]);
    }

    #[test]
    fn patch_records_properties_in_order() {
        let patch = ParleyStyleChange::<[u8; 4]>::new()
            .with(StyleProperty::FontSize(12.0))
            .with(StyleProperty::FontSize(18.0));
        assert_eq!(
            patch.properties(),
            &[StyleProperty::FontSize(12.0), StyleProperty::FontSize(18.0),]
        );

        let from_iter = [
            StyleProperty::Underline(true),
            StyleProperty::FontSize(10.0),
        ]
        .into_iter()
        .collect::<ParleyStyleChange<[u8; 4]>>();
        assert_eq!(from_iter.properties().len(), 2);
    }

    #[test]
    fn route_property_assigns_layout_and_paint_fields() {
        use styled_text::StylePatch;

        let mut layout = ParleyLayoutStyle::default();
        let mut paint = ParleyPaintStyle::<[u8; 4]>::default();

        ParleyStyleChange::new()
            .with(StyleProperty::FontSize(20.0))
            .with(StyleProperty::Underline(true))
            .with(StyleProperty::UnderlineOffset(Some(2.0)))
            .with(StyleProperty::UnderlineOffset(None))
            .apply_to(&mut layout, &mut paint);

        assert_eq!(layout.font_size, 20.0);
        assert!(paint.has_underline);
        // Last writer wins: the clearing `None` overrides the earlier `Some`.
        assert_eq!(paint.underline_offset, None);
    }
}
