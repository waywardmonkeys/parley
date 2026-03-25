// Copyright 2025 the Parley Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Various helper functions to assert truths during testing.

use std::vec::Vec;

use crate::{Brush, data::LayoutData};

fn canonicalize_layout_data<B: Brush>(layout_data: &LayoutData<B>) -> LayoutData<B> {
    let mut normalized = layout_data.clone();
    let mut canonical_styles = Vec::with_capacity(normalized.paragraph.styles.len());
    let mut remap = Vec::with_capacity(normalized.paragraph.styles.len());

    for style in &normalized.paragraph.styles {
        if let Some(index) = canonical_styles
            .iter()
            .position(|existing| existing == style)
        {
            remap.push(index as u16);
        } else {
            let index = canonical_styles.len() as u16;
            canonical_styles.push(style.clone());
            remap.push(index);
        }
    }

    for cluster in &mut normalized.paragraph.clusters {
        cluster.style_index = remap[cluster.style_index as usize];
    }
    for glyph in &mut normalized.paragraph.glyphs {
        glyph.style_index = remap[glyph.style_index as usize];
    }
    normalized.paragraph.styles = canonical_styles;
    normalized
}

/// Assert that the two provided `LayoutData` are equal.
pub(crate) fn assert_eq_layout_data<B: Brush>(a: &LayoutData<B>, b: &LayoutData<B>, case: &str) {
    let a = canonicalize_layout_data(a);
    let b = canonicalize_layout_data(b);

    assert_eq!(
        a.paragraph.scale, b.paragraph.scale,
        "{case} scale mismatch"
    );
    assert_eq!(
        a.paragraph.quantize, b.paragraph.quantize,
        "{case} quantize mismatch"
    );
    assert_eq!(
        a.paragraph.base_level, b.paragraph.base_level,
        "{case} base_level mismatch"
    );
    assert_eq!(
        a.paragraph.text_len, b.paragraph.text_len,
        "{case} text_len mismatch"
    );
    assert_eq!(a.width, b.width, "{case} width mismatch");
    assert_eq!(a.full_width, b.full_width, "{case} full_width mismatch");
    assert_eq!(a.height, b.height, "{case} height mismatch");
    assert_eq!(
        a.paragraph.fonts, b.paragraph.fonts,
        "{case} fonts mismatch"
    );
    assert_eq!(
        a.paragraph.coords, b.paragraph.coords,
        "{case} coords mismatch"
    );

    // Input (/ output of style resolution)
    assert_eq!(
        a.paragraph.styles, b.paragraph.styles,
        "{case} styles mismatch"
    );
    assert_eq!(
        a.paragraph.inline_boxes, b.paragraph.inline_boxes,
        "{case} inline_boxes mismatch"
    );

    // Output of shaping
    assert_eq!(a.paragraph.runs, b.paragraph.runs, "{case} runs mismatch");
    assert_eq!(
        a.paragraph.items, b.paragraph.items,
        "{case} items mismatch"
    );
    assert_eq!(
        a.paragraph.clusters, b.paragraph.clusters,
        "{case} clusters mismatch"
    );
    assert_eq!(
        a.paragraph.glyphs, b.paragraph.glyphs,
        "{case} glyphs mismatch"
    );

    // Output of line breaking
    assert_eq!(a.lines, b.lines, "{case} lines mismatch");
    assert_eq!(a.line_items, b.line_items, "{case} line_items mismatch");

    // Output of alignment
    assert_eq!(
        a.is_aligned_justified, b.is_aligned_justified,
        "{case} is_aligned_justified mismatch"
    );
    assert_eq!(
        a.layout_max_advance, b.layout_max_advance,
        "{case} alignment_width mismatch"
    );

    // Also compare the whole struct in case any fields have been added that aren't
    // part of this test yet. If this triggers, add the missing assert to the above set.
    assert_eq!(a, b, "{case} LayoutData mismatch");
}
