// Copyright 2021 the Parley Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use super::layout::Layout;
use super::resolve::{RangedStyle, ResolveContext, Resolved};
use super::style::{Brush, FontFeature, FontVariation};
use crate::Font;
use crate::util::nearly_eq;
use fontique::QueryFamily;
use fontique::{self, Query, QueryFont};
use swash::shape::{Direction, ShapeContext, partition};
use swash::text::cluster::{CharCluster, CharInfo, Token};
use swash::text::{Language, Script};
use swash::{FontRef, Synthesis};

use alloc::vec::Vec;

use crate::inline_box::InlineBox;

struct Item {
    style_index: u16,
    size: f32,
    script: Script,
    level: u8,
    locale: Option<Language>,
    variations: Resolved<FontVariation>,
    features: Resolved<FontFeature>,
    word_spacing: f32,
    letter_spacing: f32,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn shape_text<'a, B: Brush>(
    rcx: &'a ResolveContext,
    mut fq: Query<'a>,
    styles: &'a [RangedStyle<B>],
    inline_boxes: &[InlineBox],
    infos: &[(CharInfo, u16)],
    levels: &[u8],
    scx: &mut ShapeContext,
    mut text: &str,
    layout: &mut Layout<B>,
) {
    // If we have both empty text and no inline boxes, shape with a fake space
    // to generate metrics that can be used to size a cursor.
    if text.is_empty() && inline_boxes.is_empty() {
        text = " ";
    }
    // Do nothing if there is no text or styles (there should always be a default style)
    if text.is_empty() || styles.is_empty() {
        // Process any remaining inline boxes whose index is greater than the length of the text
        for box_idx in 0..inline_boxes.len() {
            // Push the box to the list of items
            layout.data.push_inline_box(box_idx);
        }
        return;
    }

    // Setup mutable state for iteration
    let mut style = &styles[0].style;
    let mut item = Item {
        style_index: 0,
        size: style.font_size,
        level: levels.first().copied().unwrap_or(0),
        script: infos
            .iter()
            .map(|x| x.0.script())
            .find(|&script| real_script(script))
            .unwrap_or(Script::Latin),
        locale: style.locale,
        variations: style.font_variations,
        features: style.font_features,
        word_spacing: style.word_spacing,
        letter_spacing: style.letter_spacing,
    };
    let mut char_range = 0..0;
    let mut text_range = 0..0;

    let mut inline_box_iter = inline_boxes.iter().enumerate();
    let mut current_box = inline_box_iter.next();
    let mut deferred_boxes: Vec<usize> = Vec::with_capacity(16);

    // Define macro to shape
    macro_rules! shape_item {
        () => {
            let item_text = &text[text_range.clone()];
            let item_infos = &infos[char_range.start..];
            let first_style_index = item_infos[0].1;
            let mut fs = FontSelector::new(
                &mut fq,
                rcx,
                styles,
                first_style_index,
                item.script,
                item.locale,
            );
            let options = partition::SimpleShapeOptions {
                size: item.size,
                script: item.script,
                language: item.locale,
                direction: if item.level & 1 != 0 {
                    Direction::RightToLeft
                } else {
                    Direction::LeftToRight
                },
                variations: rcx.variations(item.variations).unwrap_or(&[]),
                features: rcx.features(item.features).unwrap_or(&[]),
                insert_dotted_circles: false,
            };
            partition::shape(
                scx,
                &mut fs,
                &options,
                item_text.char_indices().zip(item_infos).map(
                    |((offset, ch), (info, style_index))| Token {
                        ch,
                        offset: (text_range.start + offset) as u32,
                        len: ch.len_utf8() as u8,
                        info: *info,
                        data: *style_index as _,
                    },
                ),
                |font, shaper| {
                    layout.data.push_run(
                        Font::new(font.font.blob.clone(), font.font.index),
                        item.size,
                        font.synthesis,
                        shaper,
                        item.level,
                        item.word_spacing,
                        item.letter_spacing,
                    );
                },
            );
        };
    }

    // Iterate over characters in the text
    for ((char_index, (byte_index, ch)), (info, style_index)) in
        text.char_indices().enumerate().zip(infos)
    {
        let mut break_run = false;
        let mut script = info.script();
        if !real_script(script) {
            script = item.script;
        }
        let level = levels.get(char_index).copied().unwrap_or(0);
        if item.style_index != *style_index {
            item.style_index = *style_index;
            style = &styles[*style_index as usize].style;
            if !nearly_eq(style.font_size, item.size)
                || style.locale != item.locale
                || style.font_variations != item.variations
                || style.font_features != item.features
                || !nearly_eq(style.letter_spacing, item.letter_spacing)
                || !nearly_eq(style.word_spacing, item.word_spacing)
            {
                break_run = true;
            }
        }

        if level != item.level || script != item.script {
            break_run = true;
        }

        // Check if there is an inline box at this index
        // Note:
        //   - We loop because there may be multiple boxes at this index
        //   - We do this *before* processing the text run because we need to know whether we should
        //     break the run due to the presence of an inline box.
        while let Some((box_idx, inline_box)) = current_box {
            // println!("{} {}", byte_index, inline_box.index);

            if inline_box.index == byte_index {
                break_run = true;
                deferred_boxes.push(box_idx);
                // Update the current box to the next box
                current_box = inline_box_iter.next();
            } else {
                break;
            }
        }

        if break_run && !text_range.is_empty() {
            shape_item!();
            item.size = style.font_size;
            item.level = level;
            item.script = script;
            item.locale = style.locale;
            item.variations = style.font_variations;
            item.features = style.font_features;
            text_range.start = text_range.end;
            char_range.start = char_range.end;
        }

        for box_idx in deferred_boxes.drain(0..) {
            // Push the box to the list of items
            layout.data.push_inline_box(box_idx);
        }

        text_range.end += ch.len_utf8();
        char_range.end += 1;
    }

    if !text_range.is_empty() {
        shape_item!();
    }

    // Process any remaining inline boxes whose index is greater than the length of the text
    if let Some((box_idx, _inline_box)) = current_box {
        layout.data.push_inline_box(box_idx);
    }
    for (box_idx, _inline_box) in inline_box_iter {
        layout.data.push_inline_box(box_idx);
    }
}

