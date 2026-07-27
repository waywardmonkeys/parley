// Copyright 2026 the Parley Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Shaping of text.

use alloc::vec::Vec;
use core::{mem, ops::Range};
use harfrust::{BufferFlags, ShapeOptions as HarfShapeOptions};
use linebender_resource_handle::FontData;
use parlance::{FontFeature, FontVariation, Language};

use crate::{
    Analysis, CharInfo, Glyph, ShapedText,
    itemize::{Item, TextRange},
    lru_cache::LruCache,
    shape::{
        CharCluster, ClusterData, cache,
        shaped_text::{Direction, process_clusters},
    },
};

/// Shaping options for one item.
///
/// These are styling options relevant for shaping. They're styling, in that they're not derived
/// from the underlying text. When you [itemize][`Analysis::itemize`] the text, you should split the
/// text at points where these options change.
#[derive(Debug)]
pub struct ShapeOptions<'a> {
    /// The font size to shape the item with.
    pub font_size: f32,
    /// The language to shape the item with.
    pub language: Option<Language>,
    /// The font features to shape the item with.
    pub features: &'a [FontFeature],
    /// The font variations that are constant over an item.
    pub variations: &'a [FontVariation],
    /// The per-character style indices.
    // TODO: rename to something like `user_data` (s.t. we don't assume it's a style per se).
    pub char_style_indices: &'a [u16],
}

/// The font instance to shape an item with.
#[derive(Clone, Debug, PartialEq)]
pub struct FontInstance {
    /// The font.
    pub font: FontData,
    /// Font synthesis suggestions.
    // TODO: Synthesis carries more than we need, and ties us to `fontique`. We can likely change
    // this to opaque user data.
    pub synthesis: fontique::Synthesis,
}

/// Reusable scratch to shape [items][`Item`] into shaped text using [`Self::shape_item`].
pub struct Shaper {
    shape_data_cache: LruCache<cache::ShapeDataKey, harfrust::ShaperData>,
    shape_instance_cache: LruCache<cache::ShapeInstanceId, harfrust::ShaperInstance>,
    shape_plan_cache: LruCache<cache::ShapePlanId, harfrust::ShapePlan>,
    unicode_buffer: Option<harfrust::UnicodeBuffer>,
    features: Vec<harfrust::Feature>,
    char_cluster: CharCluster,
    reshape_clusters: Vec<ClusterData>,
    reshape_glyphs: Vec<Glyph>,
}

impl Default for Shaper {
    fn default() -> Self {
        const MAX_ENTRIES: usize = 16;
        Self {
            shape_data_cache: LruCache::new(MAX_ENTRIES),
            shape_instance_cache: LruCache::new(MAX_ENTRIES),
            shape_plan_cache: LruCache::new(MAX_ENTRIES),
            unicode_buffer: Some(harfrust::UnicodeBuffer::new()),
            features: Vec::new(),
            char_cluster: CharCluster::default(),
            reshape_clusters: Vec::new(),
            reshape_glyphs: Vec::new(),
        }
    }
}

impl core::fmt::Debug for Shaper {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Shaper").finish_non_exhaustive()
    }
}

