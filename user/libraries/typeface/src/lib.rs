//! Shared user-space font matching, layout and rasterization through Fontconfig and PangoCairo.
extern crate liteos_runtime as _;

#[path = "pango/ffi.rs"]
mod ffi;

use std::{
    ffi::CString,
    io,
    ops::Range,
    os::unix::ffi::OsStrExt,
    path::Path,
    ptr::{NonNull, null_mut},
    rc::Rc,
    sync::LazyLock,
};

use ffi::{Api, Object, Rectangle};

const PANGO_SCALE: i32 = 1024;
const CAIRO_FONT_TYPE_FT: i32 = 1;
const CAIRO_FORMAT_ARGB32: i32 = 0;
const CAIRO_ANTIALIAS_NONE: i32 = 1;
const CAIRO_ANTIALIAS_GRAY: i32 = 2;
const CAIRO_HINT_STYLE_FULL: i32 = 4;
const CAIRO_HINT_METRICS_ON: i32 = 2;
const PANGO_WRAP_WORD_CHAR: i32 = 2;
const PANGO_STYLE_ITALIC: i32 = 2;

/// Font request in logical points, with a Fontconfig family and numeric OpenType weight.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LogicalFont {
    pub face: String,
    pub points: u16,
    pub weight: u16,
}

impl LogicalFont {
    /// Converts the logical point size to pixels at 96 DPI times `scale`.
    pub fn ppem(&self, scale: u32) -> u32 {
        (u32::from(self.points) * 96 * scale + 36) / 72
    }
}

// The immutable symbol table stays loaded until process exit: GLib registers Pango types
// process-wide and can retain callbacks after the last FontMap is destroyed.
static NATIVE: LazyLock<io::Result<Api>> = LazyLock::new(Api::load);

struct FontMap {
    api: &'static Api,
    handle: NonNull<std::ffi::c_void>,
}

impl Drop for FontMap {
    fn drop(&mut self) {
        // SAFETY: The font map reference came from a Pango constructor and is released once.
        unsafe { (self.api.g_object_unref)(self.handle.as_ptr()) };
    }
}

/// An owned Fontconfig-backed font map shared by the UI's font contexts.
pub struct FontCatalog(Rc<FontMap>);

impl FontCatalog {
    /// Opens the system and per-user Fontconfig policy. Returns a loader/configuration error on failure.
    pub fn system() -> io::Result<Self> {
        Self::open(None)
    }

    /// Opens the same system catalog and adds application-local faces from `directory`.
    /// Returns an error if the native stack or directory is unavailable.
    pub fn load(directory: &Path) -> io::Result<Self> {
        let directory = CString::new(directory.as_os_str().as_bytes())?;
        Self::open(Some(&directory))
    }

    fn open(directory: Option<&CString>) -> io::Result<Self> {
        let api = NATIVE
            .as_ref()
            .map_err(|error| io::Error::new(error.kind(), error.to_string()))?;
        // SAFETY: Native references stay local during initialization. The map takes a reference
        // to the config, and both initial references are released on every path.
        let handle = unsafe {
            let config = (api.fc_init_load_config_and_fonts)();
            if config.is_null() {
                return Err(io::Error::other("Fontconfig cannot load fonts"));
            }
            if let Some(directory) = directory
                && (api.fc_config_app_font_add_dir)(config, directory.as_ptr().cast()) == 0
            {
                (api.fc_config_destroy)(config);
                return Err(io::Error::other("Fontconfig cannot load application fonts"));
            }
            let handle = (api.pango_cairo_font_map_new_for_font_type)(CAIRO_FONT_TYPE_FT);
            if !handle.is_null() {
                (api.pango_fc_font_map_set_config)(handle, config);
            }
            (api.fc_config_destroy)(config);
            NonNull::new(handle)
                .ok_or_else(|| io::Error::other("Pango FreeType backend unavailable"))?
        };
        Ok(Self(Rc::new(FontMap { api, handle })))
    }

