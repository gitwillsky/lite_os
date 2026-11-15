//! Pango, Fontconfig and Cairo C ABI. Native objects remain inside typeface.
use linux_uapi::dynamic_library::Library;
use std::{
    ffi::{c_char, c_void},
    io,
};

pub type Object = *mut c_void;

#[derive(Clone, Copy, Default)]
#[repr(C)]
pub struct Rectangle {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

macro_rules! bindings {
    ($( $name:ident ( $( $arg:ty ),* ) $( -> $ret:ty )? $( = $symbol:literal )?; )*) => {
        pub struct Api {
            $(pub $name: unsafe extern "C" fn($($arg),*) $(-> $ret)?,)*
            _library: Library,
        }

        impl Api {
            pub fn load() -> io::Result<Self> {
                let library = open()?;
                Ok(Self {
                    // SAFETY: Each spelling and C signature below matches the pinned native ABI.
                    $($name: unsafe { library.symbol(bindings!(@symbol $name $($symbol)?)) }?,)*
                    _library: library,
                })
            }
        }
    };
    (@symbol $name:ident) => { stringify!($name) };
    (@symbol $name:ident $symbol:literal) => { $symbol };
}

bindings! {
    g_object_unref(Object);
    g_free(Object);
    fc_init_load_config_and_fonts() -> Object = "FcInitLoadConfigAndFonts";
    fc_config_app_font_add_dir(Object, *const u8) -> i32 = "FcConfigAppFontAddDir";
    fc_config_destroy(Object) = "FcConfigDestroy";
    pango_cairo_font_map_new_for_font_type(i32) -> Object;
    pango_fc_font_map_set_config(Object, Object);
    pango_font_map_create_context(Object) -> Object;
    pango_cairo_context_set_font_options(Object, Object);
    pango_font_description_new() -> Object;
    pango_font_description_free(Object);
    pango_font_description_set_family(Object, *const c_char);
    pango_font_description_set_weight(Object, i32);
    pango_font_description_set_style(Object, i32);
    pango_font_description_set_absolute_size(Object, f64);
    pango_context_get_metrics(Object, Object, Object) -> Object;
    pango_font_metrics_get_ascent(Object) -> i32;
    pango_font_metrics_get_descent(Object) -> i32;
    pango_font_metrics_unref(Object);
    pango_layout_new(Object) -> Object;
    pango_layout_set_font_description(Object, Object);
    pango_layout_set_text(Object, *const c_char, i32);
    pango_layout_set_single_paragraph_mode(Object, i32);
    pango_layout_set_width(Object, i32);
    pango_layout_set_wrap(Object, i32);
    pango_layout_get_pixel_size(Object, *mut i32, *mut i32);
    pango_layout_get_pixel_extents(Object, *mut Rectangle, *mut Rectangle);
    pango_layout_get_unknown_glyphs_count(Object) -> i32;
    pango_layout_get_baseline(Object) -> i32;
    pango_layout_get_line_count(Object) -> i32;
    pango_layout_get_line_readonly(Object, i32) -> Object;
    pango_layout_line_get_start_index(Object) -> i32;
    pango_layout_line_get_length(Object) -> i32;
    pango_layout_get_cursor_pos(Object, i32, *mut Rectangle, *mut Rectangle);
    pango_layout_xy_to_index(Object, i32, i32, *mut i32, *mut i32) -> i32;
    pango_layout_line_get_x_ranges(Object, i32, i32, *mut *mut i32, *mut i32);
    pango_cairo_show_layout(Object, Object);
    cairo_font_options_create() -> Object;
    cairo_font_options_set_antialias(Object, i32);
    cairo_font_options_set_hint_style(Object, i32);
    cairo_font_options_set_hint_metrics(Object, i32);
    cairo_font_options_destroy(Object);
    cairo_image_surface_create_for_data(*mut u8, i32, i32, i32, i32) -> Object;
    cairo_surface_destroy(Object);
    cairo_surface_flush(Object);
    cairo_create(Object) -> Object;
    cairo_destroy(Object);
    cairo_rectangle(Object, f64, f64, f64, f64);
    cairo_clip(Object);
    cairo_move_to(Object, f64, f64);
    cairo_set_source_rgb(Object, f64, f64, f64);
}

#[cfg(not(target_os = "macos"))]
fn open() -> io::Result<Library> {
    Library::open("libpangocairo-1.0.so.0")
}

#[cfg(target_os = "macos")]
fn open() -> io::Result<Library> {
    Library::open("libpangocairo-1.0.0.dylib")
        .or_else(|_| Library::open("/opt/homebrew/lib/libpangocairo-1.0.0.dylib"))
}