impl Shaper {
    /// Shape an [`Item`] produced by [`Analysis::itemize`] into glyphs.
    ///
    /// The item is broken into runs of maximal sequences of character clusters for which
    /// `select_font` returns the same font. The resulting shaped runs are appended to
    /// `shaped_text`.
    ///
    /// `text` must be the same text as originally passed to create [`Analysis`]. `item` must be an
    /// [`Item`] produced by [`Analysis::itemize`] on this text's analysis.
    ///
    /// The `select_font` callback should return the font to shape `char_cluster` with. If
    /// consecutive character clusters select a different font, they become separately-shaped runs.
    ///
    /// Returns the index range of runs appended to `shaped_text`.
    ///
    /// # Panics
    ///
    /// Panics if the font returned by `select_font` isn't a parseable font.
    ///
    // TODO: For `select_font`, on `None`, the previous font is taken (and the run is dropped if
    // `None` is returned on the first call). This is identical to Parley's old behavior, but we
    // probably want the commented-out documented behavior that follows, as returning a font is
    // cheap and it probably doesn't make a ton of sense to hardcode some font fallback behavior
    // here.
    //
    // /// Return `None` if there are no fonts available at all. The character cluster's text will be
    // /// omitted from the shaped result. Instead, you probably want to render a `.notdef` glyph from a
    // /// font you do have available, in which case you can return the previous font or some
    // /// last-resort fallback font instead.
    pub fn shape_item(
        &mut self,
        text: &str,
        analysis: &Analysis,
        item: &Item,
        options: &ShapeOptions<'_>,
        select_font: impl FnMut(&mut CharCluster) -> Option<FontInstance>,
        shaped_text: &mut ShapedText,
    ) -> Range<usize> {
        shaped_text.reserve(item.range.char_range.len());

        let start = shaped_text.runs().len();
        shape_item(
            self,
            text,
            item,
            options,
            select_font,
            analysis.char_info(),
            shaped_text,
        );
        start..shaped_text.runs().len()
    }

    /// Commits a line break at byte offset `pos`, reshaping only the bounded unsafe region on
    /// each side of the break.
    ///
    /// This is a no-op when `pos` is already break-safe or is not a cluster boundary. Undo a
    /// committed break with [`Self::apply_concat`].
    pub fn apply_break(
        &mut self,
        text: &str,
        analysis: &Analysis,
        shaped_text: &mut ShapedText,
        pos: usize,
    ) {
        let ranges = shaped_text.unsafe_break_region(pos);
        shaped_text.record_break(pos, ranges.tail.start..ranges.head.end);
        if !ranges.tail.is_empty() {
            self.reshape_fragment(text, analysis, shaped_text, ranges.tail);
        }
        if !ranges.head.is_empty() {
            self.reshape_fragment(text, analysis, shaped_text, ranges.head);
        }
    }

    /// Joins fragments at byte offset `pos`, reshaping only the bounded concat-unsafe region.
    ///
    /// This reverses [`Self::apply_break`] by restoring its bounded pre-break fragment. For other
    /// concat-unsafe boundaries it performs bounded reshaping. It is a no-op when `pos` is already
    /// concat-safe or is not a cluster boundary.
    pub fn apply_concat(
        &mut self,
        text: &str,
        analysis: &Analysis,
        shaped_text: &mut ShapedText,
        pos: usize,
    ) {
        if shaped_text.restore_break(pos) {
            return;
        }
        let range = shaped_text.unsafe_concat_region(pos);
        if !range.is_empty() {
            self.reshape_fragment(text, analysis, shaped_text, range);
        }
    }

