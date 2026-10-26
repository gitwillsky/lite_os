//! Windows 2000 GDI 式字体映射：Fonts 表来自字体目录扫描，FontSubstitutes 与 SystemLink 来自
//! `fontmap.ini`，逻辑字体只给出字体名、磅值与粗细，代码中不指定任何字体文件或字号。
use crate::{
    BdfFont, FontError, Glyph, Sfnt,
    hinting::{FontProgram, SizeProgram, hint_glyph},
    overprint, raster,
};
use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap},
    fs,
    path::{Path, PathBuf},
    rc::Rc,
};

/// 逻辑字体（`LOGFONT` 的子集）：字体名、磅值与粗细（400 常规，700 粗体）。
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LogicalFont {
    pub face: String,
    pub points: u16,
    pub weight: u16,
}

impl LogicalFont {
    /// 96 DPI × `scale` 下的 ppem：磅值 × 96 × scale / 72，四舍五入，与 GDI 的 `-MulDiv(pt, dpi, 72)` 一致。
    pub fn ppem(&self, scale: u32) -> u32 {
        (u32::from(self.points) * 96 * scale + 36) / 72
    }

    /// `FW_SEMIBOLD`（600）及以上按粗体选择或合成。
    pub fn bold(&self) -> bool {
        self.weight >= 600
    }
}

/// 一个字体成员：TrueType 轮廓（可带内嵌点阵）或固定像素的 BDF 点阵。
enum Face {
    Outline(Box<OutlineFace>),
    Bitmap(BdfFont),
}

/// TrueType 成员与它的 fpgm 执行结果。
struct OutlineFace {
    font: Sfnt,
    // OWNER: 每个字体成员只执行一次 fpgm；prep/字形中的 FDEF/IDEF 也写回这里。
    // FAILURE: 不缓存会在每个字号重复执行 fpgm；None 表示字体没有 hinting 程序或 fpgm 失败。
    program: RefCell<Option<Option<FontProgram>>>,
}

impl OutlineFace {
    /// 字体的 fpgm 程序；第一次使用时执行。
    fn program(&self) -> std::cell::RefMut<'_, Option<FontProgram>> {
        let mut program = self.program.borrow_mut();
        if program.is_none() {
            *program = Some(if self.font.hinted() {
                FontProgram::new(&self.font)
            } else {
                None
            });
        }
        std::cell::RefMut::map(program, |program| program.as_mut().expect("initialized"))
    }
}

impl Face {
    fn named(&self, name: &str) -> bool {
        match self {
            Face::Outline(face) => face
                .font
                .families()
                .iter()
                .any(|family| family.eq_ignore_ascii_case(name)),
            Face::Bitmap(font) => font.family().eq_ignore_ascii_case(name),
        }
    }

    fn is_bold(&self) -> bool {
        match self {
            Face::Outline(face) => face.font.is_bold(),
            Face::Bitmap(font) => font.is_bold(),
        }
    }

    /// 该成员在 `ppem` 下能否使用：点阵只在像素尺寸相等时可用。
    fn usable(&self, ppem: u32) -> bool {
        match self {
            Face::Outline(_) => true,
            Face::Bitmap(font) => font.pixel_size() == ppem,
        }
    }

    /// GDI 的 (tmAscent, tmDescent)：TrueType 取 OS/2 winAscent/winDescent 按 ppem 缩放取整。
    fn metrics(&self, ppem: u32) -> (i32, i32) {
        match self {
            Face::Bitmap(font) => (font.ascent(), font.descent()),
            Face::Outline(face) => {
                let font = &face.font;
                let (ascent, descent) = font.windows_metrics();
                let scale = ppem as f32 / f32::from(font.units_per_em());
                (
                    (f32::from(ascent) * scale).round() as i32,
                    (f32::from(descent) * scale).round() as i32,
                )
            }
        }
    }

    /// 该 ppem 的内嵌点阵字形（BDF 或 EBLC/EBDT）；`synthesize` 时按 GDI 合成粗体。
    fn bitmap(&self, character: char, ppem: u32, synthesize: bool) -> Option<Glyph> {
        let glyph = match self {
            Face::Bitmap(font) => font.glyph(character)?,
            Face::Outline(face) => {
                let font = &face.font;
                if !font.has_strike(ppem) {
                    return None;
                }
                font.embedded_bitmap(font.glyph_id(character)?, ppem)?
            }
        };
        Some(if synthesize {
            overprint(glyph, 1)
        } else {
            glyph
        })
    }

