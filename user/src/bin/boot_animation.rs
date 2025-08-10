#![no_std]
#![no_main]

#[macro_use]
extern crate alloc;
#[macro_use]
extern crate user_lib;

use user_lib::*;
use alloc::string::{String, ToString};

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct GuiPoint {
    x: i32,
    y: i32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct GuiRect {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct GuiColor {
    r: u8,
    g: u8,
    b: u8,
    a: u8,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct GuiScreenInfo {
    width: u32,
    height: u32,
    bytes_per_pixel: u32,
    pitch: u32,
}

impl GuiColor {
    const fn new(r: u8, g: u8, b: u8) -> Self {
        GuiColor { r, g, b, a: 255 }
    }
    
    const fn new_rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        GuiColor { r, g, b, a }
    }
    
    // Colors from XP theme
    const BLACK: GuiColor = GuiColor::new(0, 0, 0);
    const WHITE: GuiColor = GuiColor::new(255, 255, 255);
    const RED: GuiColor = GuiColor::new(255, 0, 0);
    const GREEN: GuiColor = GuiColor::new(0, 255, 0);
    const BLUE: GuiColor = GuiColor::new(0, 0, 255);
    const YELLOW: GuiColor = GuiColor::new(255, 255, 0);
    const XP_BLUE: GuiColor = GuiColor::new(0, 78, 152);
    const XP_LIGHT_BLUE: GuiColor = GuiColor::new(49, 106, 197);
    const XP_GREEN: GuiColor = GuiColor::new(125, 162, 206);
    const DARK_GRAY: GuiColor = GuiColor::new(64, 64, 64);
    const LIGHT_GRAY: GuiColor = GuiColor::new(192, 192, 192);
}

const SYSCALL_GUI_CREATE_CONTEXT: usize = 300;
const SYSCALL_GUI_DESTROY_CONTEXT: usize = 301;
const SYSCALL_GUI_CLEAR_SCREEN: usize = 302;
const SYSCALL_GUI_DRAW_PIXEL: usize = 303;
const SYSCALL_GUI_DRAW_LINE: usize = 304;
const SYSCALL_GUI_DRAW_RECT: usize = 305;
const SYSCALL_GUI_FILL_RECT: usize = 306;
const SYSCALL_GUI_DRAW_CIRCLE: usize = 307;
const SYSCALL_GUI_FILL_CIRCLE: usize = 308;
const SYSCALL_GUI_DRAW_TEXT: usize = 309;
const SYSCALL_GUI_FLUSH: usize = 310;
const SYSCALL_GUI_GET_SCREEN_INFO: usize = 311;

fn gui_create_context() -> isize {
    syscall(SYSCALL_GUI_CREATE_CONTEXT, [0, 0, 0])
}

fn gui_clear_screen(color: u32) -> isize {
    syscall(SYSCALL_GUI_CLEAR_SCREEN, [color as usize, 0, 0])
}

fn gui_fill_rect(rect: GuiRect, color: GuiColor) -> isize {
    syscall(SYSCALL_GUI_FILL_RECT, [&rect as *const _ as usize, &color as *const _ as usize, 0])
}

fn gui_draw_rect(rect: GuiRect, color: GuiColor) -> isize {
    syscall(SYSCALL_GUI_DRAW_RECT, [&rect as *const _ as usize, &color as *const _ as usize, 0])
}

fn gui_draw_text(text: &str, pos: GuiPoint, color: GuiColor) -> isize {
    syscall(SYSCALL_GUI_DRAW_TEXT, [text.as_ptr() as usize, text.len(), &pos as *const _ as usize])
}

fn gui_flush() -> isize {
    syscall(SYSCALL_GUI_FLUSH, [0, 0, 0])
}

fn gui_get_screen_info() -> Option<GuiScreenInfo> {
    let mut info = GuiScreenInfo {
        width: 0,
        height: 0,
        bytes_per_pixel: 0,
        pitch: 0,
    };
    
    let result = syscall::syscall(SYSCALL_GUI_GET_SCREEN_INFO, [&mut info as *mut _ as usize, 0, 0]);
    if result == 0 {
        Some(info)
    } else {
        None
    }
}

fn sleep_ms(ms: usize) {
    // Simple busy wait - in a real system you'd use a proper sleep syscall
    for _ in 0..(ms * 10000) {
        unsafe { core::arch::asm!("nop") };
    }
}

fn draw_windows_logo(screen_info: &GuiScreenInfo, center_x: i32, center_y: i32, size: u32) {
    let half_size = (size / 2) as i32;
    let quarter_size = (size / 4) as i32;
    
    // Draw the Windows logo as 4 colored rectangles with gaps
    let gap = 4i32;
    
    // Top-left (red)
    let tl_rect = GuiRect {
        x: center_x - half_size,
        y: center_y - half_size,
        width: quarter_size as u32 - gap as u32,
        height: quarter_size as u32 - gap as u32,
    };
    gui_fill_rect(tl_rect, GuiColor::RED);
    
    // Top-right (green)
    let tr_rect = GuiRect {
        x: center_x + gap,
        y: center_y - half_size,
        width: quarter_size as u32 - gap as u32,
        height: quarter_size as u32 - gap as u32,
    };
    gui_fill_rect(tr_rect, GuiColor::GREEN);
    
    // Bottom-left (blue)
    let bl_rect = GuiRect {
        x: center_x - half_size,
        y: center_y + gap,
        width: quarter_size as u32 - gap as u32,
        height: quarter_size as u32 - gap as u32,
    };
    gui_fill_rect(bl_rect, GuiColor::BLUE);
    
    // Bottom-right (yellow)
    let br_rect = GuiRect {
        x: center_x + gap,
        y: center_y + gap,
        width: quarter_size as u32 - gap as u32,
        height: quarter_size as u32 - gap as u32,
    };
    gui_fill_rect(br_rect, GuiColor::YELLOW);
}

fn draw_progress_bar(rect: GuiRect, progress: f32, bg_color: GuiColor, fg_color: GuiColor, border_color: GuiColor) {
    // Draw border
    gui_draw_rect(rect, border_color);
    
    // Draw background
    let inner_rect = GuiRect {
        x: rect.x + 1,
        y: rect.y + 1,
        width: rect.width - 2,
        height: rect.height - 2,
    };
    gui_fill_rect(inner_rect, bg_color);
    
    // Draw progress
    if progress > 0.0 {
        let progress_width = ((inner_rect.width as f32) * progress.min(1.0)) as u32;
        if progress_width > 0 {
            let progress_rect = GuiRect {
                x: inner_rect.x,
                y: inner_rect.y,
                width: progress_width,
                height: inner_rect.height,
            };
            gui_fill_rect(progress_rect, fg_color);
        }
    }
}

fn draw_gradient_background(screen_info: &GuiScreenInfo) {
    // Draw a simple vertical gradient from dark blue to black
    let height_step = screen_info.height / 10;
    
    for i in 0..10 {
        let y = (i * height_step) as i32;
        let intensity = 255 - (i * 25);
        let color = GuiColor::new_rgba(0, intensity as u8 / 4, intensity as u8 / 2, 255);
        
        let rect = GuiRect {
            x: 0,
            y,
            width: screen_info.width,
            height: height_step,
        };
        gui_fill_rect(rect, color);
    }
}

#[unsafe(no_mangle)]
pub fn main() -> i32 {
    println!("Starting Windows XP Boot Animation...");
    
    // Initialize GUI context
    let context = gui_create_context();
    if context < 0 {
        println!("Failed to create GUI context");
        return -1;
    }
    
    // Get screen information
    let screen_info = match gui_get_screen_info() {
        Some(info) => {
            println!("Screen: {}x{}, {} bpp", info.width, info.height, info.bytes_per_pixel * 8);
            info
        }
        None => {
            println!("Failed to get screen info, using defaults");
            GuiScreenInfo {
                width: 1024,
                height: 768,
                bytes_per_pixel: 4,
                pitch: 1024 * 4,
            }
        }
    };
    
    let center_x = (screen_info.width / 2) as i32;
    let center_y = (screen_info.height / 2) as i32 - 50;
    
    // Logo and UI positioning
    let logo_size = 120u32;
    let progress_width = 300u32;
    let progress_height = 20u32;
    let progress_rect = GuiRect {
        x: center_x - (progress_width as i32) / 2,
        y: center_y + 100,
        width: progress_width,
        height: progress_height,
    };
    
    let title_pos = GuiPoint {
        x: center_x - 90,  // Approximate center for "Microsoft Windows XP"
        y: center_y - 100,
    };
    
    let loading_pos = GuiPoint {
        x: center_x - 40,  // Approximate center for "Loading"
        y: progress_rect.y + progress_height as i32 + 30,
    };
    
    println!("Starting animation sequence...");
    
    // Phase 1: Fade in with gradient background
    println!("Phase 1: Background fade-in");
    for frame in 0..30 {
        draw_gradient_background(&screen_info);
        gui_flush();
        sleep_ms(50);
    }
    
    // Phase 2: Show Windows logo
    println!("Phase 2: Show Windows logo");
    for frame in 0..60 {
        draw_gradient_background(&screen_info);
        draw_windows_logo(&screen_info, center_x, center_y, logo_size);
        gui_flush();
        sleep_ms(50);
    }
    
    // Phase 3: Show title text
    println!("Phase 3: Show title");
    for frame in 0..60 {
        draw_gradient_background(&screen_info);
        draw_windows_logo(&screen_info, center_x, center_y, logo_size);
        gui_draw_text("Microsoft Windows XP", title_pos, GuiColor::WHITE);
        gui_flush();
        sleep_ms(50);
    }
    
    // Phase 4: Loading animation with progress bar
    println!("Phase 4: Loading animation");
    let total_frames = 120;
    for frame in 0..total_frames {
        // Clear and redraw everything
        draw_gradient_background(&screen_info);
        draw_windows_logo(&screen_info, center_x, center_y, logo_size);
        gui_draw_text("Microsoft Windows XP", title_pos, GuiColor::WHITE);
        
        // Animate progress bar
        let progress = (frame as f32) / (total_frames as f32);
        draw_progress_bar(progress_rect, progress, GuiColor::DARK_GRAY, GuiColor::XP_BLUE, GuiColor::LIGHT_GRAY);
        
        // Animate loading text with dots
        let dot_count = (frame / 15) % 4;
        let mut loading_text = String::from("Loading");
        for _ in 0..dot_count {
            loading_text.push('.');
        }
        gui_draw_text(&loading_text, loading_pos, GuiColor::WHITE);
        
        gui_flush();
        sleep_ms(100);
    }
    
    // Phase 5: Completion
    println!("Phase 5: Animation complete");
    for frame in 0..30 {
        draw_gradient_background(&screen_info);
        draw_windows_logo(&screen_info, center_x, center_y, logo_size);
        gui_draw_text("Microsoft Windows XP", title_pos, GuiColor::WHITE);
        draw_progress_bar(progress_rect, 1.0, GuiColor::DARK_GRAY, GuiColor::XP_BLUE, GuiColor::LIGHT_GRAY);
        gui_draw_text("Ready!", loading_pos, GuiColor::GREEN);
        gui_flush();
        sleep_ms(100);
    }
    
    println!("Windows XP Boot Animation Complete!");
    println!("Press Ctrl+C to exit");
    
    // Keep the final frame displayed
    loop {
        sleep_ms(1000);
    }
}

