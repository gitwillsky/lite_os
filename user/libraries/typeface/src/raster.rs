//! 轮廓光栅化：二次贝塞尔展平，再用有符号面积累积得到 8 位覆盖率。

use crate::sfnt::{Outline, Point};

/// 展平容差，像素。
const FLATNESS: f32 = 0.05;

#[derive(Clone, Copy)]
struct Vertex {
    x: f32,
    y: f32,
}

fn midpoint(a: Point, b: Point) -> Vertex {
    Vertex {
        x: (a.x + b.x) / 2.0,
        y: (a.y + b.y) / 2.0,
    }
}

fn quadratic(start: Vertex, control: Vertex, end: Vertex, out: &mut Vec<Vertex>, depth: u32) {
    // 控制点到弦中点的距离的一半就是二次曲线的最大偏差。
    let deviation =
        ((start.x + end.x) / 2.0 - control.x).hypot((start.y + end.y) / 2.0 - control.y) / 2.0;
    if deviation <= FLATNESS || depth >= 8 {
        out.push(end);
        return;
    }
    let left = Vertex {
        x: (start.x + control.x) / 2.0,
        y: (start.y + control.y) / 2.0,
    };
    let right = Vertex {
        x: (control.x + end.x) / 2.0,
        y: (control.y + end.y) / 2.0,
    };
    let middle = Vertex {
        x: (left.x + right.x) / 2.0,
        y: (left.y + right.y) / 2.0,
    };
    quadratic(start, left, middle, out, depth + 1);
    quadratic(middle, right, end, out, depth + 1);
}

/// 把一个 TrueType 轮廓（点序列，相邻 off-curve 点之间隐含 on-curve 中点）展平成折线。
fn flatten(contour: &[Point], scale: f32) -> Vec<Vertex> {
    let at = |point: &Point| Point {
        x: point.x * scale,
        y: point.y * scale,
        ..*point
    };
    let points: Vec<Point> = contour.iter().map(at).collect();
    let count = points.len();
    let start_index = points.iter().position(|point| point.on_curve);
    // 没有 on-curve 点时，从第一对 off-curve 点的中点开始。
    let (start, first) = match start_index {
        Some(index) => (
            Vertex {
                x: points[index].x,
                y: points[index].y,
            },
            index + 1,
        ),
        None => (midpoint(points[count - 1], points[0]), 0),
    };
    let mut out = vec![start];
    let mut pending: Option<Vertex> = None;
    let mut current = start;
    for step in 0..count {
        let point = points[(first + step) % count];
        let vertex = Vertex {
            x: point.x,
            y: point.y,
        };
        match (point.on_curve, pending) {
            (true, None) => {
                out.push(vertex);
                current = vertex;
            }
            (true, Some(control)) => {
                quadratic(current, control, vertex, &mut out, 0);
                pending = None;
                current = vertex;
            }
            (false, None) => pending = Some(vertex),
            (false, Some(control)) => {
                let mid = Vertex {
                    x: (control.x + vertex.x) / 2.0,
                    y: (control.y + vertex.y) / 2.0,
                };
                quadratic(current, control, mid, &mut out, 0);
                current = mid;
                pending = Some(vertex);
            }
        }
    }
    if let Some(control) = pending {
        quadratic(current, control, start, &mut out, 0);
    }
    out
}

/// 把一个展平后的轮廓整体向外加粗 `distance` 像素（轮廓加粗，与 FreeType 的 `FT_Outline_Embolden` 同类）。
///
/// TrueType 外轮廓顺时针、填充区在行进方向右侧，所以每个顶点沿相邻两条边右法线 `(dy, -dx)` 的斜接方向外移：
/// 外轮廓向外长、内轮廓（孔）向内收，笔画因此均匀变粗。斜接长度限制在 `distance` 的 4 倍，避免尖角刺出。
fn embolden(contour: &mut Vec<Vertex>, distance: f32) {
    contour.dedup_by(|a, b| a.x == b.x && a.y == b.y);
    if contour.len() > 1
        && contour
            .first()
            .zip(contour.last())
            .is_some_and(|(a, b)| a.x == b.x && a.y == b.y)
    {
        contour.pop();
    }
    let count = contour.len();
    if count < 3 {
        return;
    }
    let normal = |from: Vertex, to: Vertex| {
        let (dx, dy) = (to.x - from.x, to.y - from.y);
        let length = dx.hypot(dy);
        (dy / length, -dx / length)
    };
    let moved: Vec<Vertex> = (0..count)
        .map(|index| {
            let (previous, current, next) = (
                contour[(index + count - 1) % count],
                contour[index],
                contour[(index + 1) % count],
            );
            let (ax, ay) = normal(previous, current);
            let (bx, by) = normal(current, next);
            let denominator = (1.0 + ax * bx + ay * by).max(0.25);
            Vertex {
                x: current.x + (ax + bx) * distance / denominator,
                y: current.y + (ay + by) * distance / denominator,
            }
        })
        .collect();
    *contour = moved;
}

