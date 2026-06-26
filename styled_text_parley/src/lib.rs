// Copyright 2026 the Parley Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Styled Text Parley adapts [`styled_text`] to Parley's low-level style-run
//! builder.
//! It lowers resolved styled-text segments into Parley's style table and range
//! runs, while reusing scratch storage across layout builds.
//!
//! The crate also provides a Parley-shaped first style vocabulary.
//! [`ParleyLayoutStyle`] holds the fields that can affect shaping and line
//! layout.
//! [`ParleyPaintStyle`] holds paint-only fields such as brushes and decorations.
//! Interning those payloads separately means paint-only changes can share
//! layout identity when the styled text is lowered.
//!
//! This adapter does not own document structure, inline boxes, cascading, or
//! renderer-specific style semantics.
//! Callers can use the provided Parley style payloads for a simple path, or keep
//! their own style types in `styled_text` and use the generic lowering
//! functions.
//!
//! ## Concepts
//!
//! - [`ParleyStyledTextBuilder`] is a [`StyledTextBuilder`] configured with the
//!   default Parley style payloads and patch type.
//! - [`ParleyStyleChange`] is a partial style patch: an ordered list of Parley
//!   [`parley::StyleProperty`] values applied over the current full style.
//! - [`ParleyStyleRunWorkspace`] keeps the reusable segment workspace and the
//!   temporary [`styled_text::StyleId`] to Parley style-index map.
//! - [`build_layout_from_parley_styled_text`] creates a Parley [`parley::Layout`]
//!   from text built with the default Parley payloads.
//! - [`push_style_runs`] is the lower-level hook for callers that want to feed
//!   Parley style runs themselves.
//!
//! ## Building a Parley layout
//!
//! ```no_run
//! use parley::{FontContext, FontWeight, LayoutContext, StyleProperty};
//! use styled_text_parley::{
//!     ParleyLayoutStyle, ParleyPaintStyle, ParleyStyleChange, ParleyStyleRunWorkspace,
//!     ParleyStyledTextBuilder, build_layout_from_parley_styled_text,
//! };
//!
//! let mut text = ParleyStyledTextBuilder::<()>::new(
//!     ParleyLayoutStyle::default(),
//!     ParleyPaintStyle::default(),
//! );
//! text.push("Hello ");
//! text.push_with(
//!     "styled text",
//!     ParleyStyleChange::new()
//!         .with(StyleProperty::FontSize(24.0))
//!         .with(StyleProperty::FontWeight(FontWeight::BOLD)),
//! );
//! let styled = text.finish();
//!
//! let mut font_cx = FontContext::new();
//! let mut layout_cx = LayoutContext::<()>::new();
//! let mut workspace = ParleyStyleRunWorkspace::new();
//! let mut layout = build_layout_from_parley_styled_text(
//!     &mut layout_cx,
//!     &mut font_cx,
//!     &styled,
//!     &mut workspace,
//!     1.0,
//!     true,
//! ).unwrap();
//! layout.break_all_lines(Some(240.0));
//! ```
//!
//! ## Features
//!
//! - `std` (enabled by default): Enables `std` support in [`parley`] and
//!   [`styled_text`].
//! - `libm`: Enables the `libm` feature of [`parley`].

// LINEBENDER LINT SET - lib.rs - v3
// See https://linebender.org/wiki/canonical-lints/
// These lints shouldn't apply to examples or tests.
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
// These lints shouldn't apply to examples.
#![warn(clippy::print_stdout, clippy::print_stderr)]
// Targeting e.g. 32-bit means structs containing usize can give false positives for 64-bit.
#![cfg_attr(target_pointer_width = "64", warn(clippy::trivially_copy_pass_by_ref))]
// END LINEBENDER LINT SET
#![cfg_attr(docsrs, feature(doc_cfg))]
#![no_std]

extern crate alloc;

mod lowering;
mod style;

use styled_text::{StyledText, StyledTextBuilder};

pub use lowering::{
    Error, ParleyStyleRunWorkspace, build_layout_from_parley_styled_text,
    build_layout_from_styled_text, push_parley_style, push_style_runs,
};
pub use style::{ParleyLayoutStyle, ParleyPaintStyle, ParleyStyleChange};

/// Styled text that uses the default Parley style payloads.
pub type ParleyStyledText<T, B> = StyledText<T, ParleyLayoutStyle, ParleyPaintStyle<B>>;

/// Builder for styled text that uses the default Parley style payloads.
pub type ParleyStyledTextBuilder<B> =
    StyledTextBuilder<ParleyLayoutStyle, ParleyPaintStyle<B>, ParleyStyleChange<B>>;