    /// Resolves a font at the given scale and antialiasing policy. Fontconfig supplies fallback faces.
    pub fn resolve(&self, font: &LogicalFont, scale: u32, smoothing: bool) -> Typeface {
        let api = self.0.api;
        let ppem = font.ppem(scale.max(1)).max(1);
        let family = CString::new(font.face.as_str()).unwrap_or_else(|_| c"sans-serif".to_owned());
        // SAFETY: Constructors return owned references, and native setters copy font options and
        // family names. The Rc retains both the map and the symbol table for the context's lifetime.
        let (context, description, metrics) = unsafe {
            let context = (api.pango_font_map_create_context)(self.0.handle.as_ptr());
            let options = (api.cairo_font_options_create)();
            (api.cairo_font_options_set_antialias)(
                options,
                if smoothing {
                    CAIRO_ANTIALIAS_GRAY
                } else {
                    CAIRO_ANTIALIAS_NONE
                },
            );
            (api.cairo_font_options_set_hint_style)(options, CAIRO_HINT_STYLE_FULL);
            (api.cairo_font_options_set_hint_metrics)(options, CAIRO_HINT_METRICS_ON);
            (api.pango_cairo_context_set_font_options)(context, options);
            (api.cairo_font_options_destroy)(options);
            let description = (api.pango_font_description_new)();
            (api.pango_font_description_set_family)(description, family.as_ptr());
            (api.pango_font_description_set_weight)(description, i32::from(font.weight));
            (api.pango_font_description_set_absolute_size)(
                description,
                f64::from(ppem) * f64::from(PANGO_SCALE),
            );
            let metrics = (api.pango_context_get_metrics)(context, description, null_mut());
            let ascent = pixels((api.pango_font_metrics_get_ascent)(metrics));
            let descent = pixels((api.pango_font_metrics_get_descent)(metrics));
            (api.pango_font_metrics_unref)(metrics);
            (context, description, (ascent, descent))
        };
        Typeface {
            map: self.0.clone(),
            context,
            description,
            metrics,
            weight: font.weight,
        }
    }
}

/// A resolved font and its device-scale Pango context. Used only on its creating UI thread.
pub struct Typeface {
    map: Rc<FontMap>,
    context: Object,
    description: Object,
    metrics: (i32, i32),
    weight: u16,
}

impl Drop for Typeface {
    fn drop(&mut self) {
        let api = self.map.api;
        // SAFETY: These independent references belong to this Typeface and no Layout outlives it.
        unsafe {
            (api.pango_font_description_free)(self.description);
            (api.g_object_unref)(self.context);
        }
    }
}

impl Typeface {
    /// Returns the primary font's ascent and descent in device pixels.
    pub fn metrics(&self) -> (i32, i32) {
        self.metrics
    }

    /// Measures a shaped single line in device pixels with the requested weight.
    pub fn measure(&self, text: &str, bold: bool) -> i32 {
        self.layout(text, bold, false, None).size().0
    }

    /// Returns line byte ranges using the same shaping, Unicode line breaking and width as drawing.
    pub fn wrap(&self, text: &str, width: i32, bold: bool) -> Vec<Range<usize>> {
        if width <= 0 {
            return Vec::new();
        }
        self.layout(text, bold, false, Some(width)).lines()
    }

    /// Shapes UTF-8 into a layout. A `width` in device pixels enables paragraph wrapping.
    pub fn layout<'a>(
        &'a self,
        text: &'a str,
        bold: bool,
        italic: bool,
        width: Option<i32>,
    ) -> Layout<'a> {
        let api = self.map.api;
        let end = text.find('\0').unwrap_or(text.len()).min(i32::MAX as usize);
        let text = &text[..text.floor_char_boundary(end)];
        let weight = if bold {
            self.weight.max(700)
        } else {
            self.weight
        };
        // SAFETY: No callbacks occur; the description is copied into each layout. The input UTF-8
        // bytes are copied by pango_layout_set_text before returning.
        let handle = unsafe {
            let layout = (api.pango_layout_new)(self.context);
            (api.pango_font_description_set_weight)(self.description, i32::from(weight));
            (api.pango_font_description_set_style)(
                self.description,
                if italic { PANGO_STYLE_ITALIC } else { 0 },
            );
            (api.pango_layout_set_font_description)(layout, self.description);
            (api.pango_layout_set_single_paragraph_mode)(layout, i32::from(width.is_none()));
            if let Some(width) = width {
                (api.pango_layout_set_width)(layout, width.max(0).saturating_mul(PANGO_SCALE));
                (api.pango_layout_set_wrap)(layout, PANGO_WRAP_WORD_CHAR);
            }
            (api.pango_layout_set_text)(layout, text.as_ptr().cast(), text.len() as i32);
            layout
        };
        Layout { api, handle, text }
    }
}

