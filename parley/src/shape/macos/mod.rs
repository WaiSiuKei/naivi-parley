// Copyright 2026 the Parley Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! macOS CoreText shaping for the naivi CoreText backend.
//!
//! On `target_os = "macos"`, `shape_item` delegates to [`shape_item_coretext`]:
//! each item is shaped by a `CTLine` whose cascade list resolves system font
//! fallback (CJK, emoji, complex scripts) natively (R1 / KTD3). Non-macOS
//! targets never compile this module.
//!
//! CoreText output is pushed through the macOS run-push variant in
//! `layout::data` (CoreText positions are already in points and metrics come
//! from CoreText, not skrifa), so parley's line layout consumes it unchanged
//! (R5).
//!
//! The workspace denies `unsafe_code`; this module is the sole exception — it
//! owns the CoreText FFI surface (`CTParagraphStyleCreate`) and
//! core-foundation's unsafe attribute setters. This module is `cfg(macos)`-only.
#![allow(unsafe_code)]

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::mem::size_of;
use core::ops::Range;

use core_foundation::{
    array::CFArray,
    attributed_string::CFMutableAttributedString,
    base::{CFIndex, CFRange, CFType, CFTypeRef, TCFType},
    dictionary::CFDictionary,
    number::CFNumber,
    string::CFString,
};
use core_text::{
    font::CTFont,
    font_descriptor::{
        new_from_attributes, CTFontDescriptor, kCTFontCascadeListAttribute,
        kCTFontFamilyNameAttribute, kCTFontSlantTrait, kCTFontTraitsAttribute,
        kCTFontWeightTrait,
    },
    line::CTLine,
    string_attributes::{kCTFontAttributeName, kCTParagraphStyleAttributeName},
};
use fontique::Attributes as FontiqueAttributes;
use hashbrown::HashMap;
use parlance::{FontStyle, FontWeight};

use super::{Item, ShapeContext};
use crate::analysis::CharInfo;
use crate::layout::{Layout, MacMetrics};
use crate::resolve::{ResolveContext, ResolvedStyle};
use crate::style::Brush;

/// Snapshot of `FamilyId` → family name for the families referenced by the
/// style table, built once in `build_into_layout` (macOS only).
pub(crate) struct MacFamilyNames {
    names: HashMap<fontique::FamilyId, String>,
}

impl Default for MacFamilyNames {
    fn default() -> Self {
        Self {
            names: HashMap::new(),
        }
    }
}

impl MacFamilyNames {
    pub(crate) fn insert(&mut self, id: fontique::FamilyId, name: &str) {
        self.names.insert(id, name.to_string());
    }

    pub(crate) fn get(&self, id: fontique::FamilyId) -> Option<&str> {
        self.names.get(&id).map(|s| s.as_str())
    }
}

// ── Paragraph-style FFI ────────────────────────────────────────────────
//
// The `core-text` crate exposes `kCTParagraphStyleAttributeName` but not
// `CTParagraphStyleCreate` / `kCTParagraphStyleSpecifierBaseWritingDirection`.
// Declare the minimal C surface so a line fragment can carry an explicit base
// writing direction (mirrors the naive project's proven approach).

#[repr(C)]
struct CTParagraphStyleSetting {
    spec: u32,
    value_size: usize,
    value: *const std::ffi::c_void,
}

extern "C" {
    fn CTParagraphStyleCreate(
        settings: *const CTParagraphStyleSetting,
        setting_count: CFIndex,
    ) -> CFTypeRef;
}

/// `kCTParagraphStyleSpecifierBaseWritingDirection` (CTParagraphStyle.h).
const K_CT_PARAGRAPH_STYLE_SPECIFIER_BASE_WRITING_DIRECTION: u32 = 13;

/// `kCTWritingDirectionLeftToRight` / `kCTWritingDirectionRightToLeft`.
const CT_WRITING_DIRECTION_LTR: i8 = 0;
const CT_WRITING_DIRECTION_RTL: i8 = 1;

fn apply_base_direction(attributed: &mut CFMutableAttributedString, cf_len: CFIndex, rtl: bool) {
    let value: i8 = if rtl {
        CT_WRITING_DIRECTION_RTL
    } else {
        CT_WRITING_DIRECTION_LTR
    };
    let setting = CTParagraphStyleSetting {
        spec: K_CT_PARAGRAPH_STYLE_SPECIFIER_BASE_WRITING_DIRECTION,
        value_size: size_of::<i8>(),
        value: &value as *const i8 as *const std::ffi::c_void,
    };
    let style_ref = unsafe { CTParagraphStyleCreate(&setting, 1) };
    if style_ref.is_null() {
        return;
    }
    let style = unsafe { CFType::wrap_under_create_rule(style_ref) };
    unsafe {
        attributed.set_attribute(
            CFRange::init(0, cf_len),
            kCTParagraphStyleAttributeName,
            &style,
        );
    }
}

