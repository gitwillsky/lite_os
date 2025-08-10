#![allow(dead_code)]

use crate::syscall::syscall;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, Ordering};

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct GuiScreenInfo { pub width: u32, pub height: u32, pub bytes_per_pixel: u32, pub pitch: u32 }

struct GlobalGfx {
    info: GuiScreenInfo,
    backbuffer: Vec<u8>, // RGBA8888
}

struct SpinLock<T> {
    locked: AtomicBool,
    value: UnsafeCell<T>,
}

unsafe impl<T: Send> Sync for SpinLock<T> {}

impl<T> SpinLock<T> {
    const fn new(value: T) -> Self {
        Self { locked: AtomicBool::new(false), value: UnsafeCell::new(value) }
    }

    fn lock(&self) -> SpinLockGuard<'_, T> {
        while self
            .locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        SpinLockGuard { lock: self }
    }
}

struct SpinLockGuard<'a, T> {
    lock: &'a SpinLock<T>,
}

impl<'a, T> core::ops::Deref for SpinLockGuard<'a, T> {
    type Target = T;
    fn deref(&self) -> &Self::Target { unsafe { &*self.lock.value.get() } }
}

impl<'a, T> core::ops::DerefMut for SpinLockGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target { unsafe { &mut *self.lock.value.get() } }
}

impl<'a, T> Drop for SpinLockGuard<'a, T> {
    fn drop(&mut self) { self.lock.locked.store(false, Ordering::Release); }
}

static GFX: SpinLock<Option<GlobalGfx>> = SpinLock::new(None);

#[inline(always)]
pub fn gui_create_context() -> bool {
    let ok = syscall(300, [0,0,0]) >= 0;
    if !ok { return false; }
    let mut info = GuiScreenInfo::default();
    let p = &mut info as *mut GuiScreenInfo as usize;
    if syscall(311, [p, 0, 0]) < 0 { return false; }
    let size = (info.width as usize) * (info.height as usize) * 4usize;
    let mut guard = GFX.lock();
    *guard = Some(GlobalGfx { info, backbuffer: vec![0u8; size] });
    true
}

#[inline(always)]
pub fn screen_size() -> (u32, u32) {
    let guard = GFX.lock();
    if let Some(ref g) = *guard { (g.info.width, g.info.height) } else { (0, 0) }
}

#[inline(always)]
pub fn gui_clear(color: u32) {
    let mut guard = GFX.lock();
    if let Some(ref mut g) = *guard {
        let r = ((color >> 16) & 0xFF) as u8;
        let gch = ((color >> 8) & 0xFF) as u8;
        let b = (color & 0xFF) as u8;
        let a = ((color >> 24) & 0xFF) as u8;
        for px in g.backbuffer.chunks_exact_mut(4) {
            px[0] = r; px[1] = gch; px[2] = b; px[3] = a;
        }
    }
}

#[inline(always)]
pub fn gui_fill_rect_xywh(x: i32, y: i32, w: u32, h: u32, color: u32) {
    let mut guard = GFX.lock();
    if let Some(ref mut g) = *guard {
        let (sw, sh) = (g.info.width as i32, g.info.height as i32);
        let r = ((color >> 16) & 0xFF) as u8;
        let gch = ((color >> 8) & 0xFF) as u8;
        let b = (color & 0xFF) as u8;
        let a = ((color >> 24) & 0xFF) as u8;
        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x + w as i32).min(sw);
        let y1 = (y + h as i32).min(sh);
        if x0 >= x1 || y0 >= y1 { return; }
        for yy in y0..y1 {
            let row_off = (yy as usize) * (sw as usize) * 4;
            for xx in x0..x1 {
                let idx = row_off + (xx as usize) * 4;
                g.backbuffer[idx] = r; g.backbuffer[idx+1] = gch; g.backbuffer[idx+2] = b; g.backbuffer[idx+3] = a;
            }
        }
    }
}

