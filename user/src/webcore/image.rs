use super::loader;
use alloc::{
    string::{String, ToString},
    vec,
    vec::Vec,
};

//==============================================================================
// 图片解码器接口 (Image Decoder Interface)
//==============================================================================

pub trait ImageDecoder {
    fn decode(&self, data: &[u8]) -> Result<DecodedImage, ImageError>;
    fn can_decode(&self, data: &[u8]) -> bool;
}

//==============================================================================
// 图片数据结构 (Image Data Structures)
//==============================================================================

#[derive(Clone, Debug)]
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>, // RGBA格式，每个像素4字节
    pub format: ImageFormat,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ImageFormat {
    RGBA,
    RGB,
    Grayscale,
}

#[derive(Clone, Debug)]
pub enum ImageError {
    UnsupportedFormat,
    InvalidData,
    IoError,
    DecodingError(String),
}

//==============================================================================
// PNG解码器实现 (PNG Decoder Implementation)
//==============================================================================

pub struct PngDecoder;

impl ImageDecoder for PngDecoder {
    fn decode(&self, data: &[u8]) -> Result<DecodedImage, ImageError> {
        if !self.can_decode(data) {
            return Err(ImageError::UnsupportedFormat);
        }
        self.decode_png(data)
    }

    fn can_decode(&self, data: &[u8]) -> bool {
        data.len() >= 8
            && data[0] == 0x89
            && data[1] == 0x50
            && data[2] == 0x4E
            && data[3] == 0x47
            && data[4] == 0x0D
            && data[5] == 0x0A
            && data[6] == 0x1A
            && data[7] == 0x0A
    }
}

impl PngDecoder {
    pub fn new() -> Self {
        Self
    }

    fn read_be_u32(data: &[u8], pos: usize) -> Result<u32, ImageError> {
        if pos + 4 > data.len() {
            return Err(ImageError::InvalidData);
        }
        Ok(u32::from_be_bytes([
            data[pos],
            data[pos + 1],
            data[pos + 2],
            data[pos + 3],
        ]))
    }