/// A shaped UTF-8 layout with byte-to-visual-position mapping and drawing.
pub struct Layout<'a> {
    api: &'a Api,
    handle: Object,
    text: &'a str,
}

impl Drop for Layout<'_> {
    fn drop(&mut self) {
        // SAFETY: Releases exactly the reference returned by pango_layout_new.
        unsafe { (self.api.g_object_unref)(self.handle) };
    }
}

/// A borrowed CPU-backed premultiplied ARGB frame and its device-pixel clip.
pub struct RenderTarget<'a> {
    pub pixels: &'a mut [u32],
    pub width: i32,
    pub height: i32,
    pub stride: usize,
    pub clip: (i32, i32, i32, i32),
}

impl Layout<'_> {
    /// Returns logical layout extent in device pixels.
    pub fn size(&self) -> (i32, i32) {
        let (mut width, mut height) = (0, 0);
        // SAFETY: Two distinct out parameters are writable for the lifetime of the native call.
        unsafe { (self.api.pango_layout_get_pixel_size)(self.handle, &mut width, &mut height) };
        (width, height)
    }

    /// Returns the ink bounds (x, y, width, height) in device pixels.
    pub fn ink_bounds(&self) -> (i32, i32, i32, i32) {
        let mut ink = Rectangle::default();
        // SAFETY: The native layout is live and ink is an exclusive writable output.
        unsafe { (self.api.pango_layout_get_pixel_extents)(self.handle, &mut ink, null_mut()) };
        (ink.x, ink.y, ink.width, ink.height)
    }

    /// Returns the number of characters for which the font stack used a missing-glyph box.
    pub fn missing_glyphs(&self) -> i32 {
        // SAFETY: The layout stays live for this synchronous query.
        unsafe { (self.api.pango_layout_get_unknown_glyphs_count)(self.handle) }
    }

    /// Returns the first line's baseline measured from the top of the layout.
    pub fn baseline(&self) -> i32 {
        // SAFETY: The live layout is retained by self.
        pixels(unsafe { (self.api.pango_layout_get_baseline)(self.handle) })
    }

    /// Returns true UTF-8 line ranges after wrapping, excluding paragraph separators.
    pub fn lines(&self) -> Vec<Range<usize>> {
        // SAFETY: Read-only line objects remain valid until this layout is destroyed.
        unsafe {
            (0..(self.api.pango_layout_get_line_count)(self.handle))
                .filter_map(|index| {
                    let line = (self.api.pango_layout_get_line_readonly)(self.handle, index);
                    let start = (self.api.pango_layout_line_get_start_index)(line) as usize;
                    let end = start + (self.api.pango_layout_line_get_length)(line) as usize;
                    self.text.get(start..end).map(|_| start..end)
                })
                .collect()
        }
    }

    /// Returns the strong caret's horizontal position for a UTF-8 byte boundary.
    pub fn caret_x(&self, index: usize) -> i32 {
        let mut strong = Rectangle::default();
        let index = self.text.floor_char_boundary(index.min(self.text.len()));
        // SAFETY: The index lies at a character boundary and the stack rectangle is writable.
        unsafe {
            (self.api.pango_layout_get_cursor_pos)(
                self.handle,
                index as i32,
                &mut strong,
                null_mut(),
            )
        };
        pixels(strong.x)
    }

    /// Maps a device-pixel point to the nearest UTF-8 cursor boundary.
    pub fn hit(&self, x: i32, y: i32) -> usize {
        let (mut index, mut trailing) = (0, 0);
        // SAFETY: Both out parameters are distinct; Pango bounds its result to this layout.
        unsafe {
            (self.api.pango_layout_xy_to_index)(
                self.handle,
                x.saturating_mul(PANGO_SCALE),
                y.saturating_mul(PANGO_SCALE),
                &mut index,
                &mut trailing,
            );
        }
        let index = self
            .text
            .floor_char_boundary((index.max(0) as usize).min(self.text.len()));
        self.text[index..]
            .char_indices()
            .nth(trailing.max(0) as usize)
            .map_or(self.text.len(), |(offset, _)| index + offset)
    }

    /// Returns the visual horizontal ranges for a logical byte range on the first line.
    /// Bidirectional selections may have more than one disjoint range.
    pub fn selection(&self, range: Range<usize>) -> Vec<Range<i32>> {
        let start = self
            .text
            .floor_char_boundary(range.start.min(self.text.len()));
        let end = self
            .text
            .floor_char_boundary(range.end.min(self.text.len()));
        if start >= end {
            return Vec::new();
        }
        // SAFETY: Pango allocates count pairs of integers; the returned array is copied before
        // freeing it with GLib's allocator. The layout and its borrowed first line stay alive.
        unsafe {
            let line = (self.api.pango_layout_get_line_readonly)(self.handle, 0);
            let (mut ranges, mut count) = (null_mut(), 0);
            (self.api.pango_layout_line_get_x_ranges)(
                line,
                start as i32,
                end as i32,
                &mut ranges,
                &mut count,
            );
            if ranges.is_null() {
                return Vec::new();
            }
            let result = std::slice::from_raw_parts(ranges, count as usize * 2)
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| pixels(pair[0])..pixels(pair[1]))
                .collect();
            (self.api.g_free)(ranges.cast());
            result
        }
    }

    /// Composites the shaped text to the clipped ARGB target using the requested RGB ink.
    /// Invalid target geometry is ignored; the pixel borrow is released before returning.
    pub fn draw(&self, target: RenderTarget<'_>, x: i32, y: i32, rgb: u32) {
        let RenderTarget {
            pixels,
            width,
            height,
            stride,
            clip: (cx, cy, cw, ch),
        } = target;
        if self.text.is_empty()
            || width <= 0
            || height <= 0
            || cw <= 0
            || ch <= 0
            || stride < width as usize
            || stride > i32::MAX as usize / 4
            || stride
                .checked_mul(height as usize)
                .is_none_or(|length| length > pixels.len())
        {
            return;
        }
        let api = self.api;
        // SAFETY: Cairo borrows the exclusively owned native-endian ARGB frame within this
        // synchronous block. We release the context, flush writes, and destroy the surface before
        // returning to Rust. Native surface and clip both bound drawing to the validated frame.
        unsafe {
            let surface = (api.cairo_image_surface_create_for_data)(
                pixels.as_mut_ptr().cast(),
                CAIRO_FORMAT_ARGB32,
                width,
                height,
                (stride * 4) as i32,
            );
            let context = (api.cairo_create)(surface);
            (api.cairo_rectangle)(
                context,
                f64::from(cx),
                f64::from(cy),
                f64::from(cw),
                f64::from(ch),
            );
            (api.cairo_clip)(context);
            (api.cairo_move_to)(context, f64::from(x), f64::from(y));
            let channel = |shift| f64::from((rgb >> shift) as u8) / 255.0;
            (api.cairo_set_source_rgb)(context, channel(16), channel(8), channel(0));
            (api.pango_cairo_show_layout)(context, self.handle);
            (api.cairo_destroy)(context);
            (api.cairo_surface_flush)(surface);
            (api.cairo_surface_destroy)(surface);
        }
    }
}

fn pixels(units: i32) -> i32 {
    units
        .saturating_add(PANGO_SCALE / 2)
        .div_euclid(PANGO_SCALE)
}

#[cfg(test)]
mod tests;