/// 面积累积缓冲：每行 `width + 2` 个单元，行内前缀和即覆盖率。
struct Accumulator {
    cells: Vec<f32>,
    width: usize,
    height: usize,
    stride: usize,
}

impl Accumulator {
    fn add(&mut self, row: usize, column: i64, value: f32) {
        // 浮点误差或超出包围盒的点落在界外时丢弃，不影响界内累积。
        if row < self.height && column >= 0 && (column as usize) < self.stride {
            self.cells[row * self.stride + column as usize] += value;
        }
    }

    fn line(&mut self, from: Vertex, to: Vertex) {
        if from.y == to.y {
            return;
        }
        let (direction, top, bottom) = if from.y < to.y {
            (1.0, from, to)
        } else {
            (-1.0, to, from)
        };
        let slope = (bottom.x - top.x) / (bottom.y - top.y);
        let mut x = top.x;
        let mut y_start = top.y;
        if y_start < 0.0 {
            x -= y_start * slope;
            y_start = 0.0;
        }
        let last_row = (bottom.y.ceil().max(0.0) as usize).min(self.height);
        for row in y_start as usize..last_row {
            let dy = ((row + 1) as f32).min(bottom.y) - (row as f32).max(y_start);
            let next = x + slope * dy;
            let weight = dy * direction;
            let (low, high) = if x < next { (x, next) } else { (next, x) };
            let low_floor = low.floor();
            let (low_i, high_i) = (low_floor as i64, high.ceil() as i64);
            if high_i <= low_i + 1 {
                let mid = 0.5 * (x + next) - low_floor;
                self.add(row, low_i, weight - weight * mid);
                self.add(row, low_i + 1, weight * mid);
            } else {
                let inverse = 1.0 / (high - low);
                let low_fraction = low - low_floor;
                let first = 0.5 * inverse * (1.0 - low_fraction) * (1.0 - low_fraction);
                let high_fraction = high - high.ceil() + 1.0;
                let last = 0.5 * inverse * high_fraction * high_fraction;
                self.add(row, low_i, weight * first);
                if high_i == low_i + 2 {
                    self.add(row, low_i + 1, weight * (1.0 - first - last));
                } else {
                    let second = inverse * (1.5 - low_fraction);
                    self.add(row, low_i + 1, weight * (second - first));
                    for column in low_i + 2..high_i - 1 {
                        self.add(row, column, weight * inverse);
                    }
                    let third = second + (high_i - low_i - 3) as f32 * inverse;
                    self.add(row, high_i - 1, weight * (1.0 - third - last));
                }
                self.add(row, high_i, weight * last);
            }
            x = next;
        }
    }

    fn coverage(&self) -> Vec<u8> {
        let mut out = vec![0u8; self.width * self.height];
        for row in 0..self.height {
            let mut sum = 0.0f32;
            for column in 0..self.width {
                sum += self.cells[row * self.stride + column];
                out[row * self.width + column] = (sum.abs().min(1.0) * 255.0 + 0.5) as u8;
            }
        }
        out
    }
}

/// 栅格化结果：覆盖率位图及其相对笔位置的偏移。
pub(crate) struct Raster {
    pub coverage: Vec<u8>,
    pub width: i32,
    pub height: i32,
    /// 位图左边界相对于笔位置（原点）的像素偏移。
    pub left: i32,
    /// 位图上边界相对于基线的像素偏移，向上为正。
    pub top: i32,
}

/// 以 `scale`（像素 / font unit）栅格化 `outline`；空轮廓返回宽高为 0 的位图。
///
/// `embolden` 是每侧向外加粗的像素距离，0 表示不加粗。
/// 展平（可选加粗）后的轮廓折线与像素包围盒 (left, bottom, width, height)。
type Prepared = (Vec<Vec<Vertex>>, i32, i32, i32, i32);

