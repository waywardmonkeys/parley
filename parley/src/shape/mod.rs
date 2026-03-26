// Copyright 2021 the Parley Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Text shaping implementation using `harfrust`for shaping
//! and `icu` for text analysis.

use alloc::vec::Vec;
use core::mem;
use core::ops::RangeInclusive;
use harfrust::ShapeOptions;

use super::layout::{Glyph, RunMetrics};
use super::resolve::{ResolveContext, Resolved, ResolvedStyle};
use super::style::{Brush, FontFeature, FontVariation};
use crate::analysis::cluster::{Char, CharCluster, Status, Whitespace};
use crate::analysis::{AnalysisDataSources, CharInfo};
use crate::convert::script_to_harfrust;
use crate::inline_box::InlineBox;
use crate::layout::data::{ClusterData, ShapedParagraph, ShapedParagraphSink};
use crate::lru_cache::LruCache;
use crate::pipeline::{ShapeCluster, ShapeClusterGlyphs, ShapeRun, ShapeSink};
use crate::util::nearly_eq;
use crate::{FontData, convert};
use fontique::Language;
use icu_properties::props::Script;

use fontique::{self, Query, QueryFamily, QueryFont};

mod cache;

pub(crate) struct ShapeContext {
    shape_data_cache: LruCache<cache::ShapeDataKey, harfrust::ShaperData>,
    shape_instance_cache: LruCache<cache::ShapeInstanceId, harfrust::ShaperInstance>,
    shape_plan_cache: LruCache<cache::ShapePlanId, harfrust::ShapePlan>,
    unicode_buffer: Option<harfrust::UnicodeBuffer>,
    features: Vec<harfrust::Feature>,
    char_cluster: CharCluster,
}

