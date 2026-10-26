//! TrueType 字节码 hinting 解释器（Apple TrueType Reference Manual 与 OpenType 指令集规范）。
//!
//! 执行模型与 Windows 2000 时代的 TrueType 光栅器一致（GETINFO 报告版本 35）：
//! 1. 每个字体执行一次 `fpgm`，登记函数与指令定义。
//! 2. 每个 ppem 把 CVT 缩放到 26.6 像素后执行 `prep`，得到该字号的 CVT、存储区与默认图形状态。
//! 3. 每个字形加入四个幻影点后执行字形指令；复合字形先逐个组件 hinting，再执行复合字形自身的指令。
//!
//! 坐标为 26.6 定点像素，向量为 2.14 定点。任何越界、栈错误或超出步数上限都使本次程序失败，
//! 调用者退回未经 hinting 的轮廓；解释器从不 panic。
mod interpreter;

use crate::sfnt::{Component, GlyphSource, HintTables, Outline, Point, Sfnt};
use interpreter::Machine;
use std::{collections::BTreeMap, rc::Rc};

/// 单个程序最多执行的指令数；防止恶意或损坏的字体用跳转形成死循环。
const STEP_LIMIT: u32 = 1_000_000;
/// 函数调用嵌套上限。
const CALL_DEPTH: u32 = 64;
/// 复合字形递归上限，与未 hinting 的轮廓路径一致。
const COMPOSITE_DEPTH: u32 = 6;

type Fail = ();
type Exec<T> = Result<T, Fail>;

/// 字体级状态：fpgm 定义的函数与指令；与字号无关。
pub(crate) struct FontProgram {
    functions: Vec<Option<Rc<[u8]>>>,
    instructions: BTreeMap<u8, Rc<[u8]>>,
}

