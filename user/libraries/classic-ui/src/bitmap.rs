//! 系统图标与用户图片的有界解码；用户图片由 PictureLoader 在后台加载，不进入每帧绘制路径。
use crate::Painter;
use canvas::{Color, Image, Rect};
use std::{
    io::{self, Read},
    path::Path,
    sync::Arc,
};

/// 拥有预乘 ARGB 像素的系统图像；加载后可按整数 scale 重用。
#[derive(Debug)]
pub struct Bitmap {
    pixels: Vec<u32>,
    width: i32,
    height: i32,
    // OWNER: 8 位 BMP 的不可变原始下标，由重着色结果共享；颜色改变不改变区域身份。
    // 缺少它只能从 RGB 反推下标，相同调色板颜色的不同区域会被错误合并。
    // Arc 只共享不可变下标，使独占 Bitmap 可安全从解码线程转移到 UI，无 unsafe Send。
    indices: Option<Arc<[u8]>>,
}
impl Bitmap {
    /// 整张位图的预乘 ARGB 视图。
    pub(crate) fn image(&self) -> Image<'_> {
        Image {
            pixels: &self.pixels,
            width: self.width,
            height: self.height,
            stride: self.width as usize,
        }
    }

    /// 位图宽度（逻辑像素）。
    pub fn width(&self) -> i32 {
        self.width
    }

    /// 位图高度（逻辑像素）；工具栏位图条的帧宽等于它。
    pub fn height(&self) -> i32 {
        self.height
    }

    /// 读取界面图像（图标、系统位图、应用 PNG）：单边不超过 2048、编码数据不超过 32 MiB。
    ///
    /// 格式按文件签名识别为 PNG、Windows BMP 或 JPEG；损坏、截断或超限返回 InvalidData。
    pub fn load(path: &Path) -> io::Result<Self> {
        Self::load_limited(path, Limits::UI)
    }

    /// 读取用户图片（如桌面墙纸）：单边不超过 16384、像素总数不超过 1600 万、文件不超过 64 MiB。
    ///
    /// 格式与错误同 [`Bitmap::load`]；上限保证 2 GiB 内存的系统解码一张照片不会耗尽内存。
    pub(crate) fn load_picture(path: &Path) -> io::Result<Self> {
        Self::load_limited(path, Limits::PICTURE)
    }

    fn load_limited(path: &Path, limits: Limits) -> io::Result<Self> {
        let mut bytes = Vec::new();
        std::fs::File::open(path)?
            .take(limits.bytes as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > limits.bytes {
            return Err(invalid_data("image exceeds byte limit"));
        }
        Self::decode(&bytes, limits)
    }

    fn decode(bytes: &[u8], limits: Limits) -> io::Result<Self> {
        if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            Self::decode_png(bytes, limits)
        } else if bytes.starts_with(b"BM") {
            Self::decode_bmp(bytes, limits)
        } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
            Self::decode_jpeg(bytes, limits)
        } else {
            Err(invalid_data("unsupported image format"))
        }
    }

    /// 加载指定逻辑尺寸的系统图标：先用同尺寸原图，再选较大原图，最后选较小原图。
    /// `directory/stem` 指定系统素材目录与安全逻辑名，`size` 只接受 16/32/64。
    /// 缺少同尺寸时仅在加载阶段最近邻缩放，保留像素画边缘；不生成伪造的原始素材文件。
    /// 非法名字/尺寸返回 InvalidInput，全部缺失返回 NotFound，原图损坏与读取失败原样返回。
    pub fn load_icon(directory: &Path, stem: &str, size: i32) -> io::Result<Self> {
        if !matches!(size, 16 | 32 | 64)
            || stem.is_empty()
            || stem.len() > 96
            || !stem
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid icon resource",
            ));
        }
        let candidates = match size {
            16 => [16, 32, 64],
            32 => [32, 64, 16],
            _ => [64, 32, 16],
        };
        for candidate in candidates {
            let image = match Self::load(&directory.join(format!("{stem}-{candidate}.bmp"))) {
                Ok(image) => image,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            return Ok(image.resized(size));
        }
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            "icon resource is not installed",
        ))
    }

    /// 读取 Desktop Entry 的 Icon：绝对图像路径或 XDG hicolor 中的 PNG 图标名。
    ///
    /// `icon` 为条目声明的图标，`size` 只接受 16/32/64；返回最近邻缩放到请求尺寸的预乘位图。
    /// 不同尺寸的主题图优先使用最接近尺寸，系统经典图标沿用已有 BMP 数据。
    /// 非法名称/尺寸返回 InvalidInput，缺失返回 NotFound，PNG/BMP 损坏或超限返回 InvalidData。
    pub fn application_icon(icon: &str, size: i32) -> io::Result<Self> {
        if !matches!(size, 16 | 32 | 64) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid icon size",
            ));
        }
        if Path::new(icon).is_absolute() {
            return Ok(Self::load(Path::new(icon))?.resized(size));
        }
        if icon.is_empty()
            || !icon
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid icon name",
            ));
        }
        let mut candidates = Vec::new();
        let directories = desktop_entry::data_directories();
        for (priority, directory) in directories.iter().enumerate() {
            if let Ok(entries) = std::fs::read_dir(directory.join("icons/hicolor")) {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    let Some((width, height)) = name.to_str().and_then(|name| name.split_once('x'))
                    else {
                        continue;
                    };
                    let (Ok(width), Ok(height)) = (width.parse::<i32>(), height.parse::<i32>())
                    else {
                        continue;
                    };
                    if width != height || !(1..=2048).contains(&width) {
                        continue;
                    }
                    let path = entry.path().join("apps").join(format!("{icon}.png"));
                    if path.is_file() {
                        candidates.push((
                            width != size,
                            (width - size).abs(),
                            width < size,
                            priority,
                            path,
                        ));
                    }
                }
            }
        }
        candidates.sort();
        if let Some((_, _, _, _, path)) = candidates.first() {
            return Ok(Self::load(path)?.resized(size));
        }
        for directory in directories {
            let path = directory.join("pixmaps").join(format!("{icon}.png"));
            if path.is_file() {
                return Ok(Self::load(&path)?.resized(size));
            }
        }
        Self::load_icon(Path::new("/usr/share/liteos/ui"), icon, size)
    }

    fn resized(self, size: i32) -> Self {
        if self.width == size && self.height == size {
            return self;
        }
        let positions: Vec<usize> = (0..size)
            .flat_map(|y| {
                let row = y * self.height / size * self.width;
                let width = self.width;
                (0..size).map(move |x| (row + x * width / size) as usize)
            })
            .collect();
        Self {
            pixels: positions
                .iter()
                .map(|position| self.pixels[*position])
                .collect(),
            indices: self.indices.map(|indices| {
                Arc::from(
                    positions
                        .iter()
                        .map(|position| indices[*position])
                        .collect::<Vec<_>>()
                        .into_boxed_slice(),
                )
            }),
            width: size,
            height: size,
        }
    }

    fn decode_png(bytes: &[u8], limits: Limits) -> io::Result<Self> {
        let invalid = |error| io::Error::new(io::ErrorKind::InvalidData, error);
        let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
        decoder.set_limits(png::Limits {
            bytes: limits.pixels * 4,
        });
        decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
        let mut reader = decoder.read_info().map_err(invalid)?;
        let (width, height) = (reader.info().width, reader.info().height);
        limits.check(width, height)?;
        let length = reader
            .output_buffer_size()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "PNG output overflow"))?;
        let mut bytes = vec![0; length];
        let output = reader.next_frame(&mut bytes).map_err(invalid)?;
        let (width, height) = (output.width, output.height);
        let channels = output.color_type.samples();
        let pixels = bytes[..output.buffer_size()]
            .chunks_exact(channels)
            .map(|pixel| {
                let (r, g, b, a) = match output.color_type {
                    png::ColorType::Rgb => (pixel[0], pixel[1], pixel[2], 255),
                    png::ColorType::Rgba => (pixel[0], pixel[1], pixel[2], pixel[3]),
                    png::ColorType::Grayscale => (pixel[0], pixel[0], pixel[0], 255),
                    png::ColorType::GrayscaleAlpha => (pixel[0], pixel[0], pixel[0], pixel[1]),
                    png::ColorType::Indexed => unreachable!("EXPAND converts indexed PNG"),
                };
                let a = u32::from(a);
                a << 24
                    | (u32::from(r) * a / 255) << 16
                    | (u32::from(g) * a / 255) << 8
                    | (u32::from(b) * a / 255)
            })
            .collect();
        Ok(Self {
            pixels,
            width: width as i32,
            height: height as i32,
            indices: None,
        })
    }

    /// 解码 Windows DIB：BITMAPCOREHEADER、BITMAPINFOHEADER 与 V2–V5 扩展头；1/4/8/16/24/32 位，
    /// BI_RGB、BI_RLE4、BI_RLE8、BI_BITFIELDS 与 BI_ALPHABITFIELDS。
    ///
    /// 1. 高度为负表示自顶向下；RLE 只允许自底向上，与 GDI 相同。
    /// 2. 只有位域里声明了 alpha 掩码才使用 alpha；BI_RGB 的 32 位第四字节按 GDI 视为保留，图像不透明。
    /// 3. RLE 跳过（delta/提前结束行）的像素保持透明，绘制时露出背景，与 GDI 不写这些像素一致。
    /// 4. 未压缩的调色板图像保留原始下标，供 `with_palette` 按区域重新着色。
    fn decode_bmp(bytes: &[u8], limits: Limits) -> io::Result<Self> {
        let u16_at = |at: usize| {
            bytes
                .get(at..at + 2)
                .map(|value| u16::from_le_bytes([value[0], value[1]]))
                .ok_or_else(|| invalid_data("truncated BMP header"))
        };
        let u32_at = |at: usize| {
            bytes
                .get(at..at + 4)
                .map(|value| u32::from_le_bytes([value[0], value[1], value[2], value[3]]))
                .ok_or_else(|| invalid_data("truncated BMP header"))
        };
        let offset = u32_at(10)? as usize;
        let header = u32_at(14)? as usize;
        let (width, height, planes, bpp, compression, used) = match header {
            12 => (
                i32::from(u16_at(18)?),
                i32::from(u16_at(20)?),
                u16_at(22)?,
                u16_at(24)?,
                BI_RGB,
                0,
            ),
            40 | 52 | 56 | 108 | 124 => (
                u32_at(18)? as i32,
                u32_at(22)? as i32,
                u16_at(26)?,
                u16_at(28)?,
                u32_at(30)?,
                u32_at(46)? as usize,
            ),
            _ => return Err(invalid_data("unsupported BMP header")),
        };
        let top_down = height < 0;
        let (width, height) = (width.unsigned_abs(), height.unsigned_abs());
        limits.check(width, height)?;
        let (width, height) = (width as usize, height as usize);
        let valid = planes == 1
            && match compression {
                BI_RGB => matches!(bpp, 1 | 4 | 8 | 16 | 24 | 32),
                BI_RLE8 => bpp == 8 && !top_down,
                BI_RLE4 => bpp == 4 && !top_down,
                BI_BITFIELDS | BI_ALPHABITFIELDS => matches!(bpp, 16 | 32),
                _ => false,
            };
        if !valid {
            return Err(invalid_data("unsupported BMP encoding"));
        }
        // 位域掩码紧跟 40 字节头（与 V2+ 头内掩码位置相同），alpha 掩码只在 V3+ 头或 ALPHABITFIELDS 中存在。
        let masks = match (compression, bpp) {
            (BI_BITFIELDS | BI_ALPHABITFIELDS, _) => [
                u32_at(54)?,
                u32_at(58)?,
                u32_at(62)?,
                if header >= 56 || compression == BI_ALPHABITFIELDS {
                    u32_at(66)?
                } else {
                    0
                },
            ],
            (_, 16) => [0x7c00, 0x03e0, 0x001f, 0],
            _ => [0x00ff_0000, 0x0000_ff00, 0x0000_00ff, 0],
        };
        let channels = masks.map(Channel::new);
        if channels.iter().any(Option::is_none) {
            return Err(invalid_data("non-contiguous BMP bit mask"));
        }
        let channels = channels.map(|channel| channel.expect("validated mask"));
        let palette: Vec<u32> = if bpp <= 8 {
            let entry = if header == 12 { 3 } else { 4 };
            let count = match used {
                0 => 1 << bpp,
                count => count.min(1 << bpp),
            };
            let start = 14 + header;
            bytes
                .get(start..start + count * entry)
                .ok_or_else(|| invalid_data("truncated BMP palette"))?
                .chunks_exact(entry)
                .map(|color| opaque(color[2], color[1], color[0]))
                .collect()
        } else {
            Vec::new()
        };
        let data = bytes
            .get(offset..)
            .ok_or_else(|| invalid_data("BMP pixel data outside file"))?;
        if matches!(compression, BI_RLE8 | BI_RLE4) {
            let indices = decode_rle(data, width, height, compression == BI_RLE4)?;
            let pixels = indices
                .iter()
                .map(|index| index.map_or(0, |index| color_at(&palette, index)))
                .collect();
            return Ok(Self {
                pixels,
                width: width as i32,
                height: height as i32,
                indices: None,
            });
        }
        let stride = (bpp as usize * width).div_ceil(32) * 4;
        let rows = data
            .get(..stride * height)
            .ok_or_else(|| invalid_data("truncated BMP pixel data"))?;
        let mut pixels = vec![0; width * height];
        let mut indices = (bpp <= 8).then(|| vec![0u8; width * height]);
        for (file_row, row) in rows.chunks_exact(stride).enumerate() {
            let y = if top_down {
                file_row
            } else {
                height - 1 - file_row
            };
            let target = &mut pixels[y * width..][..width];
            match bpp {
                1 | 4 | 8 => {
                    let per_byte = 8 / bpp as usize;
                    let row_indices =
                        &mut indices.as_mut().expect("palette image indices")[y * width..][..width];
                    for (x, (pixel, slot)) in
                        target.iter_mut().zip(row_indices.iter_mut()).enumerate()
                    {
                        let byte = row[x / per_byte];
                        let shift = 8 - bpp as usize * (x % per_byte + 1);
                        let index = (byte >> shift) & ((1u16 << bpp) - 1) as u8;
                        *slot = index;
                        *pixel = color_at(&palette, index);
                    }
                }
                24 => {
                    for (pixel, color) in target.iter_mut().zip(row.as_chunks::<3>().0) {
                        *pixel = opaque(color[2], color[1], color[0]);
                    }
                }
                _ => {
                    let size = bpp as usize / 8;
                    for (pixel, value) in target.iter_mut().zip(row.chunks_exact(size)) {
                        let value = value
                            .iter()
                            .rev()
                            .fold(0u32, |sum, byte| sum << 8 | u32::from(*byte));
                        let [red, green, blue, alpha] = channels.map(|c| c.value(value));
                        let alpha = if channels[3].mask == 0 { 255 } else { alpha };
                        *pixel = premultiplied(red, green, blue, alpha);
                    }
                }
            }
        }
        Ok(Self {
            pixels,
            width: width as i32,
            height: height as i32,
            indices: indices.map(|indices| Arc::from(indices.into_boxed_slice())),
        })
    }

    /// 解码基线与渐进式 JPEG（灰度、YCbCr、CMYK/YCCK 由解码器转换为 RGB）；JPEG 没有透明度。
    fn decode_jpeg(bytes: &[u8], limits: Limits) -> io::Result<Self> {
        use zune_core::{colorspace::ColorSpace, options::DecoderOptions};
        let options = DecoderOptions::default()
            .set_max_width(limits.side as usize)
            .set_max_height(limits.side as usize)
            .jpeg_set_out_colorspace(ColorSpace::RGB);
        let mut decoder =
            zune_jpeg::JpegDecoder::new_with_options(std::io::Cursor::new(bytes), options);
        let invalid =
            |error: zune_jpeg::errors::DecodeErrors| invalid_data_owned(error.to_string());
        decoder.decode_headers().map_err(invalid)?;
        let info = decoder
            .info()
            .ok_or_else(|| invalid_data("JPEG headers missing"))?;
        limits.check(u32::from(info.width), u32::from(info.height))?;
        let samples = decoder.decode().map_err(invalid)?;
        let channels = decoder
            .output_colorspace()
            .map_or(0, |colorspace| colorspace.num_components());
        let (width, height) = (usize::from(info.width), usize::from(info.height));
        if channels == 0 || samples.len() != width * height * channels {
            return Err(invalid_data("unexpected JPEG output layout"));
        }
        let pixels = samples
            .chunks_exact(channels)
            .map(|sample| match channels {
                1 | 2 => opaque(sample[0], sample[0], sample[0]),
                _ => opaque(sample[0], sample[1], sample[2]),
            })
            .collect();
        Ok(Self {
            pixels,
            width: width as i32,
            height: height as i32,
            indices: None,
        })
    }

    /// 按原始 8-bit 下标生成着色后的不可变位图，保留尺寸与下标；不修改共享源图像。
    /// palette 为 256 个 0xRRGGBB 项；非调色板图像或颜色含高位数据返回 InvalidInput。
    pub fn with_palette(&self, palette: &[u32; 256]) -> io::Result<Self> {
        let indices = self.indices.as_ref().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "bitmap has no indexed palette")
        })?;
        if palette.iter().any(|color| color & 0xff000000 != 0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "palette colors must be RGB",
            ));
        }
        let pixels = indices
            .iter()
            .map(|index| 0xff000000 | palette[usize::from(*index)])
            .collect();
        Ok(Self {
            pixels,
            width: self.width,
            height: self.height,
            indices: Some(indices.clone()),
        })
    }

    #[cfg(test)]
    pub(crate) fn from_pixels(pixels: Vec<u32>, width: i32, height: i32) -> Self {
        Self {
            pixels,
            width,
            height,
            indices: None,
        }
    }

    /// 按 Painter 的整数输出 scale 绘制，超出绘图区自动裁剪；坐标为逻辑像素。
    pub fn paint(&self, painter: &mut Painter<'_>, x: i32, y: i32) {
        self.paint_region(painter, x, y, 0, self.width);
    }

    /// 选中项的图像（comctl32 ILD_BLEND50）：先画图像，再在不透明像素上按逻辑像素棋盘格叠 `color`。
    pub fn paint_blended(
        &self,
        painter: &mut Painter<'_>,
        x: i32,
        y: i32,
        color: classic_theme::SystemColor,
    ) {
        self.paint(painter, x, y);
        for row in 0..self.height {
            for column in (row % 2..self.width).step_by(2) {
                if self.pixels[(row * self.width + column) as usize] >> 24 >= 0x80 {
                    painter.fill(Rect::new(x + column, y + row, 1, 1), color);
                }
            }
        }
    }

    /// 用图像第一行第一个像素的颜色填充 `x, y` 起宽为图像宽、高为 `height` 的区域（竖幅图高度不足时向上补色）。
    pub fn fill_with_corner(&self, painter: &mut Painter<'_>, x: i32, y: i32, height: i32) {
        let pixel = self.pixels[0];
        let color = Color::rgb((pixel >> 16) as u8, (pixel >> 8) as u8, pixel as u8);
        let rect = Rect::new(x, y, self.width, height);
        let physical = painter.physical(rect);
        painter.surface.fill_rect(physical, color);
    }

    /// 只绘制 `source_y..source_y + source_height` 这一横条（帧竖排的动画位图按帧取用）；越界不绘制。
    pub fn paint_rows(
        &self,
        painter: &mut Painter<'_>,
        x: i32,
        y: i32,
        source_y: i32,
        source_height: i32,
    ) {
        if source_y < 0 || source_height < 1 || source_y + source_height > self.height {
            return;
        }
        let scale = painter.scale;
        let start = source_y as usize * self.width as usize;
        painter.surface.blit(
            Image {
                pixels: &self.pixels[start..],
                width: self.width,
                height: source_height,
                stride: self.width as usize,
            },
            x * scale,
            y * scale,
            scale,
        );
    }

    /// 横向按最近邻拉伸到 `width` 逻辑像素（CSS `width: 100%` 的图片，如 Web 视图分隔线）。
    pub fn paint_stretched(&self, painter: &mut Painter<'_>, x: i32, y: i32, width: i32) {
        if self.width < 1 || width < 1 {
            return;
        }
        for column in 0..width {
            let source = (i64::from(column) * i64::from(self.width) / i64::from(width)) as i32;
            self.paint_region(painter, x + column - source, y, source, 1);
        }
    }

    /// 只绘制 `source_x..source_x + source_width` 这一竖条（工具栏位图条按帧取用）；越界的部分不绘制。
    pub fn paint_region(
        &self,
        painter: &mut Painter<'_>,
        x: i32,
        y: i32,
        source_x: i32,
        source_width: i32,
    ) {
        if source_x < 0 || source_width < 1 || source_x + source_width > self.width {
            return;
        }
        let scale = painter.scale;
        painter.surface.blit(
            Image {
                pixels: &self.pixels[source_x as usize..],
                width: source_width,
                height: self.height,
                stride: self.width as usize,
            },
            x * scale,
            y * scale,
            scale,
        );
    }
}