fn real_script(script: Script) -> bool {
    script != Script::Common && script != Script::Unknown && script != Script::Inherited
}

struct FontSelector<'a, 'b, B: Brush> {
    query: &'b mut Query<'a>,
    fonts_id: Option<usize>,
    rcx: &'a ResolveContext,
    styles: &'a [RangedStyle<B>],
    style_index: u16,
    attrs: fontique::Attributes,
    variations: &'a [FontVariation],
    features: &'a [FontFeature],
}

impl<'a, 'b, B: Brush> FontSelector<'a, 'b, B> {
    fn new(
        query: &'b mut Query<'a>,
        rcx: &'a ResolveContext,
        styles: &'a [RangedStyle<B>],
        style_index: u16,
        script: Script,
        locale: Option<Language>,
    ) -> Self {
        let style = &styles[style_index as usize].style;
        let fonts_id = style.font_stack.id();
        let fonts = rcx.stack(style.font_stack).unwrap_or(&[]);
        let attrs = fontique::Attributes {
            width: style.font_width,
            weight: style.font_weight,
            style: style.font_style,
        };
        let variations = rcx.variations(style.font_variations).unwrap_or(&[]);
        let features = rcx.features(style.font_features).unwrap_or(&[]);
        query.set_families(fonts.iter().copied());

        let fb_script = crate::swash_convert::script_to_fontique(script);
        let fb_language = locale.and_then(crate::swash_convert::locale_to_fontique);
        query.set_fallbacks(fontique::FallbackKey::new(fb_script, fb_language.as_ref()));
        query.set_attributes(attrs);

        Self {
            query,
            fonts_id: Some(fonts_id),
            rcx,
            styles,
            style_index,
            attrs,
            variations,
            features,
        }
    }
}