impl Default for ShapeContext {
    fn default() -> Self {
        const MAX_ENTRIES: usize = 16;
        Self {
            shape_data_cache: LruCache::new(MAX_ENTRIES),
            shape_instance_cache: LruCache::new(MAX_ENTRIES),
            shape_plan_cache: LruCache::new(MAX_ENTRIES),
            unicode_buffer: Some(harfrust::UnicodeBuffer::new()),
            features: Vec::new(),
            char_cluster: CharCluster::default(),
        }
    }
}

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
    fq: Query<'a>,
    styles: &'a [ResolvedStyle<B>],
    inline_boxes: &[InlineBox],
    infos: &[(CharInfo, u16)],
    levels: &[u8],
    scx: &mut ShapeContext,
    text: &str,
    paragraph: &mut ShapedParagraph<B>,
    analysis_data_sources: &AnalysisDataSources,
) {
    let mut sink = ShapedParagraphSink::new(paragraph);
    shape_text_to_sink(
        rcx,
        fq,
        styles,
        inline_boxes,
        infos,
        levels,
        scx,
        text,
        &mut sink,
        analysis_data_sources,
    );
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn shape_text_to_sink<'a, B: Brush, S: ShapeSink>(
    rcx: &'a ResolveContext,
    mut fq: Query<'a>,
    styles: &'a [ResolvedStyle<B>],
    inline_boxes: &[InlineBox],
    infos: &[(CharInfo, u16)],
    levels: &[u8],
    scx: &mut ShapeContext,
    mut text: &str,
    sink: &mut S,
    analysis_data_sources: &AnalysisDataSources,
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
            sink.push_inline_box(box_idx);
        }
        return;
    }

    // Setup mutable state for iteration
    let initial_style_index = infos.first().map_or(0, |(_, style_index)| *style_index);
    let mut style = &styles[initial_style_index as usize];
    let mut item = Item {
        style_index: initial_style_index,
        size: style.font_size,
        level: levels.first().copied().unwrap_or(0),
        script: infos
            .iter()
            .map(|x| x.0.script)
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

    // Iterate over characters in the text
    for ((char_index, (byte_index, ch)), (info, style_index)) in
        text.char_indices().enumerate().zip(infos)
    {
        let mut break_run = false;
        let mut script = info.script;
        if !real_script(script) {
            script = item.script;
        }
        let level = levels.get(char_index).copied().unwrap_or(0);
        if item.style_index != *style_index {
            item.style_index = *style_index;
            style = &styles[*style_index as usize];
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
        let mut deferred_boxes: Option<RangeInclusive<usize>> = None;
        while let Some((box_idx, inline_box)) = current_box {
            if inline_box.index == byte_index {
                break_run = true;
                if let Some(boxes) = &mut deferred_boxes {
                    deferred_boxes = Some((*boxes.start())..=box_idx);
                } else {
                    deferred_boxes = Some(box_idx..=box_idx);
                };
                // Update the current box to the next box
                current_box = inline_box_iter.next();
            } else {
                break;
            }
        }

        if break_run && !text_range.is_empty() {
            shape_item(
                &mut fq,
                rcx,
                styles,
                &item,
                scx,
                text,
                &text_range,
                &char_range,
                infos,
                sink,
                analysis_data_sources,
            );
            item.size = style.font_size;
            item.level = level;
            item.script = script;
            item.locale = style.locale;
            item.variations = style.font_variations;
            item.features = style.font_features;
            item.word_spacing = style.word_spacing;
            item.letter_spacing = style.letter_spacing;
            text_range.start = text_range.end;
            char_range.start = char_range.end;
        }

        if let Some(deferred_boxes) = deferred_boxes {
            for box_idx in deferred_boxes {
                sink.push_inline_box(box_idx);
            }
        }

        text_range.end += ch.len_utf8();
        char_range.end += 1;
    }

    if !text_range.is_empty() {
        shape_item(
            &mut fq,
            rcx,
            styles,
            &item,
            scx,
            text,
            &text_range,
            &char_range,
            infos,
            sink,
            analysis_data_sources,
        );
    }

    // Process any remaining inline boxes whose index is greater than the length of the text
    if let Some((box_idx, _inline_box)) = current_box {
        sink.push_inline_box(box_idx);
    }
    for (box_idx, _inline_box) in inline_box_iter {
        sink.push_inline_box(box_idx);
    }
}

// Rebuilds the provided `char_cluster` in-place using the existing allocation
// for the given grapheme `segment_text`, consuming items from `item_infos_iter`.
fn fill_cluster_in_place(
    segment_text: &str,
    item_infos_iter: &mut core::slice::Iter<'_, (CharInfo, u16)>,
    code_unit_offset_in_string: &mut usize,
    char_cluster: &mut CharCluster,
) {
    // Reset cluster but keep allocation
    char_cluster.clear();

    let mut force_normalize = false;
    let mut is_emoji_or_pictograph = false;
    let mut map_len: u8 = 0;
    let start = *code_unit_offset_in_string as u32;

    for ((_, ch), (info, style_index)) in segment_text.char_indices().zip(item_infos_iter.by_ref())
    {
        force_normalize |= info.force_normalize();
        // TODO - make emoji detection more complete, as per (except using composite Trie tables as
        //  much as possible:
        //  https://github.com/conor-93/parley/blob/4637d826732a1a82bbb3c904c7f47a16a21cceec/parley/src/shape/mod.rs#L221-L269
        is_emoji_or_pictograph |= info.is_emoji_or_pictograph();
        *code_unit_offset_in_string += ch.len_utf8();

        // TODO: Explore ignoring other modifiers in determining `contributes_to_shaping`:
        //  regional indicators, subdivision flag tag sequences, skin tone modifiers
        //  See also: https://github.com/google/emoji-segmenter

        // If the color emoji has a non-printing variation selector, ignore the variation selector.
        // Its presentation depends on the platform and font.
        //
        // e.g.
        //  - `U+270C + U+FE0F`: `✌`, force basic presentation
        //  - `U+270C + U+FE0F`: `✌️`, force emoji presentation
        //
        // <https://www.unicode.org/reports/tr37/>
        let is_emoji_with_non_printing_variation_selector =
            is_emoji_or_pictograph && info.is_variation_selector();

        let contributes_to_shaping =
            info.contributes_to_shaping() && !is_emoji_with_non_printing_variation_selector;
        if contributes_to_shaping {
            map_len += 1;
        }

        char_cluster.chars.push(Char {
            ch,
            contributes_to_shaping,
            glyph_id: 0,
            style_index: *style_index,
            is_control_character: info.is_control(),
        });
    }

    // Finalize cluster metadata
    let end = *code_unit_offset_in_string as u32;
    char_cluster.is_emoji = is_emoji_or_pictograph;
    char_cluster.map_len = map_len;
    char_cluster.start = start;
    char_cluster.end = end;
    char_cluster.force_normalize = force_normalize;
}

fn shape_item<'a, B: Brush, S: ShapeSink>(
    fq: &mut Query<'a>,
    rcx: &'a ResolveContext,
    styles: &'a [ResolvedStyle<B>],
    item: &Item,
    scx: &mut ShapeContext,
    text: &str,
    text_range: &core::ops::Range<usize>,
    char_range: &core::ops::Range<usize>,
    infos: &[(CharInfo, u16)],
    sink: &mut S,
    analysis_data_sources: &AnalysisDataSources,
) {
    let item_text = &text[text_range.clone()];
    let item_infos = &infos[char_range.start..char_range.end]; // Only process current item
    let first_style_index = item_infos[0].1;
    let fb_script = convert::script_to_fontique(item.script, analysis_data_sources);
    let mut font_selector =
        FontSelector::new(fq, rcx, styles, first_style_index, fb_script, item.locale);

    let grapheme_cluster_boundaries = analysis_data_sources
        .grapheme_segmenter()
        .segment_str(item_text);
    let mut item_infos_iter = item_infos.iter();
    let mut code_unit_offset_in_string = text_range.start;
    let char_cluster = &mut scx.char_cluster;

    // Build an iterator of boundaries and consume the first segment to seed the loop
    let mut boundaries_iter = grapheme_cluster_boundaries.skip(1);
    let mut last_boundary = 0_usize;
    let Some(mut current_boundary) = boundaries_iter.next() else {
        return; // No clusters
    };

    fill_cluster_in_place(
        &item_text[last_boundary..current_boundary],
        &mut item_infos_iter,
        &mut code_unit_offset_in_string,
        char_cluster,
    );

    let mut current_font = font_selector.select_font(char_cluster, analysis_data_sources);

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
            fill_cluster_in_place(
                &item_text[last_boundary..current_boundary],
                &mut item_infos_iter,
                &mut code_unit_offset_in_string,
                char_cluster,
            );

            if let Some(next_font) = font_selector.select_font(char_cluster, analysis_data_sources)
            {
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
            harfrust::FontRef::from_index(font.font.blob.as_ref(), font.font.index).unwrap();

        // Create harfrust shaper
        let shaper_data = scx.shape_data_cache.entry(
            cache::ShapeDataKey::new(font.font.blob.id(), font.font.index),
            || harfrust::ShaperData::new(&font_ref),
        );
        let instance = scx.shape_instance_cache.entry(
            cache::ShapeInstanceKey::new(
                font.font.blob.id(),
                font.font.index,
                &font.font.synthesis,
                rcx.variations(item.variations),
            ),
            || {
                harfrust::ShaperInstance::from_variations(
                    &font_ref,
                    variations_iter(&font.font.synthesis, rcx.variations(item.variations)),
                )
            },
        );

        let direction = if item.level & 1 != 0 {
            harfrust::Direction::RightToLeft
        } else {
            harfrust::Direction::LeftToRight
        };
        let hb_script = script_to_harfrust(fb_script);
        let language = item
            .locale
            .as_ref()
            .and_then(|lang| lang.language().parse::<harfrust::Language>().ok());
        scx.features.clear();
        for feature in rcx.features(item.features).unwrap_or(&[]) {
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
                font.font.blob.id(),
                font.font.index,
                &font.font.synthesis,
                direction,
                hb_script,
                language.clone(),
                &scx.features,
                rcx.variations(item.variations),
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

        let glyph_buffer = harf_shaper.shape(
            buffer,
            ShapeOptions::new()
                .plan(Some(shaper_plan))
                .features(&scx.features)
                .point_size(Some(item.size)),
        );

        // Extract relevant CharInfo slice for this segment
        let char_start = char_range.start + item_text[..segment_start_offset].chars().count();
        let segment_char_start = char_start - char_range.start;
        let segment_char_count = segment_text.chars().count();
        let segment_infos =
            &item_infos[segment_char_start..(segment_char_start + segment_char_count)];

        // Stream the shaped segment as one run rather than materializing a
        // `GlyphBuffer`-specific `push_run` payload in the sink API.
        let (metrics, scale_factor) = compute_run_metrics(
            &font.font,
            item.size,
            harf_shaper.coords(),
            &styles[item.style_index as usize],
        );
        let run = ShapeRun {
            font: FontData::new(font.font.blob.clone(), font.font.index),
            font_size: item.size,
            font_attrs: font.attrs,
            synthesis: font.font.synthesis,
            coords: harf_shaper.coords(),
            text_range: (text_range.start + segment_start_offset)
                ..(text_range.start + segment_end_offset),
            bidi_level: item.level,
            metrics,
            word_spacing: item.word_spacing,
            letter_spacing: item.letter_spacing,
        };
        sink.begin_run(run);
        // Push harfrust-shaped run for the entire segment.
        let run_advance = emit_clusters_to_sink(
            sink,
            item.level,
            scale_factor,
            &glyph_buffer,
            segment_infos,
            segment_text,
        );
        sink.end_run(run_advance);

        // Replace buffer to reuse allocation in next iteration.
        scx.unicode_buffer = Some(glyph_buffer.clear());
    }
}

fn compute_run_metrics<B: Brush>(
    font: &QueryFont,
    font_size: f32,
    coords: &[harfrust::NormalizedCoord],
    style: &ResolvedStyle<B>,
) -> (RunMetrics, f32) {
    // Keep run-metric computation aligned with the previous `LayoutData`
    // shaping path so the compatibility sink remains behavior-preserving while
    // the pipeline seam is extracted.
    let font_ref = skrifa::FontRef::from_index(font.blob.as_ref(), font.index).unwrap();
    let metrics =
        skrifa::metrics::Metrics::new(&font_ref, skrifa::prelude::Size::new(font_size), coords);
    let units_per_em = metrics.units_per_em as f32;

    let (underline_offset, underline_size) = if let Some(underline) = metrics.underline {
        (underline.offset, underline.thickness)
    } else {
        // Default values from Harfbuzz: https://github.com/harfbuzz/harfbuzz/blob/00492ec7df0038f41f78d43d477c183e4e4c506e/src/hb-ot-metrics.cc#L334
        let default = units_per_em / 18.0;
        (default, default)
    };
    let (strikethrough_offset, strikethrough_size) = if let Some(strikeout) = metrics.strikeout {
        (strikeout.offset, strikeout.thickness)
    } else {
        // Default values from HarfBuzz: https://github.com/harfbuzz/harfbuzz/blob/00492ec7df0038f41f78d43d477c183e4e4c506e/src/hb-ot-metrics.cc#L334-L347
        (metrics.ascent / 2.0, units_per_em / 18.0)
    };

    // Compute line height.
    let line_height = match style.line_height {
        crate::LineHeight::Absolute(value) => value,
        crate::LineHeight::FontSizeRelative(value) => value * font_size,
        crate::LineHeight::MetricsRelative(value) => {
            (metrics.ascent - metrics.descent + metrics.leading) * value
        }
    };

    (
        RunMetrics {
            ascent: metrics.ascent,
            descent: -metrics.descent,
            leading: metrics.leading,
            underline_offset,
            underline_size,
            strikethrough_offset,
            strikethrough_size,
            line_height,
            x_height: metrics.x_height,
            cap_height: metrics.cap_height,
        },
        font_size / units_per_em,
    )
}

/// Converts `HarfRust` output into streamed glyph and cluster events.
///
/// The sink receives glyphs for the current run in run-local order, followed by
/// cluster records that describe how those glyphs attach back to source text.
fn emit_clusters_to_sink<S: ShapeSink>(
    sink: &mut S,
    bidi_level: u8,
    scale_factor: f32,
    glyph_buffer: &harfrust::GlyphBuffer,
    char_infos: &[(CharInfo, u16)],
    source_text: &str,
) -> f32 {
    let glyph_infos = glyph_buffer.glyph_infos();
    if glyph_infos.is_empty() {
        return 0.0;
    }

    let glyph_positions = glyph_buffer.glyph_positions();
    let direction = if bidi_level & 1 == 1 {
        Direction::Rtl
    } else {
        Direction::Ltr
    };

    // `HarfRust` returns glyphs in visual order, so we need to process them as
    // such while maintaining logical ordering of clusters.
    match direction {
        Direction::Ltr => emit_glyphs_and_clusters(
            sink,
            direction,
            scale_factor,
            glyph_infos,
            glyph_positions,
            char_infos,
            source_text.char_indices(),
        ),
        Direction::Rtl => emit_glyphs_and_clusters(
            sink,
            direction,
            scale_factor,
            glyph_infos,
            glyph_positions,
            char_infos,
            source_text.char_indices().rev(),
        ),
    }
}

/// Processes shaped glyphs from `HarfRust` and converts them into streamed
/// [`ShapeCluster`] and [`Glyph`] events.
///
/// # Parameters
///
/// ## Output Parameters (mutated by this function):
/// * `sink` - Sink where new [`ShapeCluster`] and [`Glyph`] events will be
///   pushed. Note: single-glyph clusters with zero offsets may be inlined
///   directly into [`ShapeCluster`].
///
/// ## Input Parameters:
/// * `direction` - Direction of the text.
/// * `scale_factor` - Scaling factor used to convert font units to the target
///   size.
/// * `glyph_infos` - `HarfRust` glyph information in visual order.
/// * `glyph_positions` - `HarfRust` glyph positioning data in visual order.
/// * `char_infos` - Character information from text analysis, indexed by
///   cluster ID.
/// * `char_indices_iter` - Iterator over (`byte_offset`, `char`) pairs from the
///   source text. Should be in logical order (forward for LTR, reverse for RTL).
fn emit_glyphs_and_clusters<S: ShapeSink, I: Iterator<Item = (usize, char)>>(
    sink: &mut S,
    direction: Direction,
    scale_factor: f32,
    glyph_infos: &[harfrust::GlyphInfo],
    glyph_positions: &[harfrust::GlyphPosition],
    char_infos: &[(CharInfo, u16)],
    char_indices_iter: I,
) -> f32 {
    let mut char_indices_iter = char_indices_iter.peekable();
    let mut cluster_start_char = char_indices_iter.next().unwrap();
    let mut total_glyphs: u32 = 0;
    let mut cluster_glyph_offset: u32 = 0;
    let start_cluster_id = glyph_infos.first().unwrap().cluster;
    let mut cluster_id = start_cluster_id;
    let mut char_info = char_infos[cluster_id as usize];
    let mut run_advance = 0.0;
    let mut cluster_advance = 0.0;
    // If the current cluster might be a single-glyph, zero-offset cluster, we
    // defer pushing the first glyph because it may be stored inline in the
    // eventual cluster record instead of the glyph stream.
    let mut pending_inline_glyph: Option<Glyph> = None;

    // The mental model for understanding this function is best grasped by
    // first reading the HarfBuzz docs on clusters:
    // https://harfbuzz.github.io/working-with-harfbuzz-clusters.html
    //
    // `num_components` is the number of characters in the current cluster.
    // Since source text's characters were inserted into HarfRust's buffer
    // using their logical indices as the cluster ID, HarfRust assigns the
    // first character's cluster ID (in logical order) to the merged cluster
    // because the minimum ID is selected for merging.
    //
    // The number of components depends on direction:
    // - In LTR, it is the difference between the next cluster and the current cluster.
    // - In RTL, it is the difference between the last cluster and the current cluster.
    //
    // This is because we compare the current cluster to its next larger logical
    // ID, which is visually downstream in LTR and visually upstream in RTL.
    //
    // Example: LTR text "afi" where "fi" form a ligature.
    //   Initial cluster values: 0, 1, 2 (logical + visual order)
    //   HarfRust assignation:   0, 1, 1
    //   Cluster count:          2
    //   `num_components`:       (1 - 0 =) 1, (3 - 1 =) 2
    //
    // Example: RTL text "حداً".
    //   Initial cluster values:  0, 1, 2, 3 (logical order)
    //   Reversed values:         3, 2, 1, 0 (visual order)
    //   HarfRust assignation:    3, 2, 0, 0
    //   Cluster count:           3
    //   `num_components`:        (4 - 3 =) 1, (3 - 2 =) 1, (2 - 0 =) 2
    let num_components =
        |next_cluster: u32, current_cluster: u32, last_cluster: u32| match direction {
            Direction::Ltr => next_cluster - current_cluster,
            Direction::Rtl => last_cluster - current_cluster,
        };
    let mut last_cluster_id: u32 = match direction {
        Direction::Ltr => 0,
        Direction::Rtl => char_infos.len() as u32,
    };

    for (glyph_info, glyph_pos) in glyph_infos.iter().zip(glyph_positions.iter()) {
        // Flush the previous cluster once we see the first glyph of a new
        // cluster.
        if cluster_id != glyph_info.cluster {
            run_advance += cluster_advance;
            let num_components = num_components(glyph_info.cluster, cluster_id, last_cluster_id);
            cluster_advance /= num_components as f32;
            let is_newline = whitespace_of(cluster_start_char.1) == Whitespace::Newline;
            let cluster_type = if num_components > 1 {
                debug_assert!(!is_newline);
                ClusterType::LigatureStart
            } else if is_newline {
                ClusterType::Newline
            } else {
                ClusterType::Regular
            };

            let inline_glyph_id = if matches!(cluster_type, ClusterType::Regular) {
                pending_inline_glyph.take().map(|g| g.id)
            } else {
                // This is not a regular cluster, so any pending glyph must stay
                // in the explicit glyph stream rather than being stored inline.
                if let Some(pending) = pending_inline_glyph.take() {
                    sink.push_glyph(pending);
                    total_glyphs += 1;
                }
                None
            };

            emit_cluster(
                sink,
                char_info,
                cluster_start_char,
                cluster_advance,
                cluster_type,
                total_glyphs - cluster_glyph_offset,
                inline_glyph_id,
            );
            cluster_glyph_offset = total_glyphs;

            if num_components > 1 {
                // Skip characters until we reach the current cluster.
                // Create ligature component clusters for the remaining characters.
                // Emit ligature component clusters for the remaining source
                // characters that participated in the ligature.
                for i in 1..num_components {
                    cluster_start_char = char_indices_iter.next().unwrap();
                    if whitespace_of(cluster_start_char.1) == Whitespace::Space {
                        break;
                    }
                    let char_info_ = match direction {
                        Direction::Ltr => char_infos[(cluster_id + i) as usize],
                        Direction::Rtl => char_infos[(cluster_id + num_components - i) as usize],
                    };
                    emit_cluster(
                        sink,
                        char_info_,
                        cluster_start_char,
                        cluster_advance,
                        ClusterType::LigatureComponent,
                        0,
                        None,
                    );
                }
            }
            cluster_start_char = char_indices_iter.next().unwrap();

            cluster_advance = 0.0;
            last_cluster_id = cluster_id;
            cluster_id = glyph_info.cluster;
            char_info = char_infos[cluster_id as usize];
            pending_inline_glyph = None;
        }

        let glyph = Glyph {
            id: glyph_info.glyph_id,
            style_index: char_info.1,
            x: (glyph_pos.x_offset as f32) * scale_factor,
            // Convert from font space (Y-up) to layout space (Y-down).
            y: -(glyph_pos.y_offset as f32) * scale_factor,
            advance: (glyph_pos.x_advance as f32) * scale_factor,
        };
        cluster_advance += glyph.advance;
        // Push any pending glyph. If it really was a zero-offset, single-glyph
        // cluster it would have been consumed as an inline glyph above.
        if let Some(pending) = pending_inline_glyph.take() {
            sink.push_glyph(pending);
            total_glyphs += 1;
        }
        if total_glyphs == cluster_glyph_offset && glyph.x == 0.0 && glyph.y == 0.0 {
            // Defer this potential zero-offset, single-glyph cluster so it can
            // be stored inline in the cluster record instead of the glyph list.
            pending_inline_glyph = Some(glyph);
        } else {
            sink.push_glyph(glyph);
            total_glyphs += 1;
        }
    }

    // Push the last cluster.
    // Emit the final cluster after the glyph loop terminates.
    // See comment above `num_components` for why we use `char_infos.len()` for LTR and 0 for RTL.
    let next_cluster_id = match direction {
        Direction::Ltr => char_infos.len() as u32,
        Direction::Rtl => 0,
    };
    let num_components = num_components(next_cluster_id, cluster_id, last_cluster_id);
    if num_components > 1 {
        // This is a ligature - create ligature start + ligature components.
        // This final cluster is a ligature: emit the ligature start cluster
        // plus component clusters for the remaining characters.
        if let Some(pending) = pending_inline_glyph.take() {
            sink.push_glyph(pending);
            total_glyphs += 1;
        }
        let ligature_advance = cluster_advance / num_components as f32;
        emit_cluster(
            sink,
            char_info,
            cluster_start_char,
            ligature_advance,
            ClusterType::LigatureStart,
            total_glyphs - cluster_glyph_offset,
            None,
        );

        for i in 1..num_components {
            let char = char_indices_iter.next().unwrap();
            if whitespace_of(char.1) == Whitespace::Space {
                break;
            }
            let component_char_info = match direction {
                Direction::Ltr => char_infos[(cluster_id + i) as usize],
                Direction::Rtl => char_infos[(cluster_id + num_components - i) as usize],
            };
            emit_cluster(
                sink,
                component_char_info,
                char,
                ligature_advance,
                ClusterType::LigatureComponent,
                0,
                None,
            );
        }
    } else {
        let is_newline = whitespace_of(cluster_start_char.1) == Whitespace::Newline;
        let cluster_type = if is_newline {
            ClusterType::Newline
        } else {
            ClusterType::Regular
        };
        let mut inline_glyph_id = None;
        match cluster_type {
            ClusterType::Regular => {
                if total_glyphs == cluster_glyph_offset {
                    if let Some(pending) = pending_inline_glyph.take() {
                        inline_glyph_id = Some(pending.id);
                    }
                }
            }
            _ => {
                if let Some(pending) = pending_inline_glyph.take() {
                    sink.push_glyph(pending);
                    total_glyphs += 1;
                }
            }
        }
        emit_cluster(
            sink,
            char_info,
            cluster_start_char,
            cluster_advance,
            cluster_type,
            total_glyphs - cluster_glyph_offset,
            inline_glyph_id,
        );
    }

    run_advance
}

#[derive(Copy, Clone, PartialEq)]
enum Direction {
    Ltr,
    Rtl,
}

#[derive(Copy, Clone)]
enum ClusterType {
    LigatureStart,
    LigatureComponent,
    Regular,
    Newline,
}

impl From<ClusterType> for u16 {
    fn from(cluster_type: ClusterType) -> Self {
        match cluster_type {
            ClusterType::LigatureStart => ClusterData::LIGATURE_START,
            ClusterType::LigatureComponent => ClusterData::LIGATURE_COMPONENT,
            ClusterType::Regular | ClusterType::Newline => 0,
        }
    }
}

fn emit_cluster<S: ShapeSink>(
    sink: &mut S,
    char_info: (CharInfo, u16),
    cluster_start_char: (usize, char),
    advance: f32,
    cluster_type: ClusterType,
    glyph_len: u32,
    inline_glyph_id: Option<u32>,
) {
    let glyphs = match cluster_type {
        ClusterType::LigatureComponent => {
            // Ligature components have no glyphs, only advance.
            debug_assert_eq!(glyph_len, 0);
            ShapeClusterGlyphs::None
        }
        ClusterType::Newline => {
            // Newline clusters are stripped of their glyph contribution.
            debug_assert_eq!(glyph_len, 1);
            ShapeClusterGlyphs::None
        }
        _ if inline_glyph_id.is_some() => {
            // Inline glyphs are stored inline within `ShapeCluster`.
            // Single zero-offset glyphs can be stored inline in the cluster.
            debug_assert_eq!(glyph_len, 0);
            ShapeClusterGlyphs::Inline(inline_glyph_id.unwrap())
        }
        ClusterType::Regular | ClusterType::LigatureStart => {
            // Regular and ligature start clusters maintain their glyphs and advance.
            // Regular clusters and ligature starts retain their glyph range.
            debug_assert_ne!(glyph_len, 0);
            ShapeClusterGlyphs::Range {
                len: glyph_len as u8,
            }
        }
    };

    sink.push_cluster(ShapeCluster {
        boundary: char_info.0.boundary,
        source_char: cluster_start_char.1,
        flags: cluster_type.into(),
        style_index: char_info.1,
        text_len: cluster_start_char.1.len_utf8() as u8,
        text_offset: cluster_start_char.0 as u16,
        advance: match cluster_type {
            ClusterType::Newline => 0.0,
            _ => advance,
        },
        glyphs,
    });
}

const fn whitespace_of(c: char) -> Whitespace {
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

fn real_script(script: Script) -> bool {
    script != Script::Common && script != Script::Unknown && script != Script::Inherited
}

fn variations_iter<'a>(
    synthesis: &'a fontique::Synthesis,
    item: Option<&'a [FontVariation]>,
) -> impl Iterator<Item = harfrust::Variation> + 'a {
    synthesis
        .variation_settings()
        .iter()
        .map(|(tag, value)| harfrust::Variation {
            tag: *tag,
            value: *value,
        })
        .chain(
            item.unwrap_or(&[])
                .iter()
                .map(|variation| harfrust::Variation {
                    tag: harfrust::Tag::new(&variation.tag.to_bytes()),
                    value: variation.value,
                }),
        )
}