// Windows DIB biCompression 取值（wingdi.h）。
const BI_RGB: u32 = 0;
const BI_RLE8: u32 = 1;
const BI_RLE4: u32 = 2;
const BI_BITFIELDS: u32 = 3;
const BI_ALPHABITFIELDS: u32 = 6;

/// 一次解码允许的尺寸与输入上限；在分配像素前检查，超限文件不能耗尽内存。
#[derive(Clone, Copy)]
struct Limits {
    side: u32,
    pixels: usize,
    bytes: usize,
}

impl Limits {
    /// 图标、系统位图与应用 PNG。
    const UI: Self = Self {
        side: 2048,
        pixels: 2048 * 2048,
        bytes: 32 * 1024 * 1024,
    };
    /// 用户图片：约 1677 万像素（2^24，64 MiB 预乘像素）。
    const PICTURE: Self = Self {
        side: 16384,
        pixels: 1 << 24,
        bytes: 64 * 1024 * 1024,
    };

    fn check(self, width: u32, height: u32) -> io::Result<()> {
        if width == 0
            || height == 0
            || width > self.side
            || height > self.side
            || width as usize * height as usize > self.pixels
        {
            return Err(invalid_data("image dimensions exceed limit"));
        }
        Ok(())
    }
}

/// BMP 位域中的一个连续通道；掩码为零表示该通道不存在。
#[derive(Clone, Copy)]
struct Channel {
    mask: u32,
    shift: u32,
    max: u64,
}

