#![allow(dead_code)]

use crate::syscall::syscall;
use alloc::vec::Vec;
// use alloc::string::String;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, Ordering};

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct GuiScreenInfo { pub width: u32, pub height: u32, pub bytes_per_pixel: u32, pub pitch: u32 }

struct GlobalGfx {
    info: GuiScreenInfo,
    fb_ptr: usize, // 直接映射的帧缓冲地址（以整数保存，避免线程安全限制）
    default_font: Option<&'static [u8]>,
    dirty: bool,
    dirty_rect: Option<(i32, i32, i32, i32)>,
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
// 几何与轮廓收集
#[derive(Clone, Copy)]
struct LineSeg { x0: f32, y0: f32, x1: f32, y1: f32 }

struct OutlineCollector {
    edges: Vec<LineSeg>,
    sx: f32, sy: f32,   // scale
    ox: f32, oy: f32,   // offset
    last_x: f32,
    last_y: f32,
    start_x: f32,
    start_y: f32,
}

impl OutlineCollector {
    fn new(scale: f32, ox: f32, oy: f32) -> Self {
        Self { edges: Vec::new(), sx: scale, sy: scale, ox, oy, last_x: 0.0, last_y: 0.0, start_x: 0.0, start_y: 0.0 }
    }
}

impl ttf_parser::OutlineBuilder for OutlineCollector {
    fn move_to(&mut self, _x: f32, _y: f32) {
        // TTF 坐标系 y 轴向上，屏幕 y 轴向下，因此需要对 y 取反
        self.last_x = _x * self.sx + self.ox; self.last_y = self.oy - _y * self.sy;
        self.start_x = self.last_x; self.start_y = self.last_y;
    }
    fn line_to(&mut self, _x: f32, _y: f32) {
        let x = _x * self.sx + self.ox; let y = self.oy - _y * self.sy;
        self.edges.push(LineSeg { x0: self.last_x, y0: self.last_y, x1: x, y1: y });
        self.last_x = x; self.last_y = y;
    }
    fn quad_to(&mut self, _x1: f32, _y1: f32, _x: f32, _y: f32) {
        // 二次贝塞尔折线化（固定细分 N）
        let n = 16;
        let mut px = self.last_x; let mut py = self.last_y;
        let x1 = _x1 * self.sx + self.ox; let y1 = self.oy - _y1 * self.sy;
        let x2 = _x * self.sx + self.ox; let y2 = self.oy - _y * self.sy;
        for i in 1..=n {
            let t = i as f32 / n as f32;
            let it = 1.0 - t;
            let x = it*it* self.last_x + 2.0*it*t*x1 + t*t*x2;
            let y = it*it* self.last_y + 2.0*it*t*y1 + t*t*y2;
            self.edges.push(LineSeg { x0: px, y0: py, x1: x, y1: y });
            px = x; py = y;
        }
        self.last_x = x2; self.last_y = y2;
    }
    fn curve_to(&mut self, _x1: f32, _y1: f32, _x2: f32, _y2: f32, _x: f32, _y: f32) {
        // 三次贝塞尔折线化（固定细分 N）
        let n = 22;
        let mut px = self.last_x; let mut py = self.last_y;
        let x1 = _x1 * self.sx + self.ox; let y1 = self.oy - _y1 * self.sy;
        let x2 = _x2 * self.sx + self.ox; let y2 = self.oy - _y2 * self.sy;
        let x3 = _x * self.sx + self.ox; let y3 = self.oy - _y * self.sy;
        for i in 1..=n {
            let t = i as f32 / n as f32; let it = 1.0 - t;
            let x = it*it*it*self.last_x + 3.0*it*it*t*x1 + 3.0*it*t*t*x2 + t*t*t*x3;
            let y = it*it*it*self.last_y + 3.0*it*it*t*y1 + 3.0*it*t*t*y2 + t*t*t*y3;
            self.edges.push(LineSeg { x0: px, y0: py, x1: x, y1: y });
            px = x; py = y;
        }
        self.last_x = x3; self.last_y = y3;
    }
    fn close(&mut self) {
        self.edges.push(LineSeg { x0: self.last_x, y0: self.last_y, x1: self.start_x, y1: self.start_y });
    }
}

//

fn rasterize_edges(edges: &Vec<LineSeg>, color: u32) {
    // 简易扫描线填充（偶奇规则），按整数像素栅格；无抗锯齿
    let mut guard = GFX.lock();
    let Some(ref mut g) = *guard else { return };
    let (sw, sh) = (g.info.width as i32, g.info.height as i32);

    // 计算字形包围盒，减少扫描范围
    let mut min_y = i32::MAX; let mut max_y = i32::MIN;
    for e in edges.iter() {
        let y0f = libm::floorf(e.y0) as i32;
        let y1f = libm::floorf(e.y1) as i32;
        let y0c = libm::ceilf(e.y0) as i32;
        let y1c = libm::ceilf(e.y1) as i32;
        min_y = min_y.min(y0f).min(y1f);
        max_y = max_y.max(y0c).max(y1c);
    }
    min_y = min_y.max(0); max_y = max_y.min(sh - 1);

    let r = ((color >> 16) & 0xFF) as u8;
    let gch = ((color >> 8) & 0xFF) as u8;
    let b = (color & 0xFF) as u8;
    let a = ((color >> 24) & 0xFF) as u8;

    for y in min_y..=max_y {
        // 收集与扫描线相交的 x 交点
        let y_f = y as f32 + 0.5;
        let mut xs: [f32; 1024] = [0.0; 1024];
        let mut cnt = 0usize;
        for e in edges.iter() {
            // 跳过水平边
            if libm::fabsf(e.y0 - e.y1) < 1e-6 { continue; }
            let (ymin, ymax, x0, y0, x1, y1) = if e.y0 < e.y1 { (e.y0, e.y1, e.x0, e.y0, e.x1, e.y1) } else { (e.y1, e.y0, e.x1, e.y1, e.x0, e.y0) };
            if y_f < ymin || y_f >= ymax { continue; }
            let t = (y_f - y0) / (y1 - y0);
            let x = x0 + t * (x1 - x0);
            if cnt < xs.len() { xs[cnt] = x; cnt += 1; }
        }

        // 排序交点
        for i in 0..cnt { let mut j = i; while j > 0 && xs[j-1] > xs[j] { let tmp = xs[j-1]; xs[j-1] = xs[j]; xs[j] = tmp; j -= 1; } }

        // 成对填充区间
        let mut i = 0;
        while i + 1 < cnt {
            let x0 = libm::ceilf(xs[i]) as i32; // 右取整，避免填充到边界外
            let x1 = libm::floorf(xs[i+1]) as i32; // 左取整
            if x1 < x0 { i += 2; continue; }
            let x0c = x0.max(0); let x1c = x1.min(sw - 1);
            if x0c <= x1c {
                let pitch = g.info.pitch as usize;
                let row_ptr = (g.fb_ptr + (y as usize) * pitch) as *mut u8;
                for x in x0c..=x1c {
                    let p = unsafe { row_ptr.add((x as usize) * 4) };
                    unsafe { *p.add(0) = r; *p.add(1) = gch; *p.add(2) = b; *p.add(3) = a; }
                }
            }
            i += 2;
        }
    }
    // 标记脏并合并脏矩形
    g.dirty = true;
    // 简化：计算该字形的包围盒作为脏矩形
    let mut min_x = i32::MAX; let mut max_x = i32::MIN;
    for e in edges.iter() {
        let x0f = libm::floorf(e.x0) as i32; let x1f = libm::floorf(e.x1) as i32;
        let x0c = libm::ceilf(e.x0) as i32; let x1c = libm::ceilf(e.x1) as i32;
        min_x = min_x.min(x0f).min(x1f); max_x = max_x.max(x0c).max(x1c);
    }
    min_x = min_x.max(0); max_x = max_x.min(sw - 1);
    let rect = (min_x, min_y, max_x + 1, max_y + 1);
    g.dirty_rect = Some(match g.dirty_rect {
        Some((ox0, oy0, ox1, oy1)) => (ox0.min(rect.0), oy0.min(rect.1), ox1.max(rect.2), oy1.max(rect.3)),
        None => rect,
    });
}

// TTF 渲染：占位实现，后续将以 ttf-parser + 简易栅格化替换
pub fn draw_text_ttf(_x: i32, _y: i32, _text: &str, _size_px: u32, _color: u32, _font_bytes: &'static [u8]) -> bool {
    // 实现：使用 ttf-parser 解析字形轮廓，折线化后扫描线填充
    let face = match ttf_parser::Face::parse(_font_bytes, 0) { Ok(f) => f, Err(_) => return false };
    let upem = face.units_per_em() as f32;
    if upem <= 0.0 { return false; }
    let scale = _size_px as f32 / upem;

    let mut pen_x = _x;
    // 将基线设置为屏幕坐标，注意 TTF y 向上，OutlineCollector 内部已做翻转
    let baseline_y = _y;

    for ch in _text.chars() {
        let gid = match face.glyph_index(ch) { Some(id) => id, None => ttf_parser::GlyphId(0) };
        // 采集并栅格化字形
        let mut collector = OutlineCollector::new(scale, pen_x as f32, baseline_y as f32);
        let _ = face.outline_glyph(gid, &mut collector);
        rasterize_edges(&collector.edges, _color);

        // 前进光标
        // 计算水平方向 advance
        let adv_units = face.glyph_hor_advance(gid).unwrap_or(0) as f32;
        let adv = adv_units * scale;
        pen_x += adv as i32;
    }
    true
}

#[inline(always)]
pub fn gui_create_context() -> bool {
    let ok = syscall(300, [0,0,0]) >= 0;
    if !ok { return false; }
    let mut info = GuiScreenInfo::default();
    let p = &mut info as *mut GuiScreenInfo as usize;
    if syscall(311, [p, 0, 0]) < 0 { return false; }
    let mut mapped_addr: usize = 0;
    let out_ptr = &mut mapped_addr as *mut usize as usize;
    if syscall(315, [out_ptr, 0, 0]) < 0 { return false; }
    let mut guard = GFX.lock();
    *guard = Some(GlobalGfx { info, fb_ptr: mapped_addr, default_font: None, dirty: false, dirty_rect: None });
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
        let pitch = g.info.pitch as usize;
        let width = g.info.width as usize;
        let height = g.info.height as usize;
        for y in 0..height {
            let row_ptr = (g.fb_ptr + y * pitch) as *mut u8;
            for x in 0..width {
                let p = (row_ptr as usize + x * 4) as *mut u8;
                unsafe { *p.add(0) = r; *p.add(1) = gch; *p.add(2) = b; *p.add(3) = a; }
            }
        }
        g.dirty = true;
        g.dirty_rect = Some((0, 0, g.info.width as i32, g.info.height as i32));
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
        let pitch = g.info.pitch as usize;
        for yy in y0..y1 {
            let row_ptr = (g.fb_ptr + (yy as usize) * pitch) as *mut u8;
            for xx in x0..x1 {
                let p = (row_ptr as usize + (xx as usize) * 4) as *mut u8;
                unsafe { *p.add(0) = r; *p.add(1) = gch; *p.add(2) = b; *p.add(3) = a; }
            }
        }
        g.dirty = true;
        g.dirty_rect = Some(match g.dirty_rect {
            Some((ox0, oy0, ox1, oy1)) => (ox0.min(x0), oy0.min(y0), ox1.max(x1), oy1.max(y1)),
            None => (x0, y0, x1, y1),
        });
    }
}

