use crate::core::alignment;
use crate::core::text::Hit;
use crate::core::{Font, Point, Rectangle, Size};
use crate::layer::Text;

use rustc_hash::{FxHashMap, FxHashSet};
use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::hash_map;
use std::hash::{BuildHasher, Hash, Hasher};
use std::sync::Arc;

#[allow(missing_debug_implementations)]
pub struct Pipeline {
    font_system: RefCell<glyphon::FontSystem>,
    renderers: Vec<glyphon::TextRenderer>,
    atlas: glyphon::TextAtlas,
    prepare_layer: usize,
    measurement_cache: RefCell<Cache>,
    render_cache: Cache,
}

impl Pipeline {
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        format: wgpu::TextureFormat,
    ) -> Self {
        Pipeline {
            font_system: RefCell::new(glyphon::FontSystem::new_with_fonts(
                [glyphon::fontdb::Source::Binary(Arc::new(
                    include_bytes!("../fonts/Iced-Icons.ttf").as_slice(),
                ))]
                .into_iter(),
            )),
            renderers: Vec::new(),
            atlas: glyphon::TextAtlas::new(device, queue, format),
            prepare_layer: 0,
            measurement_cache: RefCell::new(Cache::new()),
            render_cache: Cache::new(),
        }
    }

    pub fn load_font(&mut self, bytes: Cow<'static, [u8]>) {
        self.font_system.get_mut().db_mut().load_font_source(
            glyphon::fontdb::Source::Binary(Arc::new(bytes.into_owned())),
        );
    }

    pub fn prepare(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        sections: &[Text<'_>],
        bounds: Rectangle,
        scale_factor: f32,
        target_size: Size<u32>,
    ) -> bool {
        if self.renderers.len() <= self.prepare_layer {
            self.renderers
                .push(glyphon::TextRenderer::new(device, queue));
        }

        let font_system = self.font_system.get_mut();
        let renderer = &mut self.renderers[self.prepare_layer];

        let keys: Vec<_> = sections
            .iter()
            .map(|section| {
                let (key, _) = self.render_cache.allocate(
                    font_system,
                    Key {
                        content: section.content,
                        size: section.size * scale_factor,
                        font: section.font,
                        bounds: Size {
                            width: (section.bounds.width * scale_factor).ceil(),
                            height: (section.bounds.height * scale_factor)
                                .ceil(),
                        },
                    },
                );

                key
            })
            .collect();

        let bounds = glyphon::TextBounds {
            left: (bounds.x * scale_factor) as i32,
            top: (bounds.y * scale_factor) as i32,
            right: ((bounds.x + bounds.width) * scale_factor) as i32,
            bottom: ((bounds.y + bounds.height) * scale_factor) as i32,
        };

        let text_areas =
            sections.iter().zip(keys.iter()).map(|(section, key)| {
                let buffer =
                    self.render_cache.get(key).expect("Get cached buffer");

                let x = section.bounds.x * scale_factor;
                let y = section.bounds.y * scale_factor;

                let (total_lines, max_width) = buffer
                    .layout_runs()
                    .enumerate()
                    .fold((0, 0.0), |(_, max), (i, buffer)| {
                        (i + 1, buffer.line_w.max(max))
                    });

                let total_height =
                    total_lines as f32 * section.size * 1.2 * scale_factor;

                let left = match section.horizontal_alignment {
                    alignment::Horizontal::Left => x,
                    alignment::Horizontal::Center => x - max_width / 2.0,
                    alignment::Horizontal::Right => x - max_width,
                };

                let top = match section.vertical_alignment {
                    alignment::Vertical::Top => y,
                    alignment::Vertical::Center => y - total_height / 2.0,
                    alignment::Vertical::Bottom => y - total_height,
                };

                glyphon::TextArea {
                    buffer,
                    left: left as i32,
                    top: top as i32,
                    bounds,
                    default_color: {
                        let [r, g, b, a] = section.color.into_linear();

                        glyphon::Color::rgba(
                            (r * 255.0) as u8,
                            (g * 255.0) as u8,
                            (b * 255.0) as u8,
                            (a * 255.0) as u8,
                        )
                    },
                }
            });

        let result = renderer.prepare(
            device,
            queue,
            font_system,
            &mut self.atlas,
            glyphon::Resolution {
                width: target_size.width,
                height: target_size.height,
            },
            text_areas,
            &mut glyphon::SwashCache::new(),
        );

        match result {
            Ok(()) => {
                self.prepare_layer += 1;

                true
            }
            Err(glyphon::PrepareError::AtlasFull(content_type)) => {
                self.prepare_layer = 0;

                #[allow(clippy::needless_bool)]
                if self.atlas.grow(device, content_type) {
                    false
                } else {
                    // If the atlas cannot grow, then all bets are off.
                    // Instead of panicking, we will just pray that the result
                    // will be somewhat readable...
                    true
                }
            }
        }
    }

    pub fn render<'a>(
        &'a self,
        layer: usize,
        bounds: Rectangle<u32>,
        render_pass: &mut wgpu::RenderPass<'a>,
    ) {
        let renderer = &self.renderers[layer];

        render_pass.set_scissor_rect(
            bounds.x,
            bounds.y,
            bounds.width,
            bounds.height,
        );

        renderer
            .render(&self.atlas, render_pass)
            .expect("Render text");
    }

    pub fn end_frame(&mut self) {
        self.atlas.trim();
        self.render_cache.trim();

        self.prepare_layer = 0;
    }

    pub fn measure(
        &self,
        content: &str,
        size: f32,
        font: Font,
        bounds: Size,
    ) -> (f32, f32) {
        let mut measurement_cache = self.measurement_cache.borrow_mut();

        let (_, paragraph) = measurement_cache.allocate(
            &mut self.font_system.borrow_mut(),
            Key {
                content,
                size,
                font,
                bounds,
            },
        );

        let (total_lines, max_width) = paragraph
            .layout_runs()
            .enumerate()
            .fold((0, 0.0), |(_, max), (i, buffer)| {
                (i + 1, buffer.line_w.max(max))
            });

        (max_width, size * 1.2 * total_lines as f32)
    }

    pub fn hit_test(
        &self,
        content: &str,
        size: f32,
        font: Font,
        bounds: Size,
        point: Point,
        _nearest_only: bool,
    ) -> Option<Hit> {
        let mut measurement_cache = self.measurement_cache.borrow_mut();

        let (_, paragraph) = measurement_cache.allocate(
            &mut self.font_system.borrow_mut(),
            Key {
                content,
                size,
                font,
                bounds,
            },
        );

        let cursor = paragraph.hit(point.x, point.y)?;

        Some(Hit::CharOffset(cursor.index))
    }

    pub fn trim_measurement_cache(&mut self) {
        self.measurement_cache.borrow_mut().trim();
    }
}

