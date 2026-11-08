//! 构建期预解码的系统位图；唯一映射 owner 保活只读像素和调色板下标。
use linux_uapi::filesystem::ReadOnlyMapping;
use std::{
    fmt, io,
    ops::Deref,
    path::Path,
    sync::{Arc, Mutex, Weak},
};
const DIRECTORY: &str = "/usr/share/liteos/ui";
const HEADER: usize = 32;
const RECORD: usize = 128;
const NAME: usize = 96;

// OWNER: 进程内只合并活位图的映射，Weak 不延长最后一个图像的寿命。锁只在资源装载时取得，
// 绘制直接借用映射，不锁、不分配；不缓存失败，不把错误 atlas 退回逐图解码。
static ATLAS: Mutex<Weak<Atlas>> = Mutex::new(Weak::new());
struct Atlas {
    mapping: ReadOnlyMapping,
    count: usize,
}
/// 已验证的图集记录与映射租约；只读视图不能脱离其 Arc owner。
#[derive(Clone)]
pub(super) struct SharedImage {
    atlas: Arc<Atlas>,
    // OWNER: 只存已验证的固定记录编号，不在每个 Bitmap 重复保留像素/下标区间。
    record: usize,
    pub(super) width: i32,
    pub(super) height: i32,
}
impl fmt::Debug for SharedImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedImage")
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}
impl SharedImage {
    fn record(&self) -> &[u8] {
        let start = HEADER + self.record * RECORD;
        &self.atlas.mapping.as_ref()[start..start + RECORD]
    }
    fn pixels(&self) -> &[u32] {
        let start = offset(self.record(), 104).expect("validated pixel offset");
        let count = self.width as usize * self.height as usize;
        let bytes = &self.atlas.mapping.as_ref()[start..start + count * 4];
        // SAFETY: validate 在发表映射前验证 u32 对齐、整像素长度和完整文件内范围；mmap 基址页对齐，
        // 仅允许小端像素目标。Arc 保活只读映射，生产安装使用 rename，不原地修改/截断其 inode。
        unsafe { std::slice::from_raw_parts(bytes.as_ptr().cast(), count) }
    }
    fn indices(&self) -> Option<&[u8]> {
        let start = offset(self.record(), 112).expect("validated index offset");
        let count = self.width as usize * self.height as usize;
        (start != 0).then(|| &self.atlas.mapping.as_ref()[start..start + count])
    }
}
/// 解码结果或共享预解码像素的唯一存储 owner。
#[derive(Debug)]
pub(super) enum Pixels {
    Owned(Vec<u32>),
    Shared(SharedImage),
}
impl Deref for Pixels {
    type Target = [u32];
    fn deref(&self) -> &[u32] {
        match self {
            Self::Owned(pixels) => pixels,
            Self::Shared(image) => image.pixels(),
        }
    }
}
impl From<Vec<u32>> for Pixels {
    fn from(pixels: Vec<u32>) -> Self {
        Self::Owned(pixels)
    }
}
/// 可被重着色结果复用的原始调色板下标，不复制映射内容。
#[derive(Clone, Debug)]
pub(super) enum Indices {
    Owned(Arc<[u8]>),
    Shared(SharedImage),
}
impl Deref for Indices {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            Self::Owned(indices) => indices,
            Self::Shared(image) => image.indices().expect("indexed atlas image"),
        }
    }
}
impl From<Vec<u8>> for Indices {
    fn from(indices: Vec<u8>) -> Self {
        Self::Owned(Arc::from(indices.into_boxed_slice()))
    }
}
impl SharedImage {
    pub(super) fn bitmap(self) -> super::Bitmap {
        let indices = self
            .indices()
            .is_some()
            .then(|| Indices::Shared(self.clone()));
        super::Bitmap {
            width: self.width,
            height: self.height,
            pixels: Pixels::Shared(self),
            indices,
        }
    }
}
fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid system bitmap atlas")
}
fn word(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(
        bytes[offset..offset + 4]
            .try_into()
            .expect("validated header"),
    )
}
fn offset(bytes: &[u8], start: usize) -> io::Result<usize> {
    usize::try_from(u64::from_le_bytes(
        bytes[start..start + 8]
            .try_into()
            .expect("validated header"),
    ))
    .map_err(|_| invalid())
}
fn name(record: &[u8]) -> &[u8] {
    let end = record[..NAME]
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(NAME);
    &record[..end]
}
/// 验证固定索引与每个视图范围；只有验证成功的不可变映射能构造 SharedImage。
pub(super) fn validate(bytes: &[u8]) -> io::Result<usize> {
    if !cfg!(target_endian = "little")
        || bytes.len() < HEADER
        || &bytes[..8] != b"LUIA0001"
        || word(bytes, 12) != RECORD as u32
        || offset(bytes, 16)? != bytes.len()
        || offset(bytes, 24)? != 0
    {
        return Err(invalid());
    }
    let count = word(bytes, 8) as usize;
    let index_end = count
        .checked_mul(RECORD)
        .and_then(|n| n.checked_add(HEADER))
        .ok_or_else(invalid)?;
    if count == 0 || index_end > bytes.len() {
        return Err(invalid());
    }
    let payload = index_end.checked_add(4095).ok_or_else(invalid)? / 4096 * 4096;
    let mut previous = &[][..];
    for record in bytes[HEADER..index_end].as_chunks::<RECORD>().0 {
        let current = name(record);
        let stem = current.strip_suffix(b".bmp").ok_or_else(invalid)?;
        if stem.is_empty()
            || current.len() == NAME
            || current <= previous
            || !stem
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
            || record[current.len()..NAME].iter().any(|byte| *byte != 0)
            || offset(record, 120)? != 0
        {
            return Err(invalid());
        }
        previous = current;
        let (width, height) = (word(record, 96), word(record, 100));
        super::Limits::UI.check(width, height)?;
        let pixels = width as usize * height as usize;
        let start = offset(record, 104)?;
        let end = start.checked_add(pixels * 4).ok_or_else(invalid)?;
        let indices = offset(record, 112)?;
        if start < payload
            || !start.is_multiple_of(4)
            || end > bytes.len()
            || (indices != 0
                && (indices < payload
                    || indices
                        .checked_add(pixels)
                        .is_none_or(|end| end > bytes.len())))
        {
            return Err(invalid());
        }
    }
    Ok(count)
}
impl Atlas {
    fn find(self: &Arc<Self>, target: &[u8]) -> io::Result<SharedImage> {
        let bytes = self.mapping.as_ref();
        let records = bytes[HEADER..HEADER + self.count * RECORD]
            .as_chunks::<RECORD>()
            .0;
        let index = records
            .binary_search_by(|record| name(record).cmp(target))
            .map_err(|_| {
                io::Error::new(io::ErrorKind::NotFound, "system bitmap is not installed")
            })?;
        let record = &records[index];
        let (width, height) = (word(record, 96), word(record, 100));
        Ok(SharedImage {
            atlas: self.clone(),
            record: index,
            width: width as i32,
            height: height as i32,
        })
    }
}
/// 系统目录只能从图集取数，其他用户/应用图片不属于此格式的职责。
pub(super) fn load(path: &Path) -> io::Result<Option<SharedImage>> {
    if path.parent() != Some(Path::new(DIRECTORY)) {
        return Ok(None);
    }
    let target = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(invalid)?;
    let atlas = {
        let mut weak = ATLAS.lock().expect("bitmap atlas poisoned");
        if let Some(atlas) = weak.upgrade() {
            atlas
        } else {
            // SAFETY: 产品图集是只读安装资源；更新通过原子 rename 替换，不修改仍被映射的 inode。
            let mapping =
                unsafe { ReadOnlyMapping::open(&Path::new(DIRECTORY).join("bitmaps.atlas")) }?;
            let count = validate(mapping.as_ref())?;
            let atlas = Arc::new(Atlas { mapping, count });
            *weak = Arc::downgrade(&atlas);
            atlas
        }
    };
    atlas.find(target.as_bytes()).map(Some)
}