    /// 轮廓字形：给出 `size`（该字体在本 ppem 的 prep 结果）时先执行 TrueType 指令，失败或未给出则用未 hinting 的轮廓；
    /// 平滑关闭时按单色扫描转换，开启时为灰度覆盖率。
    fn outline(
        &self,
        character: char,
        ppem: u32,
        synthesize: bool,
        smoothing: bool,
        size: Option<&SizeProgram>,
    ) -> Option<Glyph> {
        let Face::Outline(face) = self else {
            return None;
        };
        let font = &face.font;
        let id = font.glyph_id(character)?;
        let strength = if synthesize { ppem as f32 / 24.0 } else { 0.0 };
        let hinted = size.and_then(|size| {
            let mut program = face.program();
            hint_glyph(font, program.as_mut()?, size, id, smoothing)
        });
        // 平滑关闭时按 TrueType 单色扫描转换（像素中心 + 字体指定的丢点控制），否则灰度覆盖率。
        let draw = |outline: &crate::sfnt::Outline, scale: f32, dropout: raster::Dropout| {
            if smoothing {
                // 合成粗体的外扩已超过加深量，不再叠加。
                let darkening = if synthesize { 0.0 } else { STEM_DARKENING };
                raster::rasterize(outline, scale, strength / 2.0 + darkening)
            } else {
                raster::rasterize_mono(outline, scale, strength / 2.0, dropout)
            }
        };
        let (raster, advance) = match hinted {
            Some(hinted) => (
                draw(&hinted.outline, 1.0, hinted.dropout),
                hinted.advance + strength.round() as i32,
            ),
            None => {
                let scale = ppem as f32 / f32::from(font.units_per_em());
                (
                    draw(&font.outline(id)?, scale, raster::Dropout::Off),
                    (f32::from(font.advance_units(id)?) * scale + strength).round() as i32,
                )
            }
        };
        let glyph = Glyph {
            coverage: raster.coverage,
            width: raster.width,
            height: raster.height,
            left: raster.left,
            top: raster.top,
            advance,
        };
        Some(glyph)
    }
}

/// 字体目录：已安装字体成员与映射表。一个进程只需加载一次，解析结果由调用者缓存。
pub struct FontCatalog {
    faces: Vec<Rc<Face>>,
    /// `[FontSubstitutes]`：请求的字体名未安装时改用的字体名。
    substitutes: BTreeMap<String, String>,
    /// `[SystemLink]`：字体缺字时依次查找的字体名（Win2000 FontLink）。
    links: BTreeMap<String, Vec<String>>,
}

fn key(name: &str) -> String {
    name.trim().to_lowercase()
}

