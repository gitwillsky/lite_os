//! 桌面精灵表（`assets/desktop-sprites.argb`）的 checked 解析与 alpha blend 绘制。
//!
//! 运行时从 rootfs `/usr/share/liteos/desktop-sprites.argb` 读入（资产随镜像
//! 分发，不内嵌二进制，由 `scripts/convert_sprites.py` 从 `assets/sprites-src/`
//! 的源 PNG 按固定格子合成）；文件缺失或校验失败返回 `None`，即启动失败——
//! Start 按钮与菜单图标没有可降级的绘制路径。
//!
//! 文件布局（小端）：8B magic `LSP8\0\0\0\x01`、u32 width（=576）、u32
//! height（=144），随后 width*height 个 u32 ARGB8888（a<<24|r<<16|g<<8|b）
//! 行主序像素。格子坐标与生成脚本一一对应（见下方常量），改格子必须两侧同步。

use crate::{
    scanout::{Frame, Rect},
    uifont::blend,
};

/// rootfs 中的精灵表路径。
const PATH: &str = "/usr/share/liteos/desktop-sprites.argb";
const MAGIC: &[u8; 8] = b"LSP8\0\0\0\x01";
const WIDTH: usize = 576;
const HEIGHT: usize = 144;
/// 资产头字节数（magic + width + height）。
const HEADER: usize = 16;

/// Start 按钮三个状态格（各 192x60，1× 基准 96x30）。
pub const START_NORMAL: Rect = Rect::new(0, 0, 192, 60);
pub const START_HOVER: Rect = Rect::new(192, 0, 384, 60);
pub const START_PRESSED: Rect = Rect::new(384, 0, 576, 60);
/// 开始菜单用户头像（84x84，1× 基准 42x42）。
pub const AVATAR: Rect = Rect::new(0, 60, 84, 144);
/// 终端 / 通用程序图标（60x60，1× 基准 30x30，菜单左栏项图标）。
pub const ICON_TERMINAL: Rect = Rect::new(84, 60, 144, 120);
pub const ICON_PROGRAM: Rect = Rect::new(144, 60, 204, 120);
/// 关机图标（44x44，1× 基准 22x22，菜单底栏项图标）。
pub const ICON_POWER: Rect = Rect::new(204, 60, 248, 104);
/// 终端图标小档（44x44，1× 基准 22x22，菜单右栏项图标，由 60px 源图
/// LANCZOS 缩小派生）。
pub const ICON_TERMINAL_SMALL: Rect = Rect::new(248, 60, 292, 104);

/// checked 解析后的精灵表（进程生命周期持有，退出时由内核回收，故不释放）。
pub struct Sprites {
    pixels: Vec<u32>,
}

impl Sprites {
    /// 从 rootfs 读入精灵表并校验：magic、尺寸恰为 576x144、长度恰好对齐。
    /// 任一失败返回 `None`（文件缺失、截断或内容损坏）。
    pub fn open() -> Option<Self> {
        let bytes = std::fs::read(PATH).ok()?;
        let valid = bytes.len() == HEADER + WIDTH * HEIGHT * 4
            && bytes.get(..8) == Some(MAGIC.as_slice())
            && read_u32(&bytes, 8) == Some(WIDTH as u32)
            && read_u32(&bytes, 12) == Some(HEIGHT as u32);
        if !valid {
            return None;
        }
        let mut pixels = Vec::new();
        pixels.try_reserve_exact(WIDTH * HEIGHT).ok()?;
        for chunk in bytes[HEADER..].chunks_exact(4) {
            pixels.push(u32::from_le_bytes(chunk.try_into().expect("pixel")));
        }
        Some(Self { pixels })
    }

    /// 把格子 `cell`（表内坐标）以 `origin`（屏幕坐标，格子左上角）alpha blend
    /// 进 scanout，只写 `clip` 内像素。
    pub fn blit(&self, frame: &mut Frame, cell: Rect, origin: (i32, i32), clip: Rect) {
        let dest = Rect::new(
            origin.0,
            origin.1,
            origin.0 + cell.width(),
            origin.1 + cell.height(),
        );
        let area = dest.intersect(clip).intersect(Rect::new(
            0,
            0,
            frame.width() as i32,
            frame.height() as i32,
        ));
        if area.is_empty() {
            return;
        }
        for y in area.y1..area.y2 {
            let source_y = (y - dest.y1 + cell.y1) as usize;
            let row = frame.row(y as usize);
            for x in area.x1..area.x2 {
                let source_x = (x - dest.x1 + cell.x1) as usize;
                let pixel = self.pixels[source_y * WIDTH + source_x];
                let alpha = (pixel >> 24) as u8;
                if alpha != 0 {
                    row[x as usize] = blend(row[x as usize], pixel & 0x00ff_ffff, alpha);
                }
            }
        }
    }
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}