#[inline(always)]
pub fn gui_flush() {
    let mut guard = GFX.lock();
    if let Some(ref mut g) = *guard {
        if !g.dirty { return; }
        if let Some((x0, y0, x1, y1)) = g.dirty_rect {
            #[repr(C)]
            struct Rect { x: u32, y: u32, width: u32, height: u32 }
            let rect = Rect { x: x0 as u32, y: y0 as u32, width: (x1 - x0) as u32, height: (y1 - y0) as u32 };
            let _ = syscall(313, [&rect as *const Rect as usize, 1, 0]); // 仅刷新矩形
        }
        g.dirty = false;
        g.dirty_rect = None;
    }
}

// ============ 默认字体管理 ============

pub fn set_default_font(font_bytes: &'static [u8]) {
    let mut guard = GFX.lock();
    if let Some(ref mut g) = *guard { g.default_font = Some(font_bytes); }
}

pub fn draw_text(x: i32, y: i32, text: &str, size_px: u32, color: u32) -> bool {
    // 读取默认字体后立刻释放锁，避免与栅格化中的回缓冲写入锁嵌套
    let font_opt: Option<&'static [u8]> = {
        let guard = GFX.lock();
        if let Some(ref g) = *guard { g.default_font } else { None }
    };
    if let Some(bytes) = font_opt { draw_text_ttf(x, y, text, size_px, color, bytes) } else { false }
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
    for ch in text.chars() {
        let cp = ch as u32;
        if cp < 128 {
            draw_char_scaled(cp as u8, x, y, color, s);
            x += (FONT_WIDTH * s) as i32;
        } else {
            // 非ASCII：暂以实心方块占位，后续用TTF渲染
            gui_fill_rect_xywh(x, y, FONT_WIDTH * s, FONT_HEIGHT * s, color);
            x += (FONT_WIDTH * s) as i32;
        }
    }
}