    fn reshape_fragment(
        &mut self,
        text: &str,
        analysis: &Analysis,
        shaped_text: &mut ShapedText,
        text_range: Range<usize>,
    ) {
        let Some(target) = shaped_text.reshape_locate(text_range.clone()) else {
            return;
        };
        let Some(context) = shaped_text.reshape_context(&target) else {
            return;
        };
        let run = context.run;
        let font = context.font;
        let char_info = &analysis.char_info()[target.char_range.clone()];
        let fragment_text = &text[text_range];

        let font_ref =
            harfrust::FontRef::from_index(font.font.data.as_ref(), font.font.index).unwrap();
        let instance = harfrust::ShaperInstance::from_coords(
            &font_ref,
            context
                .normalized_coords
                .iter()
                .map(|coord| harfrust::NormalizedCoord::from_bits(coord.to_bits())),
        );
        let direction = if run.bidi_level & 1 == 0 {
            harfrust::Direction::LeftToRight
        } else {
            harfrust::Direction::RightToLeft
        };
        let script = script_to_harfrust(context.script);
        let language = context
            .language
            .as_ref()
            .and_then(|lang| lang.language().parse::<harfrust::Language>().ok());
        self.features.clear();
        self.features.extend(context.features.iter().map(|feature| {
            harfrust::Feature::new(
                harfrust::Tag::new(&feature.tag.to_bytes()),
                u32::from(feature.value),
                ..,
            )
        }));

        let shaper_data = self.shape_data_cache.entry(
            cache::ShapeDataKey::new(font.font.data.id(), font.font.index),
            || harfrust::ShaperData::new(&font_ref),
        );
        let harf_shaper = shaper_data
            .shaper(&font_ref)
            .instance(Some(&instance))
            .build();
        let plan = harfrust::ShapePlan::new(
            &harf_shaper,
            direction,
            Some(script),
            language.as_ref(),
            &self.features,
        );
        let units_per_em = harf_shaper.units_per_em();

        let mut buffer = mem::take(&mut self.unicode_buffer).unwrap();
        buffer.clear();
        buffer.reserve(fragment_text.len());
        #[expect(
            clippy::cast_possible_truncation,
            reason = "Text length is already u16-limited"
        )]
        for (index, ch) in fragment_text.chars().enumerate() {
            buffer.add(ch, index as u32);
        }
        buffer.set_direction(direction);
        buffer.set_script(script);
        if let Some(language) = language {
            buffer.set_language(language);
        }
        buffer.set_flags(
            BufferFlags::PRODUCE_UNSAFE_TO_CONCAT | BufferFlags::PRODUCE_SAFE_TO_INSERT_TATWEEL,
        );
        let glyph_buffer = harf_shaper.shape(
            buffer,
            HarfShapeOptions::new()
                .plan(Some(&plan))
                .features(&self.features)
                .point_size(Some(run.font_size)),
        );

        let mut clusters = mem::take(&mut self.reshape_clusters);
        let mut glyphs = mem::take(&mut self.reshape_glyphs);
        clusters.clear();
        glyphs.clear();
        let scale_factor = run.font_size / units_per_em as f32;
        if direction == harfrust::Direction::LeftToRight {
            process_clusters(
                Direction::Ltr,
                &mut clusters,
                &mut glyphs,
                scale_factor,
                glyph_buffer.glyph_infos(),
                glyph_buffer.glyph_positions(),
                char_info,
                &context.char_style_indices,
                fragment_text.char_indices(),
            );
        } else {
            process_clusters(
                Direction::Rtl,
                &mut clusters,
                &mut glyphs,
                scale_factor,
                glyph_buffer.glyph_infos(),
                glyph_buffer.glyph_positions(),
                char_info,
                &context.char_style_indices,
                fragment_text.char_indices().rev(),
            );
            clusters.reverse();
        }
        shaped_text.splice_fragment(&target, &clusters, &glyphs);

        self.unicode_buffer = Some(glyph_buffer.clear());
        clusters.clear();
        glyphs.clear();
        self.reshape_clusters = clusters;
        self.reshape_glyphs = glyphs;
    }
}

