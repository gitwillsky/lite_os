use alloc::{vec::Vec, string::{String, ToString}};
use super::loader;

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
        // PNG文件签名检查
        if !self.can_decode(data) {
            return Err(ImageError::UnsupportedFormat);
        }

        // 简化的PNG解码器实现
        // 实际实现需要完整的PNG解码逻辑
        self.decode_png_simplified(data)
    }

    fn can_decode(&self, data: &[u8]) -> bool {
        // PNG文件签名: 89 50 4E 47 0D 0A 1A 0A
        data.len() >= 8 &&
        data[0] == 0x89 && data[1] == 0x50 && data[2] == 0x4E && data[3] == 0x47 &&
        data[4] == 0x0D && data[5] == 0x0A && data[6] == 0x1A && data[7] == 0x0A
    }
}

impl PngDecoder {
    pub fn new() -> Self {
        Self
    }

    fn decode_png_simplified(&self, data: &[u8]) -> Result<DecodedImage, ImageError> {
        // 这是一个简化的PNG解码器，只支持基本的PNG格式
        // 实际实现需要完整的PNG规范支持

        let mut pos = 8; // 跳过PNG签名
        let mut width = 0u32;
        let mut height = 0u32;
        let mut image_data = Vec::new();

        // 读取IHDR chunk获取图片信息
        if let Some((w, h)) = self.read_ihdr_chunk(data, &mut pos)? {
            width = w;
            height = h;
        }

        // 为简化，创建一个测试用的图片数据（红色正方形）
        let pixel_count = (width * height) as usize;
        image_data.reserve(pixel_count * 4);

        for _ in 0..pixel_count {
            image_data.push(255); // R
            image_data.push(0);   // G
            image_data.push(0);   // B
            image_data.push(255); // A
        }

        Ok(DecodedImage {
            width,
            height,
            data: image_data,
            format: ImageFormat::RGBA,
        })
    }

    fn read_ihdr_chunk(&self, data: &[u8], pos: &mut usize) -> Result<Option<(u32, u32)>, ImageError> {
        if *pos + 21 > data.len() {
            return Err(ImageError::InvalidData);
        }

        // 读取chunk长度
        let length = u32::from_be_bytes([data[*pos], data[*pos + 1], data[*pos + 2], data[*pos + 3]]);
        *pos += 4;

        // 读取chunk类型
        let chunk_type = &data[*pos..*pos + 4];
        *pos += 4;

        if chunk_type == b"IHDR" {
            if length < 13 {
                return Err(ImageError::InvalidData);
            }

            // 读取宽度和高度
            let width = u32::from_be_bytes([data[*pos], data[*pos + 1], data[*pos + 2], data[*pos + 3]]);
            *pos += 4;
            let height = u32::from_be_bytes([data[*pos], data[*pos + 1], data[*pos + 2], data[*pos + 3]]);
            *pos += 4;

            return Ok(Some((width, height)));
        }

        Ok(None)
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
            image_data.push(0);   // R
            image_data.push(0);   // G
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
                image_data.push(255);  // A
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
            },
            Err(err) => {
                println!("[image] Failed to load {}: {:?}, using placeholder", path, err);
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