impl FontCatalog {
    /// 扫描 `directory` 中的 `.ttf`、`.ttc` 与 `.bdf`，读取同目录的 `fontmap.ini`（可缺省）。
    ///
    /// # Errors
    ///
    /// 目录不可读，或其中没有任何可解析的字体。单个损坏的字体文件被跳过，与 GDI 忽略无效字体一致。
    pub fn load(directory: &Path) -> Result<Self, FontError> {
        let io = |path: &Path, source| FontError::Io {
            path: path.display().to_string(),
            source,
        };
        let mut paths: Vec<PathBuf> = fs::read_dir(directory)
            .map_err(|error| io(directory, error))?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .collect();
        paths.sort();
        let mut faces = Vec::new();
        for path in paths {
            let extension = path
                .extension()
                .map(|value| value.to_string_lossy().to_lowercase());
            match extension.as_deref() {
                Some("ttf" | "ttc") => {
                    let data = fs::read(&path).map_err(|error| io(&path, error))?;
                    if let Ok(members) = Sfnt::parse_collection(data) {
                        faces.extend(members.into_iter().map(|font| {
                            Rc::new(Face::Outline(Box::new(OutlineFace {
                                font,
                                program: RefCell::new(None),
                            })))
                        }));
                    }
                }
                Some("bdf") => {
                    let data = fs::read(&path).map_err(|error| io(&path, error))?;
                    if let Ok(font) = BdfFont::parse(&String::from_utf8_lossy(&data)) {
                        faces.push(Rc::new(Face::Bitmap(font)));
                    }
                }
                _ => {}
            }
        }
        if faces.is_empty() {
            return Err(FontError::Unsupported("font directory has no usable fonts"));
        }
        let mapping = directory.join("fontmap.ini");
        let text = match fs::read_to_string(&mapping) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(io(&mapping, error)),
        };
        let (mut substitutes, mut links) = (BTreeMap::new(), BTreeMap::new());
        let mut section = String::new();
        for line in text.lines().map(str::trim) {
            if line.is_empty() || line.starts_with(';') {
                continue;
            }
            if let Some(name) = line
                .strip_prefix('[')
                .and_then(|rest| rest.strip_suffix(']'))
            {
                section = name.to_owned();
                continue;
            }
            let Some((name, value)) = line.split_once('=') else {
                continue;
            };
            match section.as_str() {
                "FontSubstitutes" => {
                    substitutes.insert(key(name), value.trim().to_owned());
                }
                "SystemLink" => {
                    links.insert(
                        key(name),
                        value
                            .split(',')
                            .map(|item| item.trim().to_owned())
                            .filter(|item| !item.is_empty())
                            .collect(),
                    );
                }
                _ => {}
            }
        }
        Ok(Self {
            faces,
            substitutes,
            links,
        })
    }

    fn installed(&self, name: &str) -> bool {
        self.faces.iter().any(|face| face.named(name))
    }

    /// 把逻辑字体解析为可绘制的字体。
    ///
    /// 1. 字体名未安装时按 FontSubstitutes 替换（可多级，最多 8 次防止环）。
    /// 2. 解析出的字体后接其 SystemLink 链，逐字取第一个有该字形且在此 ppem 可用的成员。
    /// 3. 链中没有任何已安装字体时退到目录中的第一个字体，保证文字仍可显示。
    pub fn resolve(&self, font: &LogicalFont, scale: u32, smoothing: bool) -> Typeface {
        let mut name = font.face.clone();
        for _ in 0..8 {
            if self.installed(&name) {
                break;
            }
            match self.substitutes.get(&key(&name)) {
                Some(next) => name = next.clone(),
                None => break,
            }
        }
        let mut names = vec![name.clone()];
        if let Some(linked) = self
            .links
            .get(&key(&name))
            .or_else(|| self.links.get(&key(&font.face)))
        {
            names.extend(linked.iter().cloned());
        }
        let mut chain: Vec<Member> = Vec::new();
        for name in &names {
            let members: Vec<_> = self.faces.iter().filter(|face| face.named(name)).collect();
            let regular = members
                .iter()
                .find(|face| !face.is_bold())
                .or_else(|| members.first());
            if let Some(regular) = regular {
                chain.push(Member {
                    regular: Rc::clone(regular),
                    bold: members
                        .iter()
                        .find(|face| face.is_bold())
                        .map(|face| Rc::clone(face)),
                });
            }
        }
        if chain.is_empty() {
            chain.push(Member {
                regular: Rc::clone(&self.faces[0]),
                bold: None,
            });
        }
        Typeface {
            ppem: font.ppem(scale.max(1)),
            bold: font.bold(),
            smoothing,
            chain,
            sizes: RefCell::new(BTreeMap::new()),
            glyphs: RefCell::new(HashMap::new()),
        }
    }
}

/// prep 缓存的键：成员在本进程中的地址（chain 中的 Rc 保证存活期间不复用）。
fn key_of(face: &Rc<Face>) -> usize {
    Rc::as_ptr(face) as usize
}

struct Member {
    regular: Rc<Face>,
    bold: Option<Rc<Face>>,
}

/// 解析后的逻辑字体：固定 ppem 与粗细，按 SystemLink 链取字形。
pub struct Typeface {
    ppem: u32,
    /// 逻辑字体本身的粗细；绘制时的 `bold` 参数可再要求粗体。
    bold: bool,
    smoothing: bool,
    chain: Vec<Member>,
    // OWNER: 本字号独占各轮廓成员的 prep 结果，按成员地址索引；成员由 chain 中的 Rc 保活。
    // FAILURE: 不缓存会在每个字形重复执行 prep；None 记录该字号不可 hinting，避免反复失败。
    sizes: RefCell<BTreeMap<usize, Option<SizeProgram>>>,
    // OWNER: 本字号独占已光栅化字形，键为（字符，是否粗体）。
    // FAILURE: 不缓存会在每次绘制重新执行字形指令与光栅化；超过上限时整体清空，防止内存无界增长。
    glyphs: RefCell<HashMap<(char, bool), Glyph>>,
}