fn shape_item(
    scx: &mut Shaper,
    text: &str,
    item: &Item,
    options: &ShapeOptions<'_>,
    mut select_font: impl FnMut(&mut CharCluster) -> Option<FontInstance>,
    char_info: &[CharInfo],
    shaped_text: &mut ShapedText,
) {
    let text_range = &item.range.byte_range;
    let char_range = &item.range.char_range;

    let item_text = &text[text_range.clone()];

    // Only process current item
    let item_char_info = &char_info[char_range.start..char_range.end];
    let item_char_style_indices = &options.char_style_indices[char_range.start..char_range.end];

    if item_text.is_empty() {
        return; // No clusters
    }

    let mut item_infos_iter = item_char_info
        .iter()
        .copied()
        .zip(item_char_style_indices.iter().copied());
    let mut code_unit_offset_in_string = text_range.start;
    let char_cluster = &mut scx.char_cluster;

    // Build an iterator of boundaries and consume the first segment to seed the loop
    let mut boundaries_iter = item_text
        .char_indices()
        .zip(item_char_info.iter())
        .skip(1)
        .filter_map(|((byte_pos, _), info)| info.is_grapheme_start().then_some(byte_pos))
        .chain(core::iter::once(item_text.len()));
    let mut last_boundary = 0_usize;
    let mut current_boundary = boundaries_iter.next().unwrap();

    char_cluster.fill(
        &item_text[last_boundary..current_boundary],
        &mut item_infos_iter,
        &mut code_unit_offset_in_string,
    );

    let mut current_font = select_font(char_cluster);

    // Main segmentation loop (based on swash shape_clusters) - only within current item
    while let Some(font) = current_font.take() {
        // Collect all clusters for this font segment
        let cluster_range = char_cluster.range();
        let segment_start_offset = cluster_range.start as usize - text_range.start;
        let mut segment_end_offset = cluster_range.end as usize - text_range.start;

        for next_boundary in boundaries_iter.by_ref() {
            // Build next cluster in-place
            last_boundary = current_boundary;
            current_boundary = next_boundary;
            char_cluster.fill(
                &item_text[last_boundary..current_boundary],
                &mut item_infos_iter,
                &mut code_unit_offset_in_string,
            );

            if let Some(next_font) = select_font(char_cluster) {
                if next_font != font {
                    current_font = Some(next_font);
                    break;
                } else {
                    // Same font - add to current segment
                    segment_end_offset = char_cluster.range().end as usize - text_range.start;
                }
            } else {
                // No font determined, continue to next cluster
                continue;
            }
        }

        // Shape this font segment with harfrust
        let segment_text = &item_text[segment_start_offset..segment_end_offset];
        // Shape the entire segment text including newlines
        // The line breaking algorithm will handle newlines automatically

        // TODO: How do we want to handle errors like this?
        let font_ref =
            harfrust::FontRef::from_index(font.font.data.as_ref(), font.font.index).unwrap();

        // Create harfrust shaper
        let shaper_data = scx.shape_data_cache.entry(
            cache::ShapeDataKey::new(font.font.data.id(), font.font.index),
            || harfrust::ShaperData::new(&font_ref),
        );
        let instance = scx.shape_instance_cache.entry(
            cache::ShapeInstanceKey::new(
                font.font.data.id(),
                font.font.index,
                &font.synthesis,
                Some(options.variations),
            ),
            || {
                harfrust::ShaperInstance::from_variations(
                    &font_ref,
                    variations_iter(&font.synthesis, options.variations),
                )
            },
        );

        let direction = if item.bidi_level & 1 != 0 {
            harfrust::Direction::RightToLeft
        } else {
            harfrust::Direction::LeftToRight
        };
        let hb_script = script_to_harfrust(item.script);
        let language = options
            .language
            .as_ref()
            .and_then(|lang| lang.language().parse::<harfrust::Language>().ok());
        scx.features.clear();
        for feature in options.features {
            scx.features.push(harfrust::Feature::new(
                harfrust::Tag::new(&feature.tag.to_bytes()),
                feature.value as u32,
                ..,
            ));
        }
        let harf_shaper = shaper_data
            .shaper(&font_ref)
            .instance(Some(instance))
            .build();
        let shaper_plan = scx.shape_plan_cache.entry(
            cache::ShapePlanKey::new(
                font.font.data.id(),
                font.font.index,
                &font.synthesis,
                direction,
                hb_script,
                language.clone(),
                &scx.features,
                Some(options.variations),
            ),
            || {
                harfrust::ShapePlan::new(
                    &harf_shaper,
                    direction,
                    Some(hb_script),
                    language.as_ref(),
                    &scx.features,
                )
            },
        );

        // Prepare harfrust buffer
        let mut buffer = mem::take(&mut scx.unicode_buffer).unwrap();
        buffer.clear();

        // Use the entire segment text including newlines
        buffer.reserve(segment_text.len());
        #[expect(clippy::cast_possible_truncation, reason = "Deferred")]
        for (i, ch) in segment_text.chars().enumerate() {
            // Ensure that each cluster's index matches the index into `infos`. This is required
            // for efficient cluster lookup within `data.rs`.
            //
            // In other words, instead of using `buffer.push_str`, which iterates `segment_text`
            // with `char_indices`, push each char individually via `.chars` with a cluster index
            // that matches its `infos` counterpart. This allows us to lookup `infos` via cluster
            // index in `data.rs`.
            buffer.add(ch, i as u32);
        }

        buffer.set_direction(direction);

        buffer.set_script(hb_script);

        if let Some(lang) = language {
            buffer.set_language(lang);
        }
        buffer.set_flags(
            BufferFlags::PRODUCE_UNSAFE_TO_CONCAT | BufferFlags::PRODUCE_SAFE_TO_INSERT_TATWEEL,
        );

        let glyph_buffer = harf_shaper.shape(
            buffer,
            HarfShapeOptions::new()
                .plan(Some(shaper_plan))
                .features(&scx.features)
                .point_size(Some(options.font_size)),
        );

        let char_start = char_range.start + item_text[..segment_start_offset].chars().count();
        let segment_char_count = segment_text.chars().count();
        let range = TextRange {
            byte_range: (item.range.byte_range.start + segment_start_offset)
                ..(item.range.byte_range.start + segment_end_offset),
            char_range: char_start..char_start + segment_char_count,
        };
        shaped_text.push_run(
            text,
            range,
            item,
            options,
            char_info,
            &font,
            &glyph_buffer,
            harf_shaper.coords(),
        );

        // Replace buffer to reuse allocation in next iteration.
        scx.unicode_buffer = Some(glyph_buffer.clear());
    }
}