/// 某个 ppem 的 prep 结果。
pub(crate) struct SizeProgram {
    ppem: u32,
    /// 字体单位到 26.6 像素的 16.16 定点缩放：`FT_DivFix(ppem × 64, unitsPerEm)`。
    scale: i64,
    cvt: Vec<i32>,
    storage: Vec<i32>,
    state: GraphicsState,
    /// INSTCTRL 第 1 位：禁止执行字形指令。
    glyphs_disabled: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Round {
    Grid,
    HalfGrid,
    DoubleGrid,
    DownToGrid,
    UpToGrid,
    Off,
    Super,
    Super45,
}

#[derive(Clone, Copy)]
struct GraphicsState {
    rp: [usize; 3],
    zp: [usize; 3],
    freedom: (i32, i32),
    projection: (i32, i32),
    dual: (i32, i32),
    loop_count: i32,
    minimum_distance: i32,
    round: Round,
    period: i32,
    phase: i32,
    threshold: i32,
    cvt_cutin: i32,
    single_width_cutin: i32,
    single_width: i32,
    delta_base: i32,
    delta_shift: i32,
    auto_flip: bool,
    instruct_control: i32,
    /// SCANCTRL 判定的丢点控制开关与 SCANTYPE 规则；决定单色扫描转换是否补点。
    scan_control: bool,
    scan_type: i32,
}

impl Default for GraphicsState {
    /// TrueType 规范的图形状态默认值。
    fn default() -> Self {
        Self {
            rp: [0; 3],
            zp: [1; 3],
            freedom: (0x4000, 0),
            projection: (0x4000, 0),
            dual: (0x4000, 0),
            loop_count: 1,
            minimum_distance: 64,
            round: Round::Grid,
            period: 64,
            phase: 0,
            threshold: 32,
            cvt_cutin: 68,
            single_width_cutin: 0,
            single_width: 0,
            delta_base: 9,
            delta_shift: 3,
            auto_flip: true,
            instruct_control: 0,
            scan_control: false,
            scan_type: 0,
        }
    }
}

/// 一个点区：0 为 twilight，1 为字形点（含幻影点）。
#[derive(Clone, Default)]
struct Zone {
    /// 未缩放的字体单位坐标（FreeType 的 orus）：原始距离与 IUP 插值用它投影后再缩放，精度高于取整后的原始坐标。
    units: Vec<(i32, i32)>,
    original: Vec<(i32, i32)>,
    current: Vec<(i32, i32)>,
    /// bit0：x 方向已触碰，bit1：y 方向已触碰。
    touched: Vec<u8>,
    on_curve: Vec<bool>,
    ends: Vec<usize>,
}

impl Zone {
    fn with_len(len: usize) -> Self {
        Self {
            units: vec![(0, 0); len],
            original: vec![(0, 0); len],
            current: vec![(0, 0); len],
            touched: vec![0; len],
            on_curve: vec![true; len],
            ends: Vec::new(),
        }
    }
}

/// FreeType `FT_MulFix`：a × b / 65536，四舍五入（远离零）。
fn mul_fix(a: i32, b: i64) -> i32 {
    let product = i64::from(a) * b;
    let rounded = if product >= 0 {
        (product + 0x8000) >> 16
    } else {
        -((-product + 0x8000) >> 16)
    };
    rounded.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

/// FreeType `FT_DivFix`：a × 65536 / b，四舍五入；除数为零时取最大值。
fn div_fix(a: i64, b: i64) -> i64 {
    if b == 0 {
        return if a >= 0 {
            i64::from(i32::MAX)
        } else {
            i64::from(i32::MIN)
        };
    }
    let (numerator, denominator) = ((a.abs() << 16) + b.abs() / 2, b.abs());
    let quotient = numerator / denominator;
    if (a < 0) != (b < 0) {
        -quotient
    } else {
        quotient
    }
}

/// FreeType `TT_DotFix14`：(ax·bx + ay·by) 的 2.14 定点积，四舍五入。
fn dot14(ax: i32, ay: i32, (bx, by): (i32, i32)) -> i32 {
    let sum = i64::from(ax) * i64::from(bx) + i64::from(ay) * i64::from(by);
    let rounded = if sum >= 0 {
        (sum + 0x2000) >> 14
    } else {
        -((-sum + 0x2000) >> 14)
    };
    rounded.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

fn round_half_even_free(value: f64) -> i32 {
    value.round() as i32
}

fn mul_div(a: i32, b: i32, c: i32) -> i32 {
    if c == 0 {
        return if (a >= 0) == (b >= 0) {
            i32::MAX
        } else {
            i32::MIN
        };
    }
    let product = i64::from(a) * i64::from(b);
    let half = i64::from(c.abs()) / 2;
    let rounded = if (product >= 0) == (c > 0) {
        (product + half * i64::from(c.signum())) / i64::from(c)
    } else {
        (product - half * i64::from(c.signum())) / i64::from(c)
    };
    rounded.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

/// 2.14 向量与 26.6 距离的乘积，结果为 26.6。
fn mul14(value: i32, vector: i32) -> i32 {
    ((i64::from(value) * i64::from(vector) + 0x2000) >> 14) as i32
}

/// 把 26.6 方向归一化为 2.14 单位向量；零向量为 x 轴。
fn normalize(dx: i32, dy: i32) -> (i32, i32) {
    let length = f64::from(dx).hypot(f64::from(dy));
    if length == 0.0 {
        return (0x4000, 0);
    }
    (
        round_half_even_free(f64::from(dx) * 16384.0 / length),
        round_half_even_free(f64::from(dy) * 16384.0 / length),
    )
}

impl FontProgram {
    /// 执行 `fpgm`；失败表示该字体不可 hinting。
    pub(crate) fn new(font: &Sfnt) -> Option<Self> {
        let tables = font.hint_tables();
        let mut program = Self {
            functions: vec![None; tables.max_function_defs],
            instructions: BTreeMap::new(),
        };
        let mut storage = vec![0; tables.max_storage];
        let mut cvt = vec![0; tables.cvt.len()];
        let mut zones = [Zone::with_len(tables.max_twilight), Zone::default()];
        let mut machine = Machine {
            stack: Vec::with_capacity(tables.max_stack.max(32)),
            max_stack: tables.max_stack.max(32) + 32,
            state: GraphicsState::default(),
            cvt: &mut cvt,
            storage: &mut storage,
            zones: &mut zones,
            program: &mut program,
            ppem: 0,
            scale: 0,
            steps: 0,
            smoothing: false,
            in_prep: false,
        };
        machine.run(tables.fpgm, 0).ok()?;
        Some(program)
    }

    /// 为 `ppem` 缩放 CVT 并执行 `prep`；失败表示该字号退回未 hinting 的轮廓。
    pub(crate) fn size(&mut self, font: &Sfnt, ppem: u32, smoothing: bool) -> Option<SizeProgram> {
        let tables: HintTables<'_> = font.hint_tables();
        let scale = div_fix(i64::from(ppem) * 64, i64::from(font.units_per_em()));
        let mut cvt: Vec<i32> = tables
            .cvt
            .iter()
            .map(|value| mul_fix(i32::from(*value), scale))
            .collect();
        let mut storage = vec![0; tables.max_storage];
        let mut zones = [Zone::with_len(tables.max_twilight), Zone::default()];
        let mut machine = Machine {
            stack: Vec::with_capacity(tables.max_stack.max(32)),
            max_stack: tables.max_stack.max(32) + 32,
            state: GraphicsState::default(),
            cvt: &mut cvt,
            storage: &mut storage,
            zones: &mut zones,
            program: self,
            ppem,
            scale,
            steps: 0,
            smoothing,
            in_prep: true,
        };
        machine.run(tables.prep, 0).ok()?;
        let mut state = machine.state;
        // prep 只能改变图形状态的默认值；以下字段每个字形从规范默认开始。
        let defaults = GraphicsState::default();
        state.rp = defaults.rp;
        state.zp = defaults.zp;
        state.freedom = defaults.freedom;
        state.projection = defaults.projection;
        state.dual = defaults.dual;
        state.loop_count = 1;
        Some(SizeProgram {
            ppem,
            scale,
            glyphs_disabled: state.instruct_control & 1 != 0,
            cvt,
            storage,
            state,
        })
    }
}

/// hinting 后的字形：像素坐标轮廓（y 向上，原点为对齐后的 pp1）与前进量（像素）。
pub(crate) struct Hinted {
    pub outline: Outline,
    pub advance: i32,
    /// 字体指令选择的丢点控制：SCANCTRL 判定为开时，SCANTYPE 0/4 含残桩、1/5 排除残桩。
    pub dropout: crate::raster::Dropout,
}

/// 对一个字形执行 hinting。程序失败返回 None，调用者退回未 hinting 的轮廓。
pub(crate) fn hint_glyph(
    font: &Sfnt,
    program: &mut FontProgram,
    size: &SizeProgram,
    glyph: u16,
    smoothing: bool,
) -> Option<Hinted> {
    let tables = font.hint_tables();
    let mut twilight = Zone::with_len(tables.max_twilight);
    let mut dropout = (size.state.scan_control, size.state.scan_type);
    let mut zone = load(
        font,
        program,
        size,
        glyph,
        smoothing,
        0,
        &mut twilight,
        tables.max_stack,
        &mut dropout,
    )?;
    let count = zone.current.len();
    if count < 4 {
        return None;
    }
    let origin = zone.current[count - 4].0;
    let advance = zone.current[count - 3].0 - origin;
    let mut outline = Outline::default();
    let mut start = 0;
    for &end in &zone.ends {
        let contour = (start..=end)
            .map(|index| Point {
                x: (zone.current[index].0 - origin) as f32 / 64.0,
                y: zone.current[index].1 as f32 / 64.0,
                on_curve: zone.on_curve[index],
            })
            .collect();
        outline.contours.push(contour);
        start = end + 1;
    }
    zone.ends.clear();
    Some(Hinted {
        outline,
        advance: (advance + 32).div_euclid(64),
        dropout: match dropout {
            (true, 0 | 4) => crate::raster::Dropout::Stubs,
            (true, 1 | 5) => crate::raster::Dropout::NoStubs,
            _ => crate::raster::Dropout::Off,
        },
    })
}

/// 载入并 hinting 一个字形，返回含四个幻影点的点区。
#[allow(clippy::too_many_arguments)]
fn load(
    font: &Sfnt,
    program: &mut FontProgram,
    size: &SizeProgram,
    glyph: u16,
    smoothing: bool,
    depth: u32,
    twilight: &mut Zone,
    max_stack: usize,
    dropout: &mut (bool, i32),
) -> Option<Zone> {
    if depth > COMPOSITE_DEPTH {
        return None;
    }
    let scale = |value: i32| mul_fix(value, size.scale);
    let advance = i32::from(font.advance_units(glyph)?);
    let lsb = i32::from(font.left_side_bearing(glyph)?);
    let (ascent, descent) = font.vertical_metrics();
    let source = font.glyph_source(glyph)?;
    let x_min = match &source {
        GlyphSource::Empty => lsb,
        GlyphSource::Simple { x_min, .. } | GlyphSource::Composite { x_min, .. } => *x_min,
    };
    let phantom_x = x_min - lsb;
    let phantom_units = [
        (phantom_x, 0),
        (phantom_x + advance, 0),
        (0, i32::from(ascent)),
        (0, i32::from(descent)),
    ];
    let (mut zone, instructions) = match source {
        GlyphSource::Empty => (Zone::default(), Vec::new()),
        GlyphSource::Simple { glyph, .. } => {
            let mut zone = Zone::with_len(glyph.points.len());
            for (index, &(x, y, on)) in glyph.points.iter().enumerate() {
                zone.units[index] = (x, y);
                zone.original[index] = (scale(x), scale(y));
                zone.on_curve[index] = on;
            }
            zone.current = zone.original.clone();
            zone.ends = glyph.ends;
            (zone, glyph.instructions)
        }
        GlyphSource::Composite {
            components,
            instructions,
            ..
        } => (
            composite(
                font,
                program,
                size,
                &components,
                smoothing,
                depth,
                twilight,
                max_stack,
                dropout,
            )?,
            instructions,
        ),
    };
    for units in phantom_units {
        let point = (scale(units.0), scale(units.1));
        zone.units.push(units);
        zone.original.push(point);
        zone.current.push(point);
        zone.touched.push(0);
        zone.on_curve.push(true);
    }
    let count = zone.current.len();
    // pp1 对齐到整像素：整个字形随之平移，前进量端点 pp2 也取整。
    let shift = ((zone.current[count - 4].0 + 32) & !63) - zone.current[count - 4].0;
    for point in zone.current.iter_mut() {
        point.0 += shift;
    }
    zone.current[count - 3].0 = (zone.current[count - 3].0 + 32) & !63;
    if instructions.is_empty() || size.glyphs_disabled {
        return Some(zone);
    }
    let mut cvt = size.cvt.clone();
    let mut storage = size.storage.clone();
    let mut zones = [std::mem::take(twilight), zone];
    let mut machine = Machine {
        stack: Vec::with_capacity(max_stack.max(32)),
        max_stack: max_stack.max(32) + 32,
        state: size.state,
        cvt: &mut cvt,
        storage: &mut storage,
        zones: &mut zones,
        program,
        ppem: size.ppem,
        scale: size.scale,
        steps: 0,
        smoothing,
        in_prep: false,
    };
    let result = machine.run(&instructions, 0);
    *dropout = (machine.state.scan_control, machine.state.scan_type);
    let [returned_twilight, zone] = zones;
    *twilight = returned_twilight;
    result.ok()?;
    Some(zone)
}

/// 组装复合字形：逐个组件 hinting 后按偏移（或点匹配）放置，去掉组件自己的幻影点。
#[allow(clippy::too_many_arguments)]
fn composite(
    font: &Sfnt,
    program: &mut FontProgram,
    size: &SizeProgram,
    components: &[Component],
    smoothing: bool,
    depth: u32,
    twilight: &mut Zone,
    max_stack: usize,
    dropout: &mut (bool, i32),
) -> Option<Zone> {
    let mut zone = Zone::default();
    for component in components {
        let mut child = load(
            font,
            program,
            size,
            component.glyph,
            smoothing,
            depth + 1,
            twilight,
            max_stack,
            dropout,
        )?;
        let keep = child.current.len().checked_sub(4)?;
        child.units.truncate(keep);
        child.original.truncate(keep);
        child.current.truncate(keep);
        child.touched.truncate(keep);
        child.on_curve.truncate(keep);
        let [a, b, c, d] = component.matrix;
        if (a, b, c, d) != (1.0, 0.0, 0.0, 1.0) {
            let transform = |point: &mut (i32, i32)| {
                let (x, y) = (f64::from(point.0), f64::from(point.1));
                *point = (
                    round_half_even_free(f64::from(a) * x + f64::from(c) * y),
                    round_half_even_free(f64::from(b) * x + f64::from(d) * y),
                );
            };
            child.units.iter_mut().for_each(transform);
            for point in child.original.iter_mut().chain(child.current.iter_mut()) {
                let (x, y) = (f64::from(point.0), f64::from(point.1));
                *point = (
                    round_half_even_free(f64::from(a) * x + f64::from(c) * y),
                    round_half_even_free(f64::from(b) * x + f64::from(d) * y),
                );
            }
        }
        let unit_offset = if component.flags & 2 != 0 {
            component.arguments
        } else {
            (0, 0)
        };
        let (dx, dy) = if component.flags & 2 != 0 {
            let scale = |value: i32| mul_fix(value, size.scale);
            let (mut dx, mut dy) = (scale(component.arguments.0), scale(component.arguments.1));
            // ROUND_XY_TO_GRID：组件偏移取整到像素。
            if component.flags & 4 != 0 {
                dx = (dx + 32) & !63;
                dy = (dy + 32) & !63;
            }
            (dx, dy)
        } else {
            // 点匹配：父字形已放置的第 arg1 点与子字形第 arg2 点重合。
            let parent = *zone
                .current
                .get(usize::try_from(component.arguments.0).ok()?)?;
            let own = *child
                .current
                .get(usize::try_from(component.arguments.1).ok()?)?;
            (parent.0 - own.0, parent.1 - own.1)
        };
        let base = zone.current.len();
        for index in 0..child.current.len() {
            zone.units.push((
                child.units[index].0 + unit_offset.0,
                child.units[index].1 + unit_offset.1,
            ));
            zone.original
                .push((child.original[index].0 + dx, child.original[index].1 + dy));
            zone.current
                .push((child.current[index].0 + dx, child.current[index].1 + dy));
            zone.touched.push(0);
            zone.on_curve.push(child.on_curve[index]);
        }
        zone.ends.extend(child.ends.iter().map(|end| end + base));
    }
    Some(zone)
}