struct FontSelector<'a, 'b, B: Brush> {
    query: &'b mut Query<'a>,
    fonts_id: Option<usize>,
    rcx: &'a ResolveContext,
    styles: &'a [ResolvedStyle<B>],
    style_index: u16,
    attrs: fontique::Attributes,
    variations: &'a [FontVariation],
    features: &'a [FontFeature],
}

impl<'a, 'b, B: Brush> FontSelector<'a, 'b, B> {
    fn new(
        query: &'b mut Query<'a>,
        rcx: &'a ResolveContext,
        styles: &'a [ResolvedStyle<B>],
        style_index: u16,
        fb_script: fontique::Script,
        locale: Option<Language>,
    ) -> Self {
        let style = &styles[style_index as usize];
        let fonts_id = style.font_family.id();
        let fonts = rcx.stack(style.font_family).unwrap_or(&[]);
        let attrs = fontique::Attributes {
            width: style.font_width,
            weight: style.font_weight,
            style: style.font_style,
        };
        let variations = rcx.variations(style.font_variations).unwrap_or(&[]);
        let features = rcx.features(style.font_features).unwrap_or(&[]);
        query.set_families(fonts.iter().copied());

        query.set_fallbacks(fontique::FallbackKey::new(fb_script, locale.as_ref()));
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

    fn select_font(
        &mut self,
        cluster: &mut CharCluster,
        analysis_data_sources: &AnalysisDataSources,
    ) -> Option<SelectedFont> {
        let style_index = cluster.style_index();
        let is_emoji = cluster.is_emoji;
        if style_index != self.style_index || is_emoji || self.fonts_id.is_none() {
            self.style_index = style_index;
            let style = &self.styles[style_index as usize];

            let fonts_id = style.font_family.id();
            let fonts = self.rcx.stack(style.font_family).unwrap_or(&[]);
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
            let Some(charmap) = font.charmap() else {
                return fontique::QueryStatus::Continue;
            };

            let map_status = cluster.map(
                |ch| {
                    charmap
                        .map(ch)
                        .map(|g| {
                            // HACK: in reality, we're only computing coverage, so
                            // we only care about whether the font  has a mapping
                            // for a particular glyph. Any non-zero value indicates
                            // the existence of a glyph so we can simplify this
                            // without a fallible conversion from u32 to u16.
                            (g != 0) as u16
                        })
                        .unwrap_or_default()
                },
                analysis_data_sources,
            );

            match map_status {
                Status::Complete => {
                    selected_font = Some(SelectedFont {
                        font: font.clone(),
                        attrs: self.attrs,
                    });
                    fontique::QueryStatus::Stop
                }
                Status::Keep => {
                    selected_font = Some(SelectedFont {
                        font: font.clone(),
                        attrs: self.attrs,
                    });
                    fontique::QueryStatus::Continue
                }
                Status::Discard => {
                    if selected_font.is_none() {
                        selected_font = Some(SelectedFont {
                            font: font.clone(),
                            attrs: self.attrs,
                        });
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
    attrs: fontique::Attributes,
}

impl PartialEq for SelectedFont {
    fn eq(&self, other: &Self) -> bool {
        self.font.family == other.font.family && self.font.synthesis == other.font.synthesis
    }
}