impl<B: Brush> partition::Selector for FontSelector<'_, '_, B> {
    type SelectedFont = SelectedFont;

    fn select_font(&mut self, cluster: &mut CharCluster) -> Option<Self::SelectedFont> {
        let style_index = cluster.user_data() as u16;
        let is_emoji = cluster.info().is_emoji();
        if style_index != self.style_index || is_emoji || self.fonts_id.is_none() {
            self.style_index = style_index;
            let style = &self.styles[style_index as usize].style;

            let fonts_id = style.font_stack.id();
            let fonts = self.rcx.stack(style.font_stack).unwrap_or(&[]);
            let fonts = fonts.iter().copied().map(QueryFamily::Id);
            if is_emoji {
                use core::iter::once;
                let emoji_family = QueryFamily::Generic(fontique::GenericFamily::Emoji);
                self.query.set_families(fonts.chain(once(emoji_family)));
                self.fonts_id = None;
            } else if self.fonts_id != Some(fonts_id) {
                self.query.set_families(fonts);
                self.fonts_id = Some(fonts_id);
            }

            let attrs = fontique::Attributes {
                width: style.font_width,
                weight: style.font_weight,
                style: style.font_style,
            };
            if self.attrs != attrs {
                self.query.set_attributes(attrs);
                self.attrs = attrs;
            }
            self.variations = self.rcx.variations(style.font_variations).unwrap_or(&[]);
            self.features = self.rcx.features(style.font_features).unwrap_or(&[]);
        }
        let mut selected_font = None;
        self.query.matches_with(|font| {
            use skrifa::MetadataProvider;
            use swash::text::cluster::Status as MapStatus;

            let Ok(font_ref) = skrifa::FontRef::from_index(font.blob.as_ref(), font.index) else {
                return fontique::QueryStatus::Continue;
            };

            let charmap = font_ref.charmap();
            let map_status = cluster.map(|ch| {
                charmap
                    .map(ch)
                    .map(|g| {
                        g.to_u32()
                            .try_into()
                            .expect("Swash requires u16 glyph, so we hope that the glyph fits")
                    })
                    .unwrap_or_default()
            });

            match map_status {
                MapStatus::Complete => {
                    selected_font = Some(font.into());
                    fontique::QueryStatus::Stop
                }
                MapStatus::Keep => {
                    selected_font = Some(font.into());
                    fontique::QueryStatus::Continue
                }
                MapStatus::Discard => {
                    if selected_font.is_none() {
                        selected_font = Some(font.into());
                    }
                    fontique::QueryStatus::Continue
                }
            }
        });
        selected_font
    }
}

struct SelectedFont {
    font: QueryFont,
    synthesis: Synthesis,
}

impl From<&QueryFont> for SelectedFont {
    fn from(font: &QueryFont) -> Self {
        use crate::swash_convert::synthesis_to_swash;
        Self {
            font: font.clone(),
            synthesis: synthesis_to_swash(font.synthesis),
        }
    }
}

impl PartialEq for SelectedFont {
    fn eq(&self, other: &Self) -> bool {
        self.font.family == other.font.family && self.synthesis == other.synthesis
    }
}

impl partition::SelectedFont for SelectedFont {
    fn font(&self) -> FontRef<'_> {
        FontRef::from_index(self.font.blob.as_ref(), self.font.index as _).unwrap()
    }

    fn id_override(&self) -> Option<[u64; 2]> {
        Some([self.font.blob.id(), self.font.index as _])
    }

    fn synthesis(&self) -> Option<Synthesis> {
        Some(self.synthesis)
    }
}