/// 灰度平滑的笔画加深（每侧像素）：宋体等发丝笔画在 24 ppem 只有约 0.5 像素厚，灰度下会变成半灰而发虚；
/// 每侧外扩 0.25 像素使细笔画接近 1 像素实色（与 FreeType/macOS 的 stem darkening 同类），粗笔画只略加深。
/// 平滑时仍执行 hinting：横竖笔画先对齐像素网格，加深后核心为实色。
const STEM_DARKENING: f32 = 0.25;

/// 字形缓存上限；常见界面文字远少于此。
const GLYPH_CACHE_LIMIT: usize = 4096;

impl Typeface {
    /// 字体的像素尺寸（ppem）。
    pub fn ppem(&self) -> u32 {
        self.ppem
    }

    /// 取 `character` 的字形；链中都没有时返回半个 em 宽的空白，不会失败。
    ///
    /// 逐个链接字体：有该 ppem 的点阵即采用，否则采用第一个有该字形的轮廓（带 hinting）。
    pub fn glyph(&self, character: char, bold: bool) -> Glyph {
        let bold = bold || self.bold;
        if let Some(glyph) = self.glyphs.borrow().get(&(character, bold)) {
            return glyph.clone();
        }
        let glyph = self.render(character, bold);
        let mut cache = self.glyphs.borrow_mut();
        if cache.len() >= GLYPH_CACHE_LIMIT {
            cache.clear();
        }
        cache.insert((character, bold), glyph.clone());
        glyph
    }

    fn render(&self, character: char, bold: bool) -> Glyph {
        for member in &self.chain {
            let face = if bold {
                member.bold.as_ref().unwrap_or(&member.regular)
            } else {
                &member.regular
            };
            if !face.usable(self.ppem) {
                continue;
            }
            let synthesize = bold && !face.is_bold();
            if let Some(glyph) = face.bitmap(character, self.ppem, synthesize) {
                return glyph;
            }
            // 平滑开启时仍先 hinting：横竖笔画对齐像素网格后保持实色，只有斜笔画与边缘是灰度；
            // 不 hinting 时 1 像素笔画跨两行像素被平均成灰色而发虚。不读 gasp 的禁灰区间（清晰度优先，见 assets.md）。
            let size = self.size(face);
            let size = size
                .as_ref()
                .and_then(|sizes| sizes.get(&key_of(face))?.as_ref());
            if let Some(glyph) =
                face.outline(character, self.ppem, synthesize, self.smoothing, size)
            {
                return glyph;
            }
        }
        Glyph {
            coverage: Vec::new(),
            width: 0,
            height: 0,
            left: 0,
            top: 0,
            advance: (self.ppem / 2) as i32,
        }
    }

    /// 该成员在本 ppem 的 prep 结果；第一次使用时执行。
    fn size(
        &self,
        face: &Rc<Face>,
    ) -> Option<std::cell::Ref<'_, BTreeMap<usize, Option<SizeProgram>>>> {
        let Face::Outline(outline) = face.as_ref() else {
            return None;
        };
        if !self.sizes.borrow().contains_key(&key_of(face)) {
            let size = outline
                .program()
                .as_mut()
                .and_then(|program| program.size(&outline.font, self.ppem, self.smoothing));
            self.sizes.borrow_mut().insert(key_of(face), size);
        }
        Some(self.sizes.borrow())
    }

    /// `(ascent, descent)` 像素，descent 为正；取链中首个在此 ppem 可用的成员（GDI 的字体链接不改变度量）。
    pub fn metrics(&self) -> (i32, i32) {
        self.chain
            .iter()
            .map(|member| &member.regular)
            .find(|face| face.usable(self.ppem))
            .map_or((self.ppem as i32, 0), |face| face.metrics(self.ppem))
    }

    /// 文本的像素宽度；不处理换行符（遇到 `\n` 按零宽计）。
    pub fn measure(&self, text: &str, bold: bool) -> i32 {
        text.chars()
            .filter(|c| *c != '\n')
            .map(|c| self.glyph(c, bold).advance)
            .sum()
    }
}
