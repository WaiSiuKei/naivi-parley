// Copyright 2026 the Parley Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! macOS CoreText font identity for the naivi CoreText backend.
//!
//! [`MacNativeFont`] is the self-describing font identity carried by shaped
//! runs on macOS (KTD4). It crosses the fork → blitz boundary: blitz's
//! rasterizer resolves it back to a `CTFont` without access to any
//! fork-internal registry, so it must be fully self-describing
//! (PostScript name + family + size + color capability).

use alloc::string::String;

use core_text::font::CTFont;

/// `kCTFontColorGlyphsTrait` (CTFontTraits.h): the font provides color glyphs.
pub(crate) const CT_FONT_COLOR_GLYPHS_TRAIT: u32 = 1 << 13;

/// Self-describing identity of a CoreText font used by a shaped run.
#[derive(Clone, Debug, PartialEq)]
pub struct MacNativeFont {
    /// PostScript name of the run's font (e.g. `"AppleColorEmoji"`,
    /// `"PingFangSC-Regular"`). Resolvable on the consumer side via
    /// `CTFontDescriptor::new_from_postscript_name`.
    pub postscript_name: String,
    /// Family name of the run's font.
    pub family_name: String,
    /// Font size in points.
    pub size: f32,
    /// Whether the font is color-capable (e.g. Apple Color Emoji).
    pub color: bool,
}

impl MacNativeFont {
    /// Build a self-describing key from a `CTFont`.
    pub fn from_ctfont(font: &CTFont) -> Self {
        Self {
            postscript_name: font.postscript_name(),
            family_name: font.family_name(),
            size: font.pt_size() as f32,
            color: font.symbolic_traits() & CT_FONT_COLOR_GLYPHS_TRAIT != 0,
        }
    }
}