#[inline(always)]
pub fn gui_flush() {
    let mut guard = GFX.lock();
    if let Some(ref mut g) = *guard {
        let _ = syscall(312, [g.backbuffer.as_ptr() as usize, g.backbuffer.len(), 0]); // present
    }
    let _ = syscall(310, [0,0,0]); // flush to device
}

pub const FONT_WIDTH: u32 = 8;
pub const FONT_HEIGHT: u32 = 16;

// 基础 8x16 位图字体（与内核保持一致，截取 ASCII 0x20..0x7F 部分）
#[rustfmt::skip]
pub static BASIC_FONT: [[u8; 16]; 128] = {
    let mut f = [[0u8;16];128];
    // 只填充 0x20..0x7F；其余保持 0
    f[0x20] = [0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]; // space
    f[0x21] = [0x00,0x18,0x18,0x18,0x18,0x18,0x18,0x00,0x18,0x18,0,0,0,0,0,0]; // !
    f[0x2E] = [0x00,0,0,0,0,0,0,0,0x00,0x18,0x18,0,0,0,0,0]; // .
    // 数字 0-9 (0x30..0x39)
    f[0x30] = [0x00,0x3c,0x66,0x6e,0x76,0x66,0x3c,0x00,0,0,0,0,0,0,0,0];
    f[0x31] = [0x00,0x18,0x38,0x18,0x18,0x18,0x7e,0x00,0,0,0,0,0,0,0,0];
    f[0x32] = [0x00,0x3c,0x66,0x0c,0x18,0x30,0x7e,0x00,0,0,0,0,0,0,0,0];
    f[0x33] = [0x00,0x7e,0x0c,0x18,0x0c,0x66,0x3c,0x00,0,0,0,0,0,0,0,0];
    f[0x34] = [0x00,0x0c,0x1c,0x3c,0x6c,0x7e,0x0c,0x00,0,0,0,0,0,0,0,0];
    f[0x35] = [0x00,0x7e,0x60,0x7c,0x06,0x66,0x3c,0x00,0,0,0,0,0,0,0,0];
    f[0x36] = [0x00,0x3c,0x60,0x7c,0x66,0x66,0x3c,0x00,0,0,0,0,0,0,0,0];
    f[0x37] = [0x00,0x7e,0x06,0x0c,0x18,0x30,0x30,0x00,0,0,0,0,0,0,0,0];
    f[0x38] = [0x00,0x3c,0x66,0x3c,0x66,0x66,0x3c,0x00,0,0,0,0,0,0,0,0];
    f[0x39] = [0x00,0x3c,0x66,0x3e,0x06,0x0c,0x38,0x00,0,0,0,0,0,0,0,0];
    // 大写 A..Z (0x41..0x5A) - 选取部分足够用于标题
    f[0x41] = [0x00,0x18,0x3c,0x66,0x66,0x7e,0x66,0x00,0,0,0,0,0,0,0,0];
    f[0x44] = [0x00,0x7c,0x66,0x66,0x66,0x66,0x7c,0x00,0,0,0,0,0,0,0,0]; // D
    f[0x47] = [0x00,0x3c,0x66,0x60,0x6e,0x66,0x3c,0x00,0,0,0,0,0,0,0,0]; // G
    f[0x49] = [0x00,0x7e,0x18,0x18,0x18,0x18,0x7e,0x00,0,0,0,0,0,0,0,0]; // I
    f[0x4C] = [0x00,0x60,0x60,0x60,0x60,0x60,0x7e,0x00,0,0,0,0,0,0,0,0]; // L
    f[0x4E] = [0x00,0x66,0x76,0x7e,0x6e,0x66,0x66,0x00,0,0,0,0,0,0,0,0]; // N
    f[0x4F] = [0x00,0x3c,0x66,0x66,0x66,0x66,0x3c,0x00,0,0,0,0,0,0,0,0]; // O
    f[0x50] = [0x00,0x7c,0x66,0x7c,0x60,0x60,0x60,0x00,0,0,0,0,0,0,0,0]; // P
    f[0x52] = [0x00,0x7c,0x66,0x7c,0x6c,0x66,0x66,0x00,0,0,0,0,0,0,0,0]; // R
    f[0x53] = [0x00,0x3c,0x60,0x3c,0x06,0x06,0x3c,0x00,0,0,0,0,0,0,0,0]; // S
    f[0x54] = [0x00,0x7e,0x18,0x18,0x18,0x18,0x18,0x00,0,0,0,0,0,0,0,0]; // T
    f[0x55] = [0x00,0x66,0x66,0x66,0x66,0x66,0x3c,0x00,0,0,0,0,0,0,0,0]; // U
    f[0x57] = [0x00,0x63,0x6b,0x7f,0x77,0x63,0x63,0x00,0,0,0,0,0,0,0,0]; // W
    f[0x58] = [0x00,0x66,0x3c,0x18,0x3c,0x66,0x66,0x00,0,0,0,0,0,0,0,0]; // X
    f[0x59] = [0x00,0x66,0x66,0x3c,0x18,0x18,0x18,0x00,0,0,0,0,0,0,0,0]; // Y
    // 小写 a..z (0x61..0x7A) - 选取 Loading/Kernel/Shell 常用字母
    f[0x61] = [0x00,0x00,0x3c,0x06,0x3e,0x66,0x3e,0x00,0,0,0,0,0,0,0,0]; // a
    f[0x62] = [0x00,0x60,0x7c,0x66,0x66,0x66,0x7c,0x00,0,0,0,0,0,0,0,0]; // b
    f[0x64] = [0x00,0x06,0x3e,0x66,0x66,0x66,0x3e,0x00,0,0,0,0,0,0,0,0]; // d
    f[0x65] = [0x00,0x00,0x3c,0x66,0x7e,0x60,0x3c,0x00,0,0,0,0,0,0,0,0]; // e
    f[0x67] = [0x00,0x00,0x3e,0x66,0x66,0x3e,0x06,0x3c,0,0,0,0,0,0,0,0]; // g
    f[0x69] = [0x00,0x18,0x00,0x38,0x18,0x18,0x3c,0x00,0,0,0,0,0,0,0,0]; // i
    f[0x6C] = [0x00,0x38,0x18,0x18,0x18,0x18,0x3c,0x00,0,0,0,0,0,0,0,0]; // l
    f[0x6E] = [0x00,0x00,0x7c,0x66,0x66,0x66,0x66,0x00,0,0,0,0,0,0,0,0]; // n
    f[0x6F] = [0x00,0x00,0x3c,0x66,0x66,0x66,0x3c,0x00,0,0,0,0,0,0,0,0]; // o
    f[0x72] = [0x00,0x00,0x6c,0x76,0x60,0x60,0x60,0x00,0,0,0,0,0,0,0,0]; // r
    f[0x73] = [0x00,0x00,0x3e,0x60,0x3c,0x06,0x7c,0x00,0,0,0,0,0,0,0,0]; // s
    f[0x74] = [0x00,0x30,0x7c,0x30,0x30,0x36,0x1c,0x00,0,0,0,0,0,0,0,0]; // t
    f[0x75] = [0x00,0x00,0x66,0x66,0x66,0x66,0x3e,0x00,0,0,0,0,0,0,0,0]; // u
    f
};

#[inline]
fn draw_char_scaled(ch: u8, x: i32, y: i32, color: u32, scale: u32) {
    let bitmap = BASIC_FONT[ch as usize];
    for row in 0..FONT_HEIGHT {
        let bits = bitmap[row as usize];
        for col in 0..FONT_WIDTH {
            if (bits & (0x80 >> col)) != 0 {
                let px = x + (col as i32) * (scale as i32);
                let py = y + (row as i32) * (scale as i32);
                gui_fill_rect_xywh(px, py, scale, scale, color);
            }
        }
    }
}

pub fn draw_string_scaled(mut x: i32, y: i32, text: &str, color: u32, scale: u32) {
    let s = if scale > 1 { scale } else { 1 };
    for &b in text.as_bytes() {
        draw_char_scaled(b, x, y, color, s);
        x += (FONT_WIDTH * s) as i32;
    }
}

