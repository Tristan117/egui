use std::collections::BTreeMap;

use ab_glyph::{Font as _, PxScaleFont, ScaleFont as _};
use emath::{GuiRounding as _, OrderedFloat, Vec2, vec2};

use crate::{
    TextureAtlas,
    text::{
        FontTweak,
        fonts::{CachedFamily, FontFaceKey},
    },
};

// ----------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
pub struct UvRect {
    /// X/Y offset for nice rendering (unit: points).
    pub offset: Vec2,

    /// Screen size (in points) of this glyph.
    /// Note that the height is different from the font height.
    pub size: Vec2,

    /// Top left corner UV in texture.
    pub min: [u16; 2],

    /// Bottom right corner (exclusive).
    pub max: [u16; 2],
}

impl UvRect {
    pub fn is_nothing(&self) -> bool {
        self.min == self.max
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct GlyphInfo {
    /// Used for pair-kerning.
    ///
    /// Doesn't need to be unique.
    /// Use `ab_glyph::GlyphId(0)` if you just want to have an id, and don't care.
    pub(crate) id: ab_glyph::GlyphId,

    /// In [`ab_glyph`]s "unscaled" coordinate system.
    pub advance_width_unscaled: OrderedFloat<f32>,

    /// Whether this glyph has any outlines.
    pub visible: bool,
}

impl Default for GlyphInfo {
    /// Basically a zero-width space.
    fn default() -> Self {
        Self {
            id: ab_glyph::GlyphId(0),
            advance_width_unscaled: 0.0.into(),
            visible: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct GlyphAllocation {
    /// Used for pair-kerning.
    ///
    /// Doesn't need to be unique.
    /// Use `ab_glyph::GlyphId(0)` if you just want to have an id, and don't care.
    pub(crate) id: ab_glyph::GlyphId,

    /// Unit: points.
    pub advance_width: f32,

    /// UV rectangle for drawing.
    pub uv_rect: UvRect,
}

// ----------------------------------------------------------------------------

/// A specific font with a size.
/// The interface uses points as the unit for everything.
pub struct FontImpl {
    name: String,
    ab_glyph_font: ab_glyph::FontArc,
    tweak: FontTweak,
    glyph_info_cache: ahash::HashMap<char, GlyphInfo>,
    glyph_alloc_cache: ahash::HashMap<(GlyphInfo, OrderedFloat<f32>), GlyphAllocation>,
}

trait FontExt {
    fn pt_scaled(&self, scale: f32) -> PxScaleFont<&'_ Self>;

    fn pt_scale_factor(&self, scale: f32) -> f32;
}

impl<T> FontExt for T
where
    T: ab_glyph::Font,
{
    fn pt_scaled(&self, scale: f32) -> PxScaleFont<&'_ Self> {
        PxScaleFont {
            font: self,
            scale: self.pt_scale_factor(scale).into(),
        }
    }

    fn pt_scale_factor(&self, scale: f32) -> f32 {
        let units_per_em = self.units_per_em().unwrap_or_else(|| {
            panic!("The font unit size exceeds the expected range (16..=16384)")
        });
        let font_scaling = self.height_unscaled() / units_per_em;
        scale * font_scaling
    }
}

impl FontImpl {
    pub fn new(name: String, ab_glyph_font: ab_glyph::FontArc, tweak: FontTweak) -> Self {
        Self {
            name,
            ab_glyph_font,
            tweak,
            glyph_info_cache: Default::default(),
            glyph_alloc_cache: Default::default(),
        }
    }

    /// Code points that will always be replaced by the replacement character.
    ///
    /// See also [`invisible_char`].
    fn ignore_character(&self, chr: char) -> bool {
        use crate::text::FontDefinitions;

        if !FontDefinitions::builtin_font_names().contains(&self.name.as_str()) {
            return false;
        }

        matches!(
            chr,
            // Strip out a religious symbol with secondary nefarious interpretation:
            '\u{534d}' | '\u{5350}' |

            // Ignore ubuntu-specific stuff in `Ubuntu-Light.ttf`:
            '\u{E0FF}' | '\u{EFFD}' | '\u{F0FF}' | '\u{F200}'
        )
    }

    /// An un-ordered iterator over all supported characters.
    fn characters(&self) -> impl Iterator<Item = char> + '_ {
        self.ab_glyph_font
            .codepoint_ids()
            .map(|(_, chr)| chr)
            .filter(|&chr| !self.ignore_character(chr))
    }

    /// `\n` will result in `None`
    pub(super) fn glyph_info(&mut self, c: char) -> Option<GlyphInfo> {
        {
            if let Some(glyph_info) = self.glyph_info_cache.get(&c) {
                return Some(*glyph_info);
            }
        }

        if self.ignore_character(c) {
            return None; // these will result in the replacement character when rendering
        }

        if c == '\t' {
            if let Some(space) = self.glyph_info(' ') {
                let glyph_info = GlyphInfo {
                    advance_width_unscaled: (crate::text::TAB_SIZE as f32
                        * space.advance_width_unscaled.0)
                        .into(),
                    ..space
                };
                self.glyph_info_cache.insert(c, glyph_info);
                return Some(glyph_info);
            }
        }

        if c == '\u{2009}' {
            // Thin space, often used as thousands deliminator: 1 234 567 890
            // https://www.compart.com/en/unicode/U+2009
            // https://en.wikipedia.org/wiki/Thin_space

            if let Some(space) = self.glyph_info(' ') {
                let em = self.ab_glyph_font.units_per_em().unwrap_or(1.0);
                let advance_width = f32::min(em / 6.0, space.advance_width_unscaled.0 * 0.5);
                let glyph_info = GlyphInfo {
                    advance_width_unscaled: advance_width.into(),
                    ..space
                };
                self.glyph_info_cache.insert(c, glyph_info);
                return Some(glyph_info);
            }
        }

        if invisible_char(c) {
            let glyph_info = GlyphInfo::default();
            self.glyph_info_cache.insert(c, glyph_info);
            return Some(glyph_info);
        }

        // Add new character:
        let glyph_id = self.ab_glyph_font.glyph_id(c);

        if glyph_id.0 == 0 {
            None // unsupported character
        } else {
            let glyph_info = GlyphInfo {
                id: glyph_id,
                advance_width_unscaled: self.ab_glyph_font.h_advance_unscaled(glyph_id).into(),
                visible: true,
            };
            self.glyph_info_cache.insert(c, glyph_info);
            Some(glyph_info)
        }
    }

    #[inline]
    pub fn pair_kerning(
        &self,
        last_glyph_id: ab_glyph::GlyphId,
        glyph_id: ab_glyph::GlyphId,
        font_size: f32,
        pixels_per_point: f32,
    ) -> f32 {
        // Round to an even number of physical pixels to get even kerning.
        // See https://github.com/emilk/egui/issues/382
        self.ab_glyph_font
            .pt_scaled((font_size * self.tweak.scale * pixels_per_point).round())
            .kern(last_glyph_id, glyph_id)
            / pixels_per_point
    }

    /// Height of one row of text in points.
    ///
    /// Returns a value rounded to [`emath::GUI_ROUNDING`].
    #[inline(always)]
    pub fn row_height(&self, font_size: f32) -> f32 {
        let font = self.ab_glyph_font.pt_scaled(font_size);

        font.ascent().round_ui() - font.descent().round_ui() + font.line_gap().round_ui()
    }

    /// This is the distance from the top to the baseline.
    ///
    /// Unit: points.
    #[inline(always)]
    pub fn ascent(&self, font_size: f32) -> f32 {
        self.ab_glyph_font.pt_scaled(font_size).ascent().round_ui()
    }

    pub fn allocate_glyph(
        &mut self,
        glyph_info: GlyphInfo,
        atlas: &mut TextureAtlas,
        font_size: f32,
        pixels_per_point: f32,
    ) -> GlyphAllocation {
        if !glyph_info.visible {
            return GlyphAllocation::default();
        }
        // Round to an even number of physical pixels to get even kerning.
        // See https://github.com/emilk/egui/issues/382
        let scale = self
            .ab_glyph_font
            .pt_scale_factor(font_size * self.tweak.scale * pixels_per_point)
            .round();
        let entry = match self.glyph_alloc_cache.entry((glyph_info, scale.into())) {
            std::collections::hash_map::Entry::Occupied(glyph_alloc) => {
                return *glyph_alloc.get();
            }
            std::collections::hash_map::Entry::Vacant(entry) => entry,
        };

        assert!(glyph_info.id.0 != 0, "Can't allocate glyph for id 0");

        let glyph = glyph_info
            .id
            .with_scale_and_position(scale, ab_glyph::Point { x: 0.0, y: 0.0 });

        // Tweak the scale as the user desired
        let y_offset_in_points = {
            let logically_scaled = self.ab_glyph_font.pt_scaled(font_size * pixels_per_point);
            let scale_in_points = scale / pixels_per_point;

            let y_offset_points =
                ((scale_in_points * self.tweak.y_offset_factor) + self.tweak.y_offset).round_ui();

            // Center scaled glyphs properly:
            let height = (logically_scaled.ascent() / pixels_per_point).round_ui()
                + (logically_scaled.descent() / pixels_per_point).round_ui();
            let y_offset_points = y_offset_points - (1.0 - self.tweak.scale) * 0.5 * height;

            // Round to closest pixel:
            (y_offset_points * pixels_per_point).round() / pixels_per_point
        };

        let uv_rect = self.ab_glyph_font.outline_glyph(glyph).map(|glyph| {
            let bb = glyph.px_bounds();
            let glyph_width = bb.width() as usize;
            let glyph_height = bb.height() as usize;
            if glyph_width == 0 || glyph_height == 0 {
                UvRect::default()
            } else {
                let glyph_pos = {
                    let text_alpha_from_coverage = atlas.text_alpha_from_coverage;
                    let (glyph_pos, image) = atlas.allocate((glyph_width, glyph_height));
                    glyph.draw(|x, y, v| {
                        if 0.0 < v {
                            let px = glyph_pos.0 + x as usize;
                            let py = glyph_pos.1 + y as usize;
                            image[(px, py)] = text_alpha_from_coverage.color_from_coverage(v);
                        }
                    });
                    glyph_pos
                };

                let offset_in_pixels = vec2(bb.min.x, bb.min.y);
                let offset = offset_in_pixels / pixels_per_point + y_offset_in_points * Vec2::Y;
                UvRect {
                    offset,
                    size: vec2(glyph_width as f32, glyph_height as f32) / pixels_per_point,
                    min: [glyph_pos.0 as u16, glyph_pos.1 as u16],
                    max: [
                        (glyph_pos.0 + glyph_width) as u16,
                        (glyph_pos.1 + glyph_height) as u16,
                    ],
                }
            }
        });
        let uv_rect = uv_rect.unwrap_or_default();

        let allocation = GlyphAllocation {
            id: glyph_info.id,
            advance_width: (glyph_info.advance_width_unscaled.0 * scale
                / self.ab_glyph_font.height_unscaled())
                / pixels_per_point,
            uv_rect,
        };
        entry.insert(allocation);
        allocation
    }
}

// TODO(emilk): rename?
/// Wrapper over multiple [`FontImpl`] (e.g. a primary + fallbacks for emojis)
pub struct Font<'a> {
    pub(super) fonts_by_id: &'a mut nohash_hasher::IntMap<FontFaceKey, FontImpl>,
    pub(super) cached_family: &'a mut CachedFamily,
    pub(super) atlas: &'a mut TextureAtlas,
}

impl Font<'_> {
    pub fn preload_characters(&mut self, s: &str) {
        for c in s.chars() {
            self.glyph_info(c);
        }
    }

    pub fn preload_common_characters(&mut self) {
        // Preload the printable ASCII characters [32, 126] (which excludes control codes):
        const FIRST_ASCII: usize = 32; // 32 == space
        const LAST_ASCII: usize = 126;
        for c in (FIRST_ASCII..=LAST_ASCII).map(|c| c as u8 as char) {
            self.glyph_info(c);
        }
        self.glyph_info('°');
        self.glyph_info(crate::text::PASSWORD_REPLACEMENT_CHAR);
    }

    /// All supported characters, and in which font they are available in.
    pub fn characters(&mut self) -> &BTreeMap<char, Vec<String>> {
        self.cached_family.characters.get_or_insert_with(|| {
            let mut characters: BTreeMap<char, Vec<String>> = Default::default();
            for font_id in &self.cached_family.fonts {
                let font = self.fonts_by_id.get(font_id).expect("Nonexistent font ID");
                for chr in font.characters() {
                    characters.entry(chr).or_default().push(font.name.clone());
                }
            }
            characters
        })
    }

    /// Height of one row of text. In points.
    ///
    /// Returns a value rounded to [`emath::GUI_ROUNDING`].
    #[inline(always)]
    pub fn row_height(&self, font_size: f32) -> f32 {
        let Some(first_font) = self
            .cached_family
            .fonts
            .first()
            .and_then(|key| self.fonts_by_id.get(key))
        else {
            return 0.0;
        };
        first_font.row_height(font_size)
    }

    /// Width of this character in points.
    pub fn glyph_width(&mut self, c: char, font_size: f32) -> f32 {
        let (key, glyph_info) = self.glyph_info(c);
        let font = &self
            .fonts_by_id
            .get(&key)
            .expect("Nonexistent font ID")
            .ab_glyph_font;
        glyph_info.advance_width_unscaled.0 * font.pt_scale_factor(font_size)
            / font.height_unscaled()
    }

    /// Can we display this glyph?
    pub fn has_glyph(&mut self, c: char) -> bool {
        self.glyph_info(c) != self.cached_family.replacement_glyph // TODO(emilk): this is a false negative if the user asks about the replacement character itself 🤦‍♂️
    }

    /// Can we display all the glyphs in this text?
    pub fn has_glyphs(&mut self, s: &str) -> bool {
        s.chars().all(|c| self.has_glyph(c))
    }

    /// `\n` will (intentionally) show up as the replacement character.
    fn glyph_info(&mut self, c: char) -> (FontFaceKey, GlyphInfo) {
        if let Some(font_index_glyph_info) = self.cached_family.glyph_info_cache.get(&c) {
            return *font_index_glyph_info;
        }

        let font_index_glyph_info = self
            .cached_family
            .glyph_info_no_cache_or_fallback(c, self.fonts_by_id);
        let font_index_glyph_info =
            font_index_glyph_info.unwrap_or(self.cached_family.replacement_glyph);
        self.cached_family
            .glyph_info_cache
            .insert(c, font_index_glyph_info);
        font_index_glyph_info
    }

    #[inline]
    pub(crate) fn font_impl_and_glyph_info(
        &mut self,
        c: char,
    ) -> (Option<&mut FontImpl>, GlyphInfo) {
        if self.cached_family.fonts.is_empty() {
            return (None, self.cached_family.replacement_glyph.1);
        }
        let (key, glyph_info) = self.glyph_info(c);
        let font_impl = self.fonts_by_id.get_mut(&key).expect("Nonexistent font ID");
        (Some(font_impl), glyph_info)
    }

    #[inline]
    pub(crate) fn font_impl_and_glyph_alloc(
        &mut self,
        c: char,
        font_size: f32,
        pixels_per_point: f32,
    ) -> (Option<&FontImpl>, GlyphAllocation) {
        if self.cached_family.fonts.is_empty() {
            return (None, Default::default());
        }
        let (key, glyph_info) = self.glyph_info(c);
        let font_impl = self.fonts_by_id.get_mut(&key).expect("Nonexistent font ID");
        let allocated_glyph =
            font_impl.allocate_glyph(glyph_info, self.atlas, font_size, pixels_per_point);
        (Some(font_impl), allocated_glyph)
    }

    pub(crate) fn ascent(&self, font_size: f32) -> f32 {
        if let Some(first) = self.cached_family.fonts.first() {
            let first = self.fonts_by_id.get(first).expect("Nonexistent font ID");
            first.ascent(font_size)
        } else {
            self.row_height(font_size)
        }
    }
}

/// Code points that will always be invisible (zero width).
///
/// See also [`FontImpl::ignore_character`].
#[inline]
fn invisible_char(c: char) -> bool {
    if c == '\r' {
        // A character most vile and pernicious. Don't display it.
        return true;
    }

    // See https://github.com/emilk/egui/issues/336

    // From https://www.fileformat.info/info/unicode/category/Cf/list.htm

    // TODO(emilk): heed bidi characters

    matches!(
        c,
        '\u{200B}' // ZERO WIDTH SPACE
            | '\u{200C}' // ZERO WIDTH NON-JOINER
            | '\u{200D}' // ZERO WIDTH JOINER
            | '\u{200E}' // LEFT-TO-RIGHT MARK
            | '\u{200F}' // RIGHT-TO-LEFT MARK
            | '\u{202A}' // LEFT-TO-RIGHT EMBEDDING
            | '\u{202B}' // RIGHT-TO-LEFT EMBEDDING
            | '\u{202C}' // POP DIRECTIONAL FORMATTING
            | '\u{202D}' // LEFT-TO-RIGHT OVERRIDE
            | '\u{202E}' // RIGHT-TO-LEFT OVERRIDE
            | '\u{2060}' // WORD JOINER
            | '\u{2061}' // FUNCTION APPLICATION
            | '\u{2062}' // INVISIBLE TIMES
            | '\u{2063}' // INVISIBLE SEPARATOR
            | '\u{2064}' // INVISIBLE PLUS
            | '\u{2066}' // LEFT-TO-RIGHT ISOLATE
            | '\u{2067}' // RIGHT-TO-LEFT ISOLATE
            | '\u{2068}' // FIRST STRONG ISOLATE
            | '\u{2069}' // POP DIRECTIONAL ISOLATE
            | '\u{206A}' // INHIBIT SYMMETRIC SWAPPING
            | '\u{206B}' // ACTIVATE SYMMETRIC SWAPPING
            | '\u{206C}' // INHIBIT ARABIC FORM SHAPING
            | '\u{206D}' // ACTIVATE ARABIC FORM SHAPING
            | '\u{206E}' // NATIONAL DIGIT SHAPES
            | '\u{206F}' // NOMINAL DIGIT SHAPES
            | '\u{FEFF}' // ZERO WIDTH NO-BREAK SPACE
    )
}