impl Channel {
    /// 非连续掩码返回 None；GDI 同样拒绝这类位域。
    fn new(mask: u32) -> Option<Self> {
        if mask == 0 {
            return Some(Self {
                mask,
                shift: 0,
                max: 0,
            });
        }
        let shift = mask.trailing_zeros();
        let max = u64::from(mask >> shift);
        (max & (max + 1) == 0).then_some(Self { mask, shift, max })
    }

    /// 把通道值按比例展开到 8 位并四舍五入。
    fn value(self, pixel: u32) -> u8 {
        if self.max == 0 {
            return 0;
        }
        ((u64::from((pixel & self.mask) >> self.shift) * 255 + self.max / 2) / self.max) as u8
    }
}

fn invalid_data(reason: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, reason)
}

fn invalid_data_owned(reason: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, reason)
}

fn opaque(red: u8, green: u8, blue: u8) -> u32 {
    0xff00_0000 | u32::from(red) << 16 | u32::from(green) << 8 | u32::from(blue)
}

fn premultiplied(red: u8, green: u8, blue: u8, alpha: u8) -> u32 {
    let alpha = u32::from(alpha);
    alpha << 24
        | (u32::from(red) * alpha / 255) << 16
        | (u32::from(green) * alpha / 255) << 8
        | (u32::from(blue) * alpha / 255)
}