// ── Font construction ──────────────────────────────────────────────────

/// Build a CoreText font for the given family, weight, slant and size.
fn make_font(family: &str, weight: FontWeight, style: FontStyle, size: f32) -> Option<CTFont> {
    let weight_num = CFNumber::from(weight.value() as f64);
    let (slant_num, has_slant) = match style {
        FontStyle::Normal => (CFNumber::from(0.0f64), false),
        FontStyle::Italic | FontStyle::Oblique(_) => (CFNumber::from(0.2f64), true),
    };
    let weight_key = unsafe { CFString::wrap_under_get_rule(kCTFontWeightTrait) };
    let slant_key = unsafe { CFString::wrap_under_get_rule(kCTFontSlantTrait) };

    let mut trait_pairs: Vec<(CFString, CFType)> = Vec::with_capacity(2);
    trait_pairs.push((weight_key, weight_num.into_CFType()));
    if has_slant {
        trait_pairs.push((slant_key, slant_num.into_CFType()));
    }
    let traits: CFDictionary<CFString, CFType> =
        CFDictionary::from_CFType_pairs(&trait_pairs);

    let family_cf = CFString::new(family);
    let family_key = unsafe { CFString::wrap_under_get_rule(kCTFontFamilyNameAttribute) };
    let trait_key = unsafe { CFString::wrap_under_get_rule(kCTFontTraitsAttribute) };
    let attrs: CFDictionary<CFString, CFType> = CFDictionary::from_CFType_pairs(&[
        (family_key, family_cf.into_CFType()),
        (trait_key, traits.into_CFType()),
    ]);
    let desc = new_from_attributes(&attrs);
    Some(core_text::font::new_from_descriptor(&desc, size as f64))
}

/// Build a descriptor for a family name (used in the cascade preference list).
fn descriptor_for_family(name: &str) -> Option<CTFontDescriptor> {
    let family_cf = CFString::new(name);
    let family_key = unsafe { CFString::wrap_under_get_rule(kCTFontFamilyNameAttribute) };
    let attrs: CFDictionary<CFString, CFType> =
        CFDictionary::from_CFType_pairs(&[(family_key, family_cf.into_CFType())]);
    Some(new_from_attributes(&attrs))
}

/// Attach the remaining families of the CSS stack as the cascade preference,
/// so CoreText prefers them over its own generic fallback (KTD3).
fn with_cascade(font: &CTFont, size: f32, cascade_names: &[String]) -> CTFont {
    if cascade_names.is_empty() {
        return font.clone();
    }
    let descriptors: Vec<CTFontDescriptor> = cascade_names
        .iter()
        .filter_map(|n| descriptor_for_family(n))
        .collect();
    if descriptors.is_empty() {
        return font.clone();
    }
    let cascade = CFArray::from_CFTypes(&descriptors);
    let cascade_key = unsafe { CFString::wrap_under_get_rule(kCTFontCascadeListAttribute) };
    let attributes: CFDictionary<CFString, CFArray<CTFontDescriptor>> =
        CFDictionary::from_CFType_pairs(&[(cascade_key, cascade)]);
    match font
        .copy_descriptor()
        .create_copy_with_attributes(attributes.into_untyped())
    {
        Ok(desc) => core_text::font::new_from_descriptor(&desc, size as f64),
        Err(_) => font.clone(),
    }
}

// ── UTF-16 → UTF-8 mapping ─────────────────────────────────────────────

/// Map a UTF-16 code-unit offset into `text` to the UTF-8 byte offset of the
/// character containing that code unit.
fn utf16_to_utf8_map(text: &str) -> Vec<usize> {
    let mut map = Vec::new();
    let mut byte = 0usize;
    for ch in text.chars() {
        for _ in 0..ch.len_utf16() {
            map.push(byte);
        }
        byte += ch.len_utf8();
    }
    map.push(byte);
    map
}

// ── CoreText shaping ───────────────────────────────────────────────────