/// 展平轮廓并求像素包围盒；空轮廓返回 None。
fn prepare(outline: &Outline, scale: f32, embolden_distance: f32) -> Option<Prepared> {
    let contours: Vec<Vec<Vertex>> = outline
        .contours
        .iter()
        .filter(|contour| contour.len() >= 2)
        .map(|contour| {
            let mut flat = flatten(contour, scale);
            if embolden_distance > 0.0 {
                embolden(&mut flat, embolden_distance);
            }
            flat
        })
        .collect();
    let (min_x, max_x, min_y, max_y) = contours.iter().flatten().fold(
        (f32::MAX, f32::MIN, f32::MAX, f32::MIN),
        |(min_x, max_x, min_y, max_y), v| {
            (
                min_x.min(v.x),
                max_x.max(v.x),
                min_y.min(v.y),
                max_y.max(v.y),
            )
        },
    );
    if min_x > max_x {
        return None;
    }
    let (left, right) = (min_x.floor() as i32, max_x.ceil() as i32);
    let (bottom, top) = (min_y.floor() as i32, max_y.ceil() as i32);
    Some((
        contours,
        left,
        bottom,
        (right - left).max(1),
        (top - bottom).max(1),
    ))
}

fn empty() -> Raster {
    Raster {
        coverage: Vec::new(),
        width: 0,
        height: 0,
        left: 0,
        top: 0,
    }
}

/// 扫描线与轮廓的一个交点：位置、环绕方向与所在线段（轮廓号，线段号）。
#[derive(Clone, Copy)]
struct Crossing {
    position: f32,
    winding: i32,
    segment: (usize, usize),
}

/// 单色扫描转换的丢点控制（TrueType SCANTYPE）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Dropout {
    Off,
    /// 规则 3/5：补点，包括残桩。
    Stubs,
    /// 规则 4/6：补点，但排除残桩。
    NoStubs,
}

/// 一条扫描线与轮廓的全部交点，按位置排序。
/// `along_y` 为真时扫描水平线 y = `line`，否则扫描竖线 x = `line`；端点按半开区间计入，避免重复计数。
fn crossings(contours: &[Vec<Vertex>], line: f32, along_y: bool) -> Vec<Crossing> {
    let mut out = Vec::new();
    for (contour_index, contour) in contours.iter().enumerate() {
        for index in 0..contour.len() {
            let (a, b) = (contour[index], contour[(index + 1) % contour.len()]);
            let (a0, a1, b0, b1) = if along_y {
                (a.y, a.x, b.y, b.x)
            } else {
                (a.x, a.y, b.x, b.y)
            };
            if a0 == b0 {
                continue;
            }
            let (low, high, winding) = if a0 < b0 { (a0, b0, 1) } else { (b0, a0, -1) };
            if line < low || line >= high {
                continue;
            }
            out.push(Crossing {
                position: a1 + (line - a0) * (b1 - a1) / (b0 - a0),
                winding,
                segment: (contour_index, index),
            });
        }
    }
    out.sort_by(|left, right| left.position.total_cmp(&right.position));
    out
}

/// 非零环绕规则下的填充区间：(进入交点, 离开交点)。
fn spans(crossings: &[Crossing]) -> Vec<(Crossing, Crossing)> {
    let mut out = Vec::new();
    let mut winding = 0;
    let mut start = None;
    for &crossing in crossings {
        let before = winding;
        winding += crossing.winding;
        if before == 0 && winding != 0 {
            start = Some(crossing);
        } else if before != 0
            && winding == 0
            && let Some(entry) = start
        {
            out.push((entry, crossing));
        }
    }
    out
}

/// 区间两端的线段沿轮廓经过一个折返点相连，且折返点落在本像素带 [line - 0.5, line + 0.5) 内：
/// 这是轮廓极值处的残桩（ftraster 的 stub 判定），排除残桩的丢点规则不为它补点。
fn stub(
    contours: &[Vec<Vertex>],
    entry: Crossing,
    exit: Crossing,
    line: f32,
    along_y: bool,
) -> bool {
    if entry.segment.0 != exit.segment.0 {
        return false;
    }
    let contour = &contours[entry.segment.0];
    let count = contour.len();
    let coordinate = |vertex: Vertex| if along_y { vertex.y } else { vertex.x };
    let walk = |from: usize, to: usize| {
        // 从 from 线段的终点走到 to 线段的起点，途经的顶点都必须在扫描线同一侧。
        let mut index = (from + 1) % count;
        let mut extreme: Option<f32> = None;
        let mut side = 0.0f32;
        for _ in 0..count {
            let value = coordinate(contour[index]) - line;
            if side == 0.0 {
                side = value.signum();
            } else if value.signum() != side && value != 0.0 {
                return None;
            }
            extreme = Some(extreme.map_or(value, |current: f32| {
                if value.abs() > current.abs() {
                    value
                } else {
                    current
                }
            }));
            if index == to {
                return extreme;
            }
            index = (index + 1) % count;
        }
        None
    };
    let (a, b) = (entry.segment.1, exit.segment.1);
    [walk(a, b), walk(b, a)]
        .into_iter()
        .flatten()
        .any(|extreme| extreme.abs() < 0.5)
}