/// 超出调色板的下标按 GDI 显示为黑色，不拒绝整张图。
fn color_at(palette: &[u32], index: u8) -> u32 {
    palette
        .get(usize::from(index))
        .copied()
        .unwrap_or(0xff00_0000)
}

/// 展开自底向上的 BI_RLE8/BI_RLE4 数据为自顶向下的下标；未写到的像素为 None。
///
/// 1. 编码段 `(n, v)` 重复 n 个像素（RLE4 交替高低半字节）。
/// 2. 转义 `0,0` 换行、`0,1` 结束、`0,2,dx,dy` 跳转，`0,n` 为 n 个绝对像素并按字对齐。
/// 3. 超出图像的像素丢弃；数据在结束标记前截断返回 InvalidData。
fn decode_rle(data: &[u8], width: usize, height: usize, four: bool) -> io::Result<Vec<Option<u8>>> {
    let truncated = || invalid_data("truncated BMP RLE data");
    let nibble = |value: u8, index: usize| {
        if !four {
            value
        } else if index.is_multiple_of(2) {
            value >> 4
        } else {
            value & 0x0f
        }
    };
    let mut output = vec![None; width * height];
    let (mut x, mut y, mut at) = (0usize, 0usize, 0usize);
    while y < height {
        let [count, value] = *data.get(at..at + 2).ok_or_else(truncated)? else {
            unreachable!("two-byte RLE record")
        };
        at += 2;
        let mut put = |x: usize, index: u8| {
            if x < width {
                output[(height - 1 - y) * width + x] = Some(index);
            }
        };
        match (count, value) {
            (0, 0) => {
                x = 0;
                y += 1;
            }
            (0, 1) => break,
            (0, 2) => {
                let [dx, dy] = *data.get(at..at + 2).ok_or_else(truncated)? else {
                    unreachable!("two-byte RLE delta")
                };
                at += 2;
                x = x.saturating_add(usize::from(dx));
                y = y.saturating_add(usize::from(dy));
            }
            (0, absolute) => {
                let pixels = usize::from(absolute);
                let bytes = if four { pixels.div_ceil(2) } else { pixels };
                let run = data.get(at..at + bytes).ok_or_else(truncated)?;
                for index in 0..pixels {
                    let byte = run[if four { index / 2 } else { index }];
                    put(x.saturating_add(index), nibble(byte, index));
                }
                x = x.saturating_add(pixels);
                at += bytes.next_multiple_of(2);
            }
            (count, value) => {
                for index in 0..usize::from(count) {
                    put(x.saturating_add(index), nibble(value, index));
                }
                x = x.saturating_add(usize::from(count));
            }
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn v4_bitmap_is_bounded_and_premultiplies_alpha() {
        let mut bytes = vec![0; 126];
        bytes[..2].copy_from_slice(b"BM");
        for (offset, value) in [
            (10, 122u32),
            (14, 108),
            (18, 1),
            (22, (-1i32) as u32),
            (26, 0x0020_0001),
            (30, 3),
            (54, 0x00ff0000),
            (58, 0x0000ff00),
            (62, 0xff),
            (66, 0xff000000),
            (70, 0x73524742),
        ] {
            bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        bytes[122..].copy_from_slice(&[0, 0, 255, 128]);
        assert_eq!(
            Bitmap::decode(&bytes, Limits::UI).unwrap().pixels,
            [0x80800000]
        );
        for length in 0..bytes.len() {
            assert!(Bitmap::decode(&bytes[..length], Limits::UI).is_err());
        }
        bytes[22..26].copy_from_slice(&i32::MIN.to_le_bytes());
        assert!(Bitmap::decode(&bytes, Limits::UI).is_err());
    }

    /// 写一个 40 字节 BITMAPINFOHEADER 的 BMP；`extra` 是调色板或位域，`data` 是像素数据。
    fn info_bmp(
        width: i32,
        height: i32,
        bpp: u16,
        compression: u32,
        extra: &[u8],
        data: &[u8],
    ) -> Vec<u8> {
        let offset = 54 + extra.len() as u32;
        let mut bytes = b"BM".to_vec();
        for value in [offset + data.len() as u32, 0, offset, 40] {
            bytes.extend(value.to_le_bytes());
        }
        bytes.extend(width.to_le_bytes());
        bytes.extend(height.to_le_bytes());
        bytes.extend(1u16.to_le_bytes());
        bytes.extend(bpp.to_le_bytes());
        bytes.extend(compression.to_le_bytes());
        // biSizeImage、两项分辨率为零；调色板图像的 biClrUsed 声明实际条目数。
        bytes.extend([0; 12]);
        let used = if bpp <= 8 { extra.len() as u32 / 4 } else { 0 };
        bytes.extend(used.to_le_bytes());
        bytes.extend([0; 4]);
        bytes.extend(extra);
        bytes.extend(data);
        bytes
    }

    #[test]
    fn windows_2000_wallpapers_decode_bottom_up_palettes_like_gdi() {
        // 参照值由独立的 Pillow 解码同一原版文件得到：(x, y) 处 0xRRGGBB 与全图 RGB 分量和。
        let directory =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../assets/classic/wallpapers");
        for (name, size, samples, total) in [
            ("Blue Lace 16.bmp", 48, [0x000080; 5], 398_163u64),
            (
                "Soap Bubbles.bmp",
                256,
                [0x293184, 0x31318c, 0x31318c, 0x31398c, 0x101073],
                10_874_642,
            ),
        ] {
            let bitmap = Bitmap::load_picture(&directory.join(name)).unwrap();
            assert_eq!((bitmap.width, bitmap.height), (size, size), "{name}");
            let points = [
                (0, 0),
                (size - 1, 0),
                (0, size - 1),
                (size - 1, size - 1),
                (size / 2, size / 3),
            ];
            for ((x, y), expected) in points.into_iter().zip(samples) {
                assert_eq!(
                    bitmap.pixels[(y * size + x) as usize],
                    0xff00_0000 | expected,
                    "{name} ({x},{y})"
                );
            }
            let sum: u64 = bitmap
                .pixels
                .iter()
                .map(|pixel| {
                    u64::from(pixel >> 16 & 0xff)
                        + u64::from(pixel >> 8 & 0xff)
                        + u64::from(pixel & 0xff)
                })
                .sum();
            assert_eq!(sum, total, "{name}");
            assert!(bitmap.indices.is_some(), "{name} keeps palette indices");
        }
    }

    #[test]
    fn rle8_skipped_pixels_stay_transparent_and_absolute_runs_are_word_aligned() {
        let palette = [0, 0, 0, 0, 0, 0, 255, 0, 0, 255, 0, 0];
        // 自底向上：底行 = 2×下标1、3 个绝对像素 [2,1,2]（补齐到偶数）；顶行跳过 2 列后 1×下标2。
        let data = [2, 1, 0, 3, 2, 1, 2, 0, 0, 0, 0, 2, 2, 0, 1, 2, 0, 1];
        let bitmap =
            Bitmap::decode(&info_bmp(5, 2, 8, BI_RLE8, &palette, &data), Limits::UI).unwrap();
        let (red, green) = (0xffff_0000, 0xff00_ff00);
        assert_eq!(
            bitmap.pixels,
            [0, 0, green, 0, 0, red, red, green, red, green]
        );
        assert!(
            Bitmap::decode(
                &info_bmp(5, 2, 8, BI_RLE8, &palette, &data[..5]),
                Limits::UI
            )
            .is_err()
        );
        assert!(Bitmap::decode(&info_bmp(5, -2, 8, BI_RLE8, &palette, &data), Limits::UI).is_err());
    }

    #[test]
    fn bitfields_expand_565_and_reject_non_contiguous_masks() {
        let masks: Vec<u8> = [0xf800u32, 0x07e0, 0x001f]
            .iter()
            .flat_map(|mask| mask.to_le_bytes())
            .collect();
        let data = [0x00, 0xf8, 0xe0, 0x07, 0x1f, 0x00, 0x00, 0x00];
        let bitmap = Bitmap::decode(
            &info_bmp(3, -1, 16, BI_BITFIELDS, &masks, &data),
            Limits::UI,
        )
        .unwrap();
        assert_eq!(bitmap.pixels, [0xffff_0000, 0xff00_ff00, 0xff00_00ff]);
        let broken: Vec<u8> = [0xf00fu32, 0x07e0, 0x001f]
            .iter()
            .flat_map(|mask| mask.to_le_bytes())
            .collect();
        assert!(
            Bitmap::decode(
                &info_bmp(3, -1, 16, BI_BITFIELDS, &broken, &data),
                Limits::UI
            )
            .is_err()
        );
    }

    #[test]
    fn jpeg_color_and_grayscale_become_opaque_rgb() {
        let color = Bitmap::decode(include_bytes!("testdata/solid-16x8.jpg"), Limits::UI).unwrap();
        assert_eq!((color.width, color.height), (16, 8));
        assert_eq!(color.pixels[2 * 16 + 2], 0xffc8_1e27);
        let gray = Bitmap::decode(include_bytes!("testdata/gray-8x8.jpg"), Limits::UI).unwrap();
        assert_eq!(gray.pixels[2 * 8 + 2], 0xff4d_4d4d);
        let truncated = include_bytes!("testdata/solid-16x8.jpg");
        assert!(Bitmap::decode(&truncated[..truncated.len() / 2], Limits::UI).is_err());
    }
}

#[cfg(test)]
mod png_tests {
    use super::*;

    #[test]
    fn png_expands_palette_and_premultiplies_alpha_before_resizing() {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, 2, 1);
            encoder.set_color(png::ColorType::Indexed);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.set_palette(vec![255, 128, 0, 10, 20, 30]);
            encoder.set_trns(vec![128, 0]);
            encoder
                .write_header()
                .unwrap()
                .write_image_data(&[0, 1])
                .unwrap();
        }
        let bitmap = Bitmap::decode_png(&bytes, Limits::UI).unwrap();
        assert_eq!(bitmap.pixels, [0x8080_4000, 0]);
        let scaled = bitmap.resized(16);
        assert_eq!(
            (scaled.width, scaled.height, scaled.pixels.len()),
            (16, 16, 256)
        );
        assert_eq!(scaled.pixels[0], 0x8080_4000);
        assert_eq!(scaled.pixels[15], 0);
        assert!(Bitmap::decode_png(&bytes[..bytes.len() / 2], Limits::UI).is_err());
    }

    #[test]
    fn png_rejects_oversized_dimensions_before_allocating_output() {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, 2049, 1);
            encoder.set_color(png::ColorType::Grayscale);
            encoder
                .write_header()
                .unwrap()
                .write_image_data(&[0; 2049])
                .unwrap();
        }
        assert_eq!(
            Bitmap::decode_png(&bytes, Limits::UI).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}