#[inline]
fn variations_iter<'a>(
    synthesis: &'a fontique::Synthesis,
    item: &'a [FontVariation],
) -> impl Iterator<Item = harfrust::Variation> + 'a {
    synthesis
        .variation_settings()
        .iter()
        .map(|(tag, value)| harfrust::Variation {
            tag: *tag,
            value: *value,
        })
        .chain(item.iter().map(|variation| harfrust::Variation {
            tag: harfrust::Tag::new(&variation.tag.to_bytes()),
            value: variation.value,
        }))
}

pub(crate) fn script_to_harfrust(script: fontique::Script) -> harfrust::Script {
    harfrust::Script::from_iso15924_tag(harfrust::Tag::new(&script.to_bytes()))
        .unwrap_or(harfrust::script::UNKNOWN)
}

#[cfg(test)]
mod tests {
    use alloc::{sync::Arc, vec, vec::Vec};

    use fontique::Synthesis;
    use linebender_resource_handle::{Blob, FontData};

    use crate::{Analysis, AnalysisOptions, Analyzer, ShapedText};

    use super::{FontInstance, ShapeOptions, Shaper};

    const ROBOTO: &[u8] =
        include_bytes!("../../../parley_dev/assets/fonts/roboto_fonts/Roboto-Regular.ttf");
    const NOTO_ARABIC: &[u8] =
        include_bytes!("../../../parley_dev/assets/fonts/noto_fonts/NotoKufiArabic-Regular.otf");

    fn shape(text: &str, font_data: &'static [u8]) -> (Analysis, Shaper, ShapedText) {
        let mut analysis = Analysis::new();
        Analyzer::new().analyze(
            text,
            &AnalysisOptions {
                word_break: &[],
                line_break_override: None,
                ..AnalysisOptions::default()
            },
            &mut analysis,
        );
        let font = FontInstance {
            font: FontData::new(Blob::new(Arc::new(font_data)), 0),
            synthesis: Synthesis::default(),
        };
        let char_style_indices = vec![0; text.chars().count()];
        let mut shaper = Shaper::default();
        let mut shaped = ShapedText::new();
        for item in analysis.itemize(text, |_| false) {
            shaper.shape_item(
                text,
                &analysis,
                &item,
                &ShapeOptions {
                    font_size: 32.0,
                    language: None,
                    features: &[],
                    variations: &[],
                    char_style_indices: &char_style_indices,
                },
                |_| Some(font.clone()),
                &mut shaped,
            );
        }
        (analysis, shaper, shaped)
    }

    fn unsafe_breaks(text: &str, shaped: &ShapedText) -> Vec<usize> {
        text.char_indices()
            .skip(1)
            .map(|(pos, _)| pos)
            .filter(|&pos| !shaped.unsafe_break_region(pos).is_empty())
            .collect()
    }