/// TrueType 单色扫描转换：像素中心在轮廓内即点亮（规则 1）；开启丢点控制时在横竖两个方向补点：
/// 区间内没有任何像素中心时点亮区间左侧（纵向为下方）的像素，两侧相邻像素任一已亮则不补。
/// 规则与 FreeType ftraster（v35 单色）逐像素一致，而不是对灰度覆盖率取阈值（后者会让一像素宽的斜笔画随机断开）。
pub(crate) fn rasterize_mono(
    outline: &Outline,
    scale: f32,
    embolden_distance: f32,
    dropout: Dropout,
) -> Raster {
    let Some((contours, left, bottom, width, height)) = prepare(outline, scale, embolden_distance)
    else {
        return empty();
    };
    let top = bottom + height;
    let mut coverage = vec![0u8; (width * height) as usize];
    let lit = |coverage: &[u8], column: i32, row: i32| {
        (0..width).contains(&column)
            && (0..height).contains(&row)
            && coverage[(row * width + column) as usize] != 0
    };
    let set = |coverage: &mut [u8], column: i32, row: i32| {
        if (0..width).contains(&column) && (0..height).contains(&row) {
            coverage[(row * width + column) as usize] = 255;
        }
    };
    // 1. 规则 1：像素中心落在 [进入, 离开] 闭区间内即点亮；没有覆盖任何中心的区间记为丢点候选。
    let mut candidates = Vec::new();
    for row in 0..height {
        let center = top as f32 - row as f32 - 0.5;
        for (entry, exit) in spans(&crossings(&contours, center, true)) {
            let (start, end) = (entry.position, exit.position);
            let first = (start - 0.5 - left as f32).ceil() as i32;
            let last = (end - 0.5 - left as f32).floor() as i32;
            if first <= last {
                for column in first..=last {
                    set(&mut coverage, column, row);
                }
            } else if dropout == Dropout::Stubs
                || (dropout == Dropout::NoStubs && !stub(&contours, entry, exit, center, true))
            {
                // 区间夹在 first - 1 与 first 两个像素中心之间：候选为左侧像素，另一侧用于邻居检查。
                candidates.push((first - 1, first, row, true));
            }
        }
    }
    if dropout != Dropout::Off {
        for column in 0..width {
            let center = left as f32 + column as f32 + 0.5;
            for (entry, exit) in spans(&crossings(&contours, center, false)) {
                let (start, end) = (entry.position, exit.position);
                let first = (start - 0.5 - bottom as f32).ceil() as i32;
                let last = (end - 0.5 - bottom as f32).floor() as i32;
                if first > last
                    && (dropout == Dropout::Stubs || !stub(&contours, entry, exit, center, false))
                {
                    candidates.push((height - first, height - 1 - first, column, false));
                }
            }
        }
    }
    // 2. 丢点补点（ftraster）：两侧相邻像素任一已点亮时不补；选中的像素在包围盒外时改用另一侧。
    for (chosen, other, line, horizontal) in candidates {
        let at = |index: i32| {
            if horizontal {
                (index, line)
            } else {
                (line, index)
            }
        };
        let (chosen_at, other_at) = (at(chosen), at(other));
        if lit(&coverage, chosen_at.0, chosen_at.1) || lit(&coverage, other_at.0, other_at.1) {
            continue;
        }
        let limit = if horizontal { width } else { height };
        let target = if (0..limit).contains(&chosen) {
            chosen_at
        } else {
            other_at
        };
        set(&mut coverage, target.0, target.1);
    }
    Raster {
        coverage,
        width,
        height,
        left,
        top,
    }
}

pub(crate) fn rasterize(outline: &Outline, scale: f32, embolden_distance: f32) -> Raster {
    let Some((contours, left, bottom, width, height)) = prepare(outline, scale, embolden_distance)
    else {
        return empty();
    };
    let top = bottom + height;
    let stride = width as usize + 2;
    let mut accumulator = Accumulator {
        cells: vec![0.0; stride * height as usize],
        width: width as usize,
        height: height as usize,
        stride,
    };
    // 位图坐标 y 向下：把字体坐标（y 向上）翻转并平移到包围盒原点。
    let map = |v: Vertex| Vertex {
        x: v.x - left as f32,
        y: top as f32 - v.y,
    };
    for contour in &contours {
        for pair in contour.windows(2) {
            accumulator.line(map(pair[0]), map(pair[1]));
        }
        if let (Some(&last), Some(&first)) = (contour.last(), contour.first()) {
            accumulator.line(map(last), map(first));
        }
    }
    Raster {
        coverage: accumulator.coverage(),
        width,
        height,
        left,
        top,
    }
}
