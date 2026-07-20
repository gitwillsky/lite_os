//! 桌面壁纸：`assets/wallpaper.xrgb` 的 checked 解析与一次性缩放到屏幕 mode。
//!
//! 文件布局（小端）：8B magic `LWP8\0\0\0\x01`、u32 width、u32 height、
//! width*height*4 字节 XRGB8888 行主序像素（源图 1672x941）。
//!
//! 启动时把源图按当前 mode 双线性缩放进一块匿名 mmap 的独立 buffer（一次性
//! 成本，避免每次合成重复缩放）；之后合成背景按 damage 矩形从该 buffer 直拷。
//! mmap 而不是固定数组：mode 尺寸运行期才知，全屏像素数组进栈会爆栈。

use crate::{
    ffi,
    scanout::{Frame, Mode, Rect},
};

const BYTES: &[u8] = include_bytes!("../../../assets/wallpaper.xrgb");
const MAGIC: &[u8; 8] = b"LWP8\0\0\0\x01";
/// 资产头字节数（magic + width + height）。
const HEADER: usize = 16;

/// 已缩放到屏幕 mode 的壁纸 buffer（匿名 mmap 持有，Drop 时 munmap）。
pub struct Wallpaper {
    pixels: *mut u32,
    map_size: usize,
    width: usize,
}

impl Wallpaper {
    /// 校验资产（magic、尺寸非零、长度恰好对齐），再把源图双线性缩放到
    /// `mode` 尺寸的独立 buffer。任一失败返回 `None`（启动失败）。
    pub fn open(mode: Mode) -> Option<Self> {
        let (source, source_width, source_height) = checked_source()?;
        let map_size = mode.width.checked_mul(mode.height)?.checked_mul(4)?;
        // SAFETY: 匿名映射不触碰 fd；失败返回 MAP_FAILED（usize::MAX）。
        let pixels = unsafe {
            ffi::mmap(
                core::ptr::null_mut(),
                map_size,
                ffi::PROT_READ | ffi::PROT_WRITE,
                ffi::MAP_PRIVATE | ffi::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if pixels as usize == usize::MAX {
            return None;
        }
        let wallpaper = Self {
            pixels: pixels.cast(),
            map_size,
            width: mode.width,
        };
        // SAFETY: pixels 指向 map_size 字节的私有映射，行切片互不重叠。
        let target = unsafe {
            core::slice::from_raw_parts_mut(wallpaper.pixels, mode.width * mode.height)
        };
        scale(source, source_width, source_height, target, mode.width, mode.height);
        Some(wallpaper)
    }

    /// 把 `clip`（屏幕坐标，调用方保证已裁到屏幕内）覆盖的像素从壁纸 buffer
    /// 直拷进 scanout（替代纯色背景）。
    pub fn blit(&self, frame: &mut Frame, clip: Rect) {
        if clip.is_empty() {
            return;
        }
        for y in clip.y1..clip.y2 {
            let y = y as usize;
            // SAFETY: pixels 指向 width*height 个 u32 的私有映射；合成单线程，
            // 读切片与 scanout 行切片不别名。
            let source = unsafe {
                core::slice::from_raw_parts(self.pixels.add(y * self.width), self.width)
            };
            frame.row(y)[clip.x1 as usize..clip.x2 as usize]
                .copy_from_slice(&source[clip.x1 as usize..clip.x2 as usize]);
        }
    }
}

impl Drop for Wallpaper {
    fn drop(&mut self) {
        // SAFETY: pixels/map_size 为本对象持有的匿名映射，仅释放一次。
        unsafe { ffi::munmap(self.pixels.cast(), self.map_size) };
    }
}

/// 校验资产头与总长度，返回像素区字节切片与源图尺寸。
fn checked_source() -> Option<(&'static [u8], usize, usize)> {
    if BYTES.get(..8)? != MAGIC {
        return None;
    }
    let width = read_u32(8)? as usize;
    let height = read_u32(12)? as usize;
    if width == 0 || height == 0 || HEADER + width.checked_mul(height)?.checked_mul(4)? != BYTES.len()
    {
        return None;
    }
    Some((BYTES.get(HEADER..)?, width, height))
}

/// 双线性缩放（16.16 定点）：目标像素中心映射回源图坐标，四角按分数混合。
fn scale(
    source: &[u8],
    source_width: usize,
    source_height: usize,
    target: &mut [u32],
    width: usize,
    height: usize,
) {
    let step_x = ((source_width - 1) as u64) << 16;
    let step_y = ((source_height - 1) as u64) << 16;
    // 源图 XRGB8888 字节序即小端 u32。
    let sample = |x: usize, y: usize| {
        let offset = (y * source_width + x) * 4;
        u32::from_le_bytes(source[offset..offset + 4].try_into().expect("pixel"))
    };
    for y in 0..height {
        let fixed_y = y as u64 * step_y / (height.max(1) - 1).max(1) as u64;
        let y0 = (fixed_y >> 16) as usize;
        let y1 = (y0 + 1).min(source_height - 1);
        let frac_y = (fixed_y & 0xffff) as u32;
        for x in 0..width {
            let fixed_x = x as u64 * step_x / (width.max(1) - 1).max(1) as u64;
            let x0 = (fixed_x >> 16) as usize;
            let x1 = (x0 + 1).min(source_width - 1);
            let frac_x = (fixed_x & 0xffff) as u32;
            let corners = (
                sample(x0, y0),
                sample(x1, y0),
                sample(x0, y1),
                sample(x1, y1),
            );
            target[y * width + x] = bilinear(corners, frac_x, frac_y);
        }
    }
}

/// 四角双线性混合（`frac_*` 为 16.16 定点分数，0..=65535；中间值用 u64 防溢出）。
fn bilinear(corners: (u32, u32, u32, u32), frac_x: u32, frac_y: u32) -> u32 {
    let (a, b, c, d) = corners;
    let (fx, fy) = (u64::from(frac_x), u64::from(frac_y));
    let mix = |pa: u32, pb: u32, pc: u32, pd: u32| {
        let (pa, pb, pc, pd) = (u64::from(pa), u64::from(pb), u64::from(pc), u64::from(pd));
        let top = pa * (65536 - fx) + pb * fx;
        let bottom = pc * (65536 - fx) + pd * fx;
        ((top * (65536 - fy) + bottom * fy) >> 32) as u32
    };
    let red = mix(a >> 16 & 0xff, b >> 16 & 0xff, c >> 16 & 0xff, d >> 16 & 0xff);
    let green = mix(a >> 8 & 0xff, b >> 8 & 0xff, c >> 8 & 0xff, d >> 8 & 0xff);
    let blue = mix(a & 0xff, b & 0xff, c & 0xff, d & 0xff);
    red << 16 | green << 8 | blue
}

fn read_u32(offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        BYTES.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}