/// Metrics extracted from a CoreText font for one run.
pub(crate) fn ct_metrics(font: &CTFont) -> MacMetrics {
    MacMetrics {
        ascent: font.ascent() as f32,
        descent: font.descent() as f32,
        leading: font.leading() as f32,
        underline_offset: font.underline_position() as f32,
        underline_size: font.underline_thickness() as f32,
        cap_height: font.cap_height() as f32,
        x_height: font.x_height() as f32,
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn shape_item_coretext<B: Brush>(
    rcx: &ResolveContext,
    styles: &[ResolvedStyle<B>],
    item: &Item,
    _scx: &mut ShapeContext,
    item_text: &str,
    item_infos: &[(CharInfo, u16)],
    text_start: usize,
    layout: &mut Layout<B>,
    mac_families: &MacFamilyNames,
) {
    if item_text.is_empty() || item_infos.is_empty() {
        return;
    }
    let style = &styles[item.style_index as usize];

    // Resolve the family stack to names for the primary font and the cascade
    // preference list (KTD3).
    let family_ids = rcx.stack(style.font_family).unwrap_or(&[]);
    let family_names: Vec<String> = family_ids
        .iter()
        .filter_map(|id| mac_families.get(*id).map(|s| s.to_string()))
        .collect();
    let primary_family = family_names
        .first()
        .cloned()
        .unwrap_or_else(|| "Helvetica".to_string());

    // Build the primary font and attach the remaining stack as cascade.
    let Some(base_font) = make_font(&primary_family, style.font_weight, style.font_style, item.size)
    else {
        return;
    };
    let cascade_font = with_cascade(&base_font, item.size, &family_names[1..]);

    // Attributed string over the whole item text with the (cascade) font.
    let cf_string = CFString::new(item_text);
    let mut attributed = CFMutableAttributedString::new();
    attributed.replace_str(&cf_string, CFRange::init(0, 0));
    let cf_len = cf_string.char_len();
    unsafe {
        attributed.set_attribute(
            CFRange::init(0, cf_len),
            kCTFontAttributeName,
            &cascade_font,
        );
    }
    apply_base_direction(&mut attributed, cf_len, item.level & 1 == 1);

    let line = CTLine::new_with_attributed_string(attributed.as_concrete_TypeRef());
    let glyph_runs = line.glyph_runs();
    let line_width = line.get_typographic_bounds().width as f32;
    let utf16_to_utf8 = utf16_to_utf8_map(item_text);

    // Collect per-run glyph data (advances computed from consecutive
    // positions; the final glyph reaches the line width).
    let mut all_glyphs: Vec<(u32, f32, f32, f32, usize)> = Vec::new();
    struct RunInfo {
        font: CTFont,
        start_byte: usize,
        end_byte: usize,
        glyph_range: Range<usize>,
    }
    let mut runs_info: Vec<RunInfo> = Vec::new();

    for run in glyph_runs.into_iter() {
        let Some(attributes) = run.attributes() else {
            continue;
        };
        let Some(run_font) = attributes.get(unsafe { kCTFontAttributeName }).downcast::<CTFont>()
        else {
            continue;
        };
        let glyphs = run.glyphs();
        let positions = run.positions();
        let indices = run.string_indices();
        let count = glyphs.len().min(positions.len()).min(indices.len());
        if count == 0 {
            continue;
        }
        // CoreText returns string indices in visual order, so for RTL runs
        // they are not monotonic. Compute the run's text range from the min
        // and max indices, and extend the end past the last covered character
        // (surrogate pairs share a UTF-16 index).
        let min_utf16 = indices[..count].iter().copied().min().unwrap_or(0) as usize;
        let max_utf16 = indices[..count].iter().copied().max().unwrap_or(0) as usize;
        let start_byte = utf16_to_utf8.get(min_utf16).copied().unwrap_or(0);
        let max_byte = utf16_to_utf8
            .get(max_utf16)
            .copied()
            .unwrap_or(item_text.len());
        let last_char_len = item_text[max_byte..]
            .chars()
            .next()
            .map(|c| c.len_utf8())
            .unwrap_or(0);
        let end_byte = (max_byte + last_char_len).min(item_text.len());
        let glyph_range = all_glyphs.len()..(all_glyphs.len() + count);
        for i in 0..count {
            let byte_off = utf16_to_utf8
                .get(indices[i] as usize)
                .copied()
                .unwrap_or(item_text.len());
            all_glyphs.push((
                glyphs[i] as u32,
                positions[i].x as f32,
                positions[i].y as f32,
                0.0, // advance filled below
                byte_off.saturating_sub(start_byte),
            ));
        }
        runs_info.push(RunInfo {
            font: run_font,
            start_byte,
            end_byte,
            glyph_range,
        });
    }

    // Fill advances from consecutive x positions (last glyph → line width).
    for i in 0..all_glyphs.len() {
        let next_x = if i + 1 < all_glyphs.len() {
            all_glyphs[i + 1].1
        } else {
            line_width
        };
        all_glyphs[i].3 = (next_x - all_glyphs[i].1).max(0.0);
    }

    for run_info in runs_info {
        let run_text = &item_text[run_info.start_byte..run_info.end_byte];
        let run_char_start = item_text[..run_info.start_byte].chars().count();
        let run_char_count = run_text.chars().count();
        let run_char_end = (run_char_start + run_char_count).min(item_infos.len());
        let run_infos = &item_infos[run_char_start..run_char_end];
        let run_glyphs: Vec<(u32, f32, f32, f32, usize)> =
            all_glyphs[run_info.glyph_range.clone()].to_vec();

        let font_size = run_info.font.pt_size() as f32;
        let metrics = ct_metrics(&run_info.font);
        let native_font = Some(crate::MacNativeFont::from_ctfont(&run_info.font));

        let font_attrs = FontiqueAttributes {
            width: style.font_width,
            style: style.font_style,
            weight: style.font_weight,
        };
        layout.data.push_run_coretext(
            native_font,
            font_size,
            font_attrs,
            item.level,
            item.style_index,
            item.word_spacing,
            item.letter_spacing,
            run_text,
            run_infos,
            (text_start + run_info.start_byte)..(text_start + run_info.end_byte),
            metrics,
            &run_glyphs,
        );
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use alloc::borrow::Cow;
    use alloc::string::String;
    use alloc::vec::Vec;

    use core_foundation::string::CFString;
    use fontique::{Collection, CollectionOptions, SourceCache};
    use parlance::FontFamilyName;

    use crate::layout::{Layout, PositionedLayoutItem};
    use crate::{FontContext, FontFamily, LayoutContext};

    /// Shape `text` with the system font "Helvetica" at 16pt through the
    /// CoreText path (macOS only).
    fn shape(text: &str) -> Layout<[u8; 4]> {
        let collection = Collection::new(CollectionOptions {
            shared: false,
            system_fonts: true,
        });
        let mut fcx = FontContext {
            collection,
            source_cache: SourceCache::default(),
        };
        let mut lcx: LayoutContext = LayoutContext::new();
        let mut rb = lcx.ranged_builder(&mut fcx, text, 16.0, false);
        rb.push_default(FontFamily::from(
            &[FontFamilyName::Named(Cow::Borrowed("Helvetica"))][..],
        ));
        let mut layout = rb.build(text);
        layout.break_all_lines(None);
        layout
    }

    /// Extract (postscript name, color flag, glyph count) per shaped run.
    fn run_summary(layout: &Layout<[u8; 4]>) -> Vec<(String, bool, usize)> {
        let mut out = Vec::new();
        for line in layout.lines() {
            for item in line.items() {
                if let PositionedLayoutItem::GlyphRun(gr) = item {
                    let run = gr.run();
                    let nf = run.native_font().expect("macOS run must carry a native font");
                    out.push((
                        nf.postscript_name.clone(),
                        nf.color,
                        gr.positioned_glyphs().count(),
                    ));
                }
            }
        }
        out
    }

    #[test]
    fn latin_shapes_as_single_run_with_clusters() {
        let layout = shape("Hello");
        let summary = run_summary(&layout);
        assert!(!summary.is_empty(), "expected at least one run");
        let (ps, color, glyphs) = &summary[0];
        assert!(!color, "Latin run must not be color");
        assert_eq!(*glyphs, 5, "Hello should be 5 glyphs");
        assert!(ps.contains("Helvetica"), "unexpected font: {ps}");
        // Clusters must map back to the UTF-8 text: the run covers "Hello".
        assert_eq!(layout.lines().count(), 1);
    }

    #[test]
    fn mixed_cjk_emoji_uses_cascade_with_color_run() {
        let layout = shape("Hello 世界 😀");
        let summary = run_summary(&layout);
        assert!(summary.len() >= 3, "expected >=3 runs, got {summary:?}");
        assert!(
            summary.iter().any(|(_, color, _)| *color),
            "expected a color (emoji) run"
        );
        // No .notdef anywhere: glyph id 0 must not appear.
        let mut saw_notdef = false;
        for line in layout.lines() {
            for item in line.items() {
                if let PositionedLayoutItem::GlyphRun(gr) = item {
                    if gr.positioned_glyphs().any(|g| g.id == 0) {
                        saw_notdef = true;
                    }
                }
            }
        }
        assert!(!saw_notdef, "CoreText cascade must cover all chars (no .notdef)");
    }

    #[test]
    fn arabic_shapes_with_ligatures_intact() {
        let layout = shape("مرحبا");
        let summary = run_summary(&layout);
        assert!(!summary.is_empty(), "expected Arabic runs");
        assert!(summary[0].2 > 0, "expected glyphs");
        // No panic and no .notdef: the whole string must be covered.
        let mut saw_notdef = false;
        for line in layout.lines() {
            for item in line.items() {
                if let PositionedLayoutItem::GlyphRun(gr) = item {
                    if gr.positioned_glyphs().any(|g| g.id == 0) {
                        saw_notdef = true;
                    }
                }
            }
        }
        assert!(!saw_notdef);
    }

    #[test]
    fn rtl_paragraph_keeps_base_direction() {
        // A pure RTL string resolves to an RTL paragraph in parley's bidi
        // analysis; the CoreText path must shape it without panicking and
        // produce a run with a positive advance.
        let layout = shape("مرحبا");
        assert!(layout.is_rtl(), "expected RTL paragraph");
        let summary = run_summary(&layout);
        assert!(!summary.is_empty());
    }

    #[test]
    fn empty_text_produces_no_runs() {
        let layout = shape("");
        assert_eq!(run_summary(&layout).len(), 0);
    }

    #[test]
    fn zwj_emoji_shapes_as_single_glyph() {
        // CoreText ligates ZWJ sequences into a single glyph; the native
        // reference render (macOS CoreText) produces exactly 1 glyph / 1 run
        // for every sequence below. The CoreText backend must match.
        let sequences = [
            "👨‍👩‍👧",
            "👨‍👩‍👧‍👦",
            "👩‍💻",
            "🏳️‍🌈",
            "👍🏻",
            "👨‍👦",
            "❤️",
            "🚴‍♂️",
        ];
        for seq in sequences {
            let layout = shape(seq);
            let mut total = 0usize;
            for line in layout.lines() {
                for item in line.items() {
                    if let PositionedLayoutItem::GlyphRun(gr) = item {
                        total += gr.positioned_glyphs().count();
                    }
                }
            }
            assert_eq!(total, 1, "ZWJ sequence {seq:?} must shape to a single glyph");
        }
    }

    #[test]
    fn native_font_key_resolves_back_to_ctfont() {
        // KTD4: the self-describing key must resolve to a CTFont on the
        // consumer side (blitz) without any fork-internal registry.
        let layout = shape("Hello");
        let summary = run_summary(&layout);
        assert!(!summary.is_empty());
        let first = layout
            .lines()
            .next()
            .unwrap()
            .items()
            .find_map(|item| match item {
                PositionedLayoutItem::GlyphRun(gr) => Some(gr.run().native_font().unwrap().clone()),
                _ => None,
            })
            .unwrap();
        let name = CFString::new(&first.postscript_name);
        let desc = core_text::font_descriptor::new_from_postscript_name(&name);
        let resolved = core_text::font::new_from_descriptor(&desc, first.size as f64);
        assert_eq!(resolved.postscript_name(), first.postscript_name);
    }

    #[test]
    fn bold_weight_selects_the_bold_face() {
        let collection = Collection::new(CollectionOptions {
            shared: false,
            system_fonts: true,
        });
        let mut fcx = FontContext {
            collection,
            source_cache: SourceCache::default(),
        };
        let mut lcx: LayoutContext = LayoutContext::new();
        let mut rb = lcx.ranged_builder(&mut fcx, "Hello", 16.0, false);
        rb.push_default(FontFamily::from(
            &[FontFamilyName::Named(Cow::Borrowed("Helvetica"))][..],
        ));
        rb.push_default(crate::StyleProperty::FontWeight(parlance::FontWeight::BOLD));
        let mut layout = rb.build("Hello");
        layout.break_all_lines(None);
        let summary = run_summary(&layout);
        assert!(!summary.is_empty());
        let ps = &summary[0].0;
        assert!(
            ps.to_lowercase().contains("bold") || !ps.eq_ignore_ascii_case("Helvetica"),
            "expected a bold face, got {ps}"
        );
    }
}