    fn check_invariants(text: &str, shaped: &ShapedText) {
        let mut next_byte = 0;
        let mut next_char = 0;
        let mut next_cluster = 0;
        let mut next_glyph = 0;
        for run in shaped.runs() {
            assert_eq!(run.range.byte_range.start, next_byte);
            assert_eq!(run.range.char_range.start, next_char);
            assert_eq!(run.clusters_range.start, next_cluster);
            assert_eq!(run.glyphs_range.start, next_glyph);

            let clusters = &shaped.clusters()[run.clusters_range.clone()];
            assert_eq!(clusters.len(), run.range.char_range.len());
            let mut source = run.range.byte_range.start;
            let mut advance = 0.0_f32;
            for cluster in clusters {
                assert_eq!(
                    run.range.byte_range.start + usize::from(cluster.text_offset),
                    source
                );
                source += usize::from(cluster.text_len);
                assert!(text.is_char_boundary(source));
                advance += cluster.advance;
            }
            assert_eq!(source, run.range.byte_range.end);
            assert!((advance - run.advance).abs() < 0.01);

            next_byte = run.range.byte_range.end;
            next_char = run.range.char_range.end;
            next_cluster = run.clusters_range.end;
            next_glyph = run.glyphs_range.end;
        }
        assert_eq!(next_byte, text.len());
        assert_eq!(next_char, text.chars().count());
        assert_eq!(next_cluster, shaped.clusters().len());
        assert_eq!(next_glyph, shaped.glyphs().len());
    }

    #[test]
    fn arabic_break_reshapes_and_concat_restores() {
        let text = "سلام";
        let (analysis, mut shaper, base) = shape(text, NOTO_ARABIC);
        let pos = *unsafe_breaks(text, &base)
            .first()
            .expect("Arabic cursive joining has an unsafe interior break");
        check_invariants(text, &base);

        let mut broken = base.clone();
        shaper.apply_break(text, &analysis, &mut broken, pos);
        assert_ne!(broken, base, "committing the break changes shaped output");
        check_invariants(text, &broken);

        shaper.apply_concat(text, &analysis, &mut broken, pos);
        assert_eq!(broken, base, "concatenating restores the original shaping");
        check_invariants(text, &broken);
    }

    #[test]
    fn arabic_break_across_zero_width_space_concat_restores() {
        let text = "س سل\u{200b}ام";
        let (analysis, mut shaper, base) = shape(text, NOTO_ARABIC);
        let pos = text.find("ام").unwrap();
        assert!(
            !base.unsafe_break_region(pos).is_empty(),
            "joining across a legal zero-width break is unsafe"
        );

        let mut broken = base.clone();
        shaper.apply_break(text, &analysis, &mut broken, pos);
        assert_ne!(broken, base, "committing the break changes shaped output");
        check_invariants(text, &broken);

        shaper.apply_concat(text, &analysis, &mut broken, pos);
        assert_eq!(
            broken, base,
            "concatenating must restore joining across the default-ignorable separator"
        );
        check_invariants(text, &broken);
    }

    #[test]
    fn latin_ligature_break_reshapes_and_concat_restores() {
        let text = "office";
        let (analysis, mut shaper, base) = shape(text, ROBOTO);
        let pos = text.find("fi").unwrap() + 1;
        assert!(
            !base.unsafe_break_region(pos).is_empty(),
            "the boundary inside the fi ligature is unsafe"
        );

        let mut broken = base.clone();
        shaper.apply_break(text, &analysis, &mut broken, pos);
        assert_ne!(broken, base, "committing the break decomposes the ligature");
        check_invariants(text, &broken);

        shaper.apply_concat(text, &analysis, &mut broken, pos);
        assert_eq!(broken, base, "concatenating restores the fi ligature");
        check_invariants(text, &broken);
    }

    #[test]
    fn safe_break_and_concat_are_noops() {
        let text = "hello world";
        let (analysis, mut shaper, base) = shape(text, ROBOTO);
        let pos = text.find("world").unwrap();
        assert!(base.unsafe_break_region(pos).is_empty());

        let mut shaped = base.clone();
        shaper.apply_break(text, &analysis, &mut shaped, pos);
        shaper.apply_concat(text, &analysis, &mut shaped, pos);
        assert_eq!(shaped, base);
    }
}
