use std::{rc::Rc, str::FromStr, sync::Arc};

use fontconfig::{FontConfig, OwnedPattern};
use freetype::{bitmap::PixelMode, face::LoadFlag, Face, Library};
use idmap::IdMap;
use vulkano::{command_buffer::CommandBufferUsage, format::Format, image::Image};

use crate::graphics::WlxGraphics;

pub struct FontCache {
    primary_font: Arc<str>,
    fc: FontConfig,
    ft: Library,
    collections: IdMap<isize, FontCollection>,
}

struct FontCollection {
    fonts: Vec<Font>,
    cp_map: IdMap<usize, usize>,
}

struct Font {
    face: Face,
    glyphs: IdMap<usize, Rc<Glyph>>,
}

pub struct Glyph {
    pub tex: Option<Arc<Image>>,
    pub top: f32,
    pub left: f32,
    pub width: f32,
    pub height: f32,
    pub advance: f32,
}

impl FontCache {
    pub fn new(primary_font: Arc<str>) -> anyhow::Result<Self> {
        let ft = Library::init()?;
        let fc = FontConfig::default();

        Ok(FontCache {
            primary_font,
            fc,
            ft,
            collections: IdMap::new(),
        })
    }

    pub fn get_text_size(
        &mut self,
        text: &str,
        size: isize,
        graphics: Arc<WlxGraphics>,
    ) -> anyhow::Result<(f32, f32)> {
        let sizef = size as f32;

        let height = sizef + ((text.lines().count() as f32) - 1f32) * (sizef * 1.5);

        let mut max_w = sizef * 0.33;
        for line in text.lines() {
            let w: f32 = line
                .chars()
                .filter_map(|c| {
                    self.get_glyph_for_cp(c as usize, size, graphics.clone())
                        .map(|glyph| glyph.advance)
                        .ok()
                })
                .sum();

            if w > max_w {
                max_w = w;
            }
        }
        Ok((max_w, height))
    }

    pub fn get_glyphs(
        &mut self,
        text: &str,
        size: isize,
        graphics: Arc<WlxGraphics>,
    ) -> anyhow::Result<Vec<Rc<Glyph>>> {
        let mut glyphs = Vec::new();
        for line in text.lines() {
            for c in line.chars() {
                glyphs.push(self.get_glyph_for_cp(c as usize, size, graphics.clone())?);
            }
        }
        Ok(glyphs)
    }

    fn get_font_for_cp(&mut self, cp: usize, size: isize) -> usize {
        if !self.collections.contains_key(size) {
            self.collections.insert(
                size,
                FontCollection {
                    fonts: Vec::new(),
                    cp_map: IdMap::new(),
                },
            );
        }
        let coll = self.collections.get_mut(size).unwrap(); // safe because of the insert above

        if let Some(font) = coll.cp_map.get(cp) {
            return *font;
        }

        let primary_font = self.primary_font.clone();
        let pattern_str = format!("{primary_font}:size={size}:charset={cp:04x}");
        let mut pattern = OwnedPattern::from_str(&pattern_str).unwrap(); // safe because PRIMARY_FONT is const
        self.fc
            .substitute(&mut pattern, fontconfig::MatchKind::Pattern);
        pattern.default_substitute();

        let pattern = pattern.font_match(&mut self.fc);

        if let Some(path) = pattern.filename() {
            log::debug!(
                "Loading font: {} {}pt",
                pattern.name().unwrap_or(path),
                size
            );

            let font_idx = pattern.face_index().unwrap_or(0);

            let face = match self.ft.new_face(path, font_idx as _) {
                Ok(face) => face,
                Err(e) => {
                    log::warn!("Failed to load font at {}: {:?}", path, e);
                    coll.cp_map.insert(cp, 0);
                    return 0;
                }
            };
            match face.set_char_size(size << 6, size << 6, 96, 96) {
                Ok(_) => {}
                Err(e) => {
                    log::warn!("Failed to set font size: {:?}", e);
                    coll.cp_map.insert(cp, 0);
                    return 0;
                }
            };

            let idx = coll.fonts.len();
            for cp in 0..0xFFFF {
                if coll.cp_map.contains_key(cp) {
                    continue;
                }
                let g = face.get_char_index(cp);
                if g.is_some() {
                    coll.cp_map.insert(cp, idx);
                }
            }

            let zero_glyph = Rc::new(Glyph {
                tex: None,
                top: 0.,
                left: 0.,
                width: 0.,
                height: 0.,
                advance: size as f32 / 3.,
            });
            let mut glyphs = IdMap::new();
            glyphs.insert(0, zero_glyph);

            let font = Font { face, glyphs };
            coll.fonts.push(font);

            return idx;
        }
        coll.cp_map.insert(cp, 0);
        0
    }

    fn get_glyph_for_cp(
        &mut self,
        cp: usize,
        size: isize,
        graphics: Arc<WlxGraphics>,
    ) -> anyhow::Result<Rc<Glyph>> {
        let key = self.get_font_for_cp(cp, size);

        let Some(font) = &mut self.collections[size].fonts.get_mut(key) else {
            log::warn!("No font found for codepoint: {}", cp);
            return Ok(self.collections[size].fonts[0].glyphs[0].clone());
        };

        if let Some(glyph) = font.glyphs.get(cp) {
            return Ok(glyph.clone());
        }

        if font.face.load_char(cp, LoadFlag::DEFAULT).is_err() {
            return Ok(font.glyphs[0].clone());
        }

        let glyph = font.face.glyph();
        if glyph.render_glyph(freetype::RenderMode::Normal).is_err() {
            return Ok(font.glyphs[0].clone());
        }

        let bmp = glyph.bitmap();
        let buf = bmp.buffer().to_vec();
        if buf.is_empty() {
            return Ok(font.glyphs[0].clone());
        }

        let metrics = glyph.metrics();

        let format = match bmp.pixel_mode() {
            Ok(PixelMode::Gray) => Format::R8_UNORM,
            Ok(PixelMode::Gray2) => Format::R16_SFLOAT,
            Ok(PixelMode::Gray4) => Format::R32_SFLOAT,
            _ => return Ok(font.glyphs[0].clone()),
        };

        let mut cmd_buffer = graphics.create_command_buffer(CommandBufferUsage::OneTimeSubmit)?;
        let texture = cmd_buffer.texture2d(bmp.width() as _, bmp.rows() as _, format, &buf)?;
        cmd_buffer.build_and_execute_now()?;

        let g = Glyph {
            tex: Some(texture),
            top: (metrics.horiBearingY >> 6i64) as _,
            left: (metrics.horiBearingX >> 6i64) as _,
            advance: (metrics.horiAdvance >> 6i64) as _,
            width: bmp.width() as _,
            height: bmp.rows() as _,
        };

        font.glyphs.insert(cp, Rc::new(g));
        Ok(font.glyphs[cp].clone())
    }
}