    fn decode_png(&self, data: &[u8]) -> Result<DecodedImage, ImageError> {
        use miniz_oxide::inflate::decompress_to_vec_zlib;
        if data.len() < 8 {
            return Err(ImageError::InvalidData);
        }
        let mut pos = 8;
        let mut width = 0u32;
        let mut height = 0u32;
        let mut bit_depth = 0u8;
        let mut color_type = 0u8;
        let mut interlace = 0u8;
        let mut idat = Vec::new();
        let mut plte: Vec<u8> = Vec::new();
        let mut trns: Vec<u8> = Vec::new();
        while pos + 8 <= data.len() {
            let length = Self::read_be_u32(data, pos)? as usize;
            pos += 4;
            let ctype = &data[pos..pos + 4];
            pos += 4;
            if pos + length + 4 > data.len() {
                return Err(ImageError::InvalidData);
            }
            let chunk_data = &data[pos..pos + length];
            pos += length;
            let _crc = Self::read_be_u32(data, pos)?;
            pos += 4;
            if ctype == b"IHDR" {
                if length < 13 {
                    return Err(ImageError::InvalidData);
                }
                width = Self::read_be_u32(chunk_data, 0)?;
                height = Self::read_be_u32(chunk_data, 4)?;
                bit_depth = chunk_data[8];
                color_type = chunk_data[9];
                let compression = chunk_data[10];
                let filter_method = chunk_data[11];
                interlace = chunk_data[12];
                if compression != 0 || filter_method != 0 {
                    return Err(ImageError::UnsupportedFormat);
                }
                if interlace != 0 {
                    return Err(ImageError::UnsupportedFormat);
                }
                if bit_depth != 8 {
                    return Err(ImageError::UnsupportedFormat);
                }
                match color_type {
                    0 | 2 | 3 | 6 => {}
                    _ => return Err(ImageError::UnsupportedFormat),
                }
            } else if ctype == b"PLTE" {
                plte.clear();
                plte.extend_from_slice(chunk_data);
            } else if ctype == b"tRNS" {
                trns.clear();
                trns.extend_from_slice(chunk_data);
            } else if ctype == b"IDAT" {
                idat.extend_from_slice(chunk_data);
            } else if ctype == b"IEND" {
                break;
            }
        }
        if width == 0 || height == 0 {
            return Err(ImageError::InvalidData);
        }
        let decompressed = decompress_to_vec_zlib(&idat)
            .map_err(|_| ImageError::DecodingError("inflate".to_string()))?;
        let bpp = match color_type {
            0 => 1,
            2 => 3,
            3 => 1,
            6 => 4,
            _ => return Err(ImageError::UnsupportedFormat),
        };
        let stride = width as usize * bpp as usize;
        let expected = height as usize * (1 + stride);
        if decompressed.len() != expected {
            if decompressed.len() < expected {
                return Err(ImageError::InvalidData);
            }
        }
        let mut recon = vec![0u8; height as usize * stride];
        for row in 0..height as usize {
            let in_off = row * (stride + 1);
            let filter = decompressed[in_off];
            let src = &decompressed[in_off + 1..in_off + 1 + stride];
            let out_off = row * stride;
            let (prev_rows, cur_and_after) = recon.split_at_mut(out_off);
            let dst = &mut cur_and_after[..stride];
            let up_slice = if row > 0 {
                &prev_rows[out_off - stride..out_off]
            } else {
                &[][..]
            };
            match filter {
                0 => dst.copy_from_slice(src),
                1 => {
                    for i in 0..stride {
                        let left = if i >= bpp as usize {
                            dst[i - bpp as usize]
                        } else {
                            0
                        };
                        dst[i] = src[i].wrapping_add(left);
                    }
                }
                2 => {
                    for i in 0..stride {
                        let up = if row > 0 { up_slice[i] } else { 0 };
                        dst[i] = src[i].wrapping_add(up);
                    }
                }
                3 => {
                    for i in 0..stride {
                        let left = if i >= bpp as usize {
                            dst[i - bpp as usize]
                        } else {
                            0
                        };
                        let up = if row > 0 { up_slice[i] } else { 0 };
                        let avg = ((left as u16 + up as u16) / 2) as u8;
                        dst[i] = src[i].wrapping_add(avg);
                    }
                }
                4 => {
                    for i in 0..stride {
                        let a = if i >= bpp as usize {
                            dst[i - bpp as usize]
                        } else {
                            0
                        };
                        let b = if row > 0 { up_slice[i] } else { 0 };
                        let c = if row > 0 && i >= bpp as usize {
                            up_slice[i - bpp as usize]
                        } else {
                            0
                        };
                        let pa = (b as i32 - c as i32).abs();
                        let pb = (a as i32 - c as i32).abs();
                        let pc = (a as i32 + b as i32 - 2 * c as i32).abs();
                        let pr = if pa <= pb && pa <= pc {
                            a
                        } else if pb <= pc {
                            b
                        } else {
                            c
                        };
                        dst[i] = src[i].wrapping_add(pr);
                    }
                }
                _ => return Err(ImageError::UnsupportedFormat),
            }
        }
        let mut out = vec![0u8; width as usize * height as usize * 4];
        match color_type {
            6 => {
                for y in 0..height as usize {
                    let mut si = y * stride;
                    let mut di = y * width as usize * 4;
                    for _ in 0..width as usize {
                        out[di] = recon[si];
                        out[di + 1] = recon[si + 1];
                        out[di + 2] = recon[si + 2];
                        out[di + 3] = recon[si + 3];
                        si += 4;
                        di += 4;
                    }
                }
            }
            2 => {
                for y in 0..height as usize {
                    let mut si = y * stride;
                    let mut di = y * width as usize * 4;
                    for _ in 0..width as usize {
                        out[di] = recon[si];
                        out[di + 1] = recon[si + 1];
                        out[di + 2] = recon[si + 2];
                        out[di + 3] = 255;
                        si += 3;
                        di += 4;
                    }
                }
            }
            0 => {
                for y in 0..height as usize {
                    let mut si = y * stride;
                    let mut di = y * width as usize * 4;
                    for _ in 0..width as usize {
                        let g = recon[si];
                        out[di] = g;
                        out[di + 1] = g;
                        out[di + 2] = g;
                        out[di + 3] = 255;
                        si += 1;
                        di += 4;
                    }
                }
            }
            3 => {
                if plte.is_empty() {
                    return Err(ImageError::InvalidData);
                }
                let entries = plte.len() / 3;
                let mut alpha = vec![255u8; entries];
                if !trns.is_empty() {
                    for (i, a) in trns.iter().enumerate() {
                        if i < entries {
                            alpha[i] = *a;
                        }
                    }
                }
                for y in 0..height as usize {
                    let mut si = y * stride;
                    let mut di = y * width as usize * 4;
                    for _ in 0..width as usize {
                        let idx = recon[si] as usize;
                        if idx >= entries {
                            return Err(ImageError::InvalidData);
                        }
                        let pi = idx * 3;
                        out[di] = plte[pi];
                        out[di + 1] = plte[pi + 1];
                        out[di + 2] = plte[pi + 2];
                        out[di + 3] = alpha[idx];
                        si += 1;
                        di += 4;
                    }
                }
            }
            _ => return Err(ImageError::UnsupportedFormat),
        }
        Ok(DecodedImage {
            width,
            height,
            data: out,
            format: ImageFormat::RGBA,
        })
    }
}