fn to_family(font: Font) -> glyphon::Family<'static> {
    match font {
        Font::Name(name) => glyphon::Family::Name(name),
        Font::SansSerif => glyphon::Family::SansSerif,
        Font::Serif => glyphon::Family::Serif,
        Font::Cursive => glyphon::Family::Cursive,
        Font::Fantasy => glyphon::Family::Fantasy,
        Font::Monospace => glyphon::Family::Monospace,
    }
}

struct Cache {
    entries: FxHashMap<KeyHash, glyphon::Buffer>,
    recently_used: FxHashSet<KeyHash>,
    hasher: HashBuilder,
}

#[cfg(not(target_arch = "wasm32"))]
type HashBuilder = twox_hash::RandomXxHashBuilder64;

#[cfg(target_arch = "wasm32")]
type HashBuilder = std::hash::BuildHasherDefault<twox_hash::XxHash64>;

impl Cache {
    fn new() -> Self {
        Self {
            entries: FxHashMap::default(),
            recently_used: FxHashSet::default(),
            hasher: HashBuilder::default(),
        }
    }

    fn get(&self, key: &KeyHash) -> Option<&glyphon::Buffer> {
        self.entries.get(key)
    }

    fn allocate(
        &mut self,
        font_system: &mut glyphon::FontSystem,
        key: Key<'_>,
    ) -> (KeyHash, &mut glyphon::Buffer) {
        let hash = {
            let mut hasher = self.hasher.build_hasher();

            key.content.hash(&mut hasher);
            key.size.to_bits().hash(&mut hasher);
            key.font.hash(&mut hasher);
            key.bounds.width.to_bits().hash(&mut hasher);
            key.bounds.height.to_bits().hash(&mut hasher);

            hasher.finish()
        };

        if let hash_map::Entry::Vacant(entry) = self.entries.entry(hash) {
            let metrics = glyphon::Metrics::new(key.size, key.size * 1.2);
            let mut buffer = glyphon::Buffer::new(font_system, metrics);

            buffer.set_size(
                font_system,
                key.bounds.width,
                key.bounds.height.max(key.size * 1.2),
            );
            buffer.set_text(
                font_system,
                key.content,
                glyphon::Attrs::new()
                    .family(to_family(key.font))
                    .monospaced(matches!(key.font, Font::Monospace)),
            );

            let _ = entry.insert(buffer);
        }

        let _ = self.recently_used.insert(hash);

        (hash, self.entries.get_mut(&hash).unwrap())
    }

    fn trim(&mut self) {
        self.entries
            .retain(|key, _| self.recently_used.contains(key));

        self.recently_used.clear();
    }
}

#[derive(Debug, Clone, Copy)]
struct Key<'a> {
    content: &'a str,
    size: f32,
    font: Font,
    bounds: Size,
}

type KeyHash = u64;
