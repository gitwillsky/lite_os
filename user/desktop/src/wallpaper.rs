//! 桌面壁纸：运行时从 rootfs `/usr/share/liteos/wallpaper.xrgb` 读入、checked
//! 解析并一次性缩放到屏幕 mode（资产随镜像分发，不内嵌进二进制）。
//!
//! 文件布局（小端）：8B magic `LWP8\0\0\0\x01`、u32 width、u32 height、
//! width*height*4 字节 XRGB8888 行主序像素。
//!
//! 启动时把源图按当前 mode 双线性缩放进独立 `Vec` buffer（一次性
//! 成本，避免每次合成重复缩放），源文件映射随后立即 `munmap`；之后合成背景
//! 按 damage 矩形从该 buffer 直拷。mode 尺寸运行期才知，不能使用固定数组。
//! 文件缺失或校验失败返回 `None`（启动失败）。

use crate::scanout::{Frame, Mode, Rect};

/// rootfs 中的壁纸路径（NUL 结尾）。
const PATH: &str = "/usr/share/liteos/wallpaper.xrgb";
const MAGIC: &[u8; 8] = b"LWP8\0\0\0\x01";
/// 资产头字节数（magic + width + height）。
const HEADER: usize = 16;

/// 已缩放到屏幕 mode 的壁纸 buffer。
pub struct Wallpaper {
    pixels: Vec<u32>,
    width: usize,
}

impl Wallpaper {
    /// 读入并校验资产（magic、尺寸非零、长度恰好对齐），再把源图双线性缩放到
    /// `mode` 尺寸的独立 buffer，源映射随即释放。任一失败返回 `None`（启动失败）。
    pub fn open(mode: Mode) -> Option<Self> {
        let file = std::fs::read(PATH).ok()?;
        let (source, source_width, source_height) = checked_source(&file)?;
        let pixel_count = mode.width.checked_mul(mode.height)?;
        let mut pixels = Vec::new();
        pixels.try_reserve_exact(pixel_count).ok()?;
        pixels.resize(pixel_count, 0);
        scale(
            source,
            source_width,
            source_height,
            &mut pixels,
            mode.width,
            mode.height,
        );
        Some(Self {
            pixels,
            width: mode.width,
        })
    }

    /// 把 `clip`（屏幕坐标，调用方保证已裁到屏幕内）覆盖的像素从壁纸 buffer
    /// 直拷进 scanout（替代纯色背景）。
    pub fn blit(&self, frame: &mut Frame, clip: Rect) {
        if clip.is_empty() {
            return;
        }
        for y in clip.y1..clip.y2 {
            let y = y as usize;
            let source = &self.pixels[y * self.width..(y + 1) * self.width];
            frame.row(y)[clip.x1 as usize..clip.x2 as usize]
                .copy_from_slice(&source[clip.x1 as usize..clip.x2 as usize]);
        }
    }
}

/// 校验资产头与总长度，返回像素区字节切片与源图尺寸。
fn checked_source(bytes: &[u8]) -> Option<(&[u8], usize, usize)> {
    if bytes.get(..8)? != MAGIC {
        return None;
    }
    let width = read_u32(bytes, 8)? as usize;
    let height = read_u32(bytes, 12)? as usize;
    if width == 0
        || height == 0
        || HEADER + width.checked_mul(height)?.checked_mul(4)? != bytes.len()
    {
        return None;
    }
    Some((bytes.get(HEADER..)?, width, height))
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
    let red = mix(
        a >> 16 & 0xff,
        b >> 16 & 0xff,
        c >> 16 & 0xff,
        d >> 16 & 0xff,
    );
    let green = mix(a >> 8 & 0xff, b >> 8 & 0xff, c >> 8 & 0xff, d >> 8 & 0xff);
    let blue = mix(a & 0xff, b & 0xff, c & 0xff, d & 0xff);
    red << 16 | green << 8 | blue
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}