//==============================================================================
// JPEG解码器实现 (JPEG Decoder Implementation)
//==============================================================================

pub struct JpegDecoder;

impl ImageDecoder for JpegDecoder {
    fn decode(&self, data: &[u8]) -> Result<DecodedImage, ImageError> {
        if !self.can_decode(data) {
            return Err(ImageError::UnsupportedFormat);
        }

        self.decode_jpeg_simplified(data)
    }

    fn can_decode(&self, data: &[u8]) -> bool {
        // JPEG文件签名: FF D8 FF
        data.len() >= 3 && data[0] == 0xFF && data[1] == 0xD8 && data[2] == 0xFF
    }
}

impl JpegDecoder {
    pub fn new() -> Self {
        Self
    }

    fn decode_jpeg_simplified(&self, data: &[u8]) -> Result<DecodedImage, ImageError> {
        // 简化的JPEG解码器
        // 实际实现需要完整的JPEG解码逻辑

        // 为简化，解析基本的JPEG头信息并创建测试图片（蓝色正方形）
        let (width, height) = self.extract_jpeg_dimensions(data)?;

        let pixel_count = (width * height) as usize;
        let mut image_data = Vec::with_capacity(pixel_count * 4);

        for _ in 0..pixel_count {
            image_data.push(0); // R
            image_data.push(0); // G
            image_data.push(255); // B
            image_data.push(255); // A
        }

        Ok(DecodedImage {
            width,
            height,
            data: image_data,
            format: ImageFormat::RGBA,
        })
    }

    fn extract_jpeg_dimensions(&self, _data: &[u8]) -> Result<(u32, u32), ImageError> {
        // 简化：返回固定尺寸
        // 实际实现需要解析JPEG的SOF段
        Ok((200, 200))
    }
}

//==============================================================================
// 图片管理器 (Image Manager)
//==============================================================================

pub struct ImageManager {
    png_decoder: PngDecoder,
    jpeg_decoder: JpegDecoder,
}

impl ImageManager {
    pub fn new() -> Self {
        Self {
            png_decoder: PngDecoder::new(),
            jpeg_decoder: JpegDecoder::new(),
        }
    }

    pub fn load_image(&self, path: &str) -> Result<DecodedImage, ImageError> {
        // 加载图片文件
        let data = loader::read_all(path).ok_or(ImageError::IoError)?;

        // 尝试不同的解码器
        if self.png_decoder.can_decode(&data) {
            println!("[image] Decoding PNG: {}", path);
            self.png_decoder.decode(&data)
        } else if self.jpeg_decoder.can_decode(&data) {
            println!("[image] Decoding JPEG: {}", path);
            self.jpeg_decoder.decode(&data)
        } else {
            println!("[image] Unsupported format: {}", path);
            Err(ImageError::UnsupportedFormat)
        }
    }

    pub fn create_placeholder(&self, width: u32, height: u32) -> DecodedImage {
        // 创建占位符图片（灰色渐变）
        let pixel_count = (width * height) as usize;
        let mut image_data = Vec::with_capacity(pixel_count * 4);

        for y in 0..height {
            for x in 0..width {
                let gray = ((x + y) % 256) as u8;
                image_data.push(gray); // R
                image_data.push(gray); // G
                image_data.push(gray); // B
                image_data.push(255); // A
            }
        }

        DecodedImage {
            width,
            height,
            data: image_data,
            format: ImageFormat::RGBA,
        }
    }
}

//==============================================================================
// 图片缓存 (Image Cache)
//==============================================================================

use alloc::collections::BTreeMap;

pub struct ImageCache {
    cache: BTreeMap<String, DecodedImage>,
    manager: ImageManager,
}

impl ImageCache {
    pub fn new() -> Self {
        Self {
            cache: BTreeMap::new(),
            manager: ImageManager::new(),
        }
    }

    pub fn get_image(&mut self, path: &str) -> DecodedImage {
        // 检查缓存
        if let Some(image) = self.cache.get(path) {
            println!("[image] Cache hit: {}", path);
            return image.clone();
        }

        // 尝试加载图片
        match self.manager.load_image(path) {
            Ok(image) => {
                println!("[image] Loaded and cached: {}", path);
                self.cache.insert(path.to_string(), image.clone());
                image
            }
            Err(err) => {
                println!(
                    "[image] Failed to load {}: {:?}, using placeholder",
                    path, err
                );
                // 返回占位符
                let placeholder = self.manager.create_placeholder(100, 100);
                self.cache.insert(path.to_string(), placeholder.clone());
                placeholder
            }
        }
    }

    pub fn clear_cache(&mut self) {
        self.cache.clear();
        println!("[image] Cache cleared");
    }
}
