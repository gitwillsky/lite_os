//! 字节码虚拟机：取指、栈与图形状态、全部 TrueType 指令的语义（MDRP/MIRP、IUP、DELTA、SCANCTRL 等）。
//! 程序与字形的装载、点区准备由上层 [`super`] 负责；本模块只执行给定的指令流。
use super::*;

/// 一次程序执行的虚拟机：栈、图形状态，以及借用的 CVT、存储区、点区与字体级定义。
pub(super) struct Machine<'a> {
    pub(super) stack: Vec<i32>,
    pub(super) max_stack: usize,
    pub(super) state: GraphicsState,
    pub(super) cvt: &'a mut Vec<i32>,
    pub(super) storage: &'a mut Vec<i32>,
    pub(super) zones: &'a mut [Zone; 2],
    pub(super) program: &'a mut FontProgram,
    pub(super) ppem: u32,
    pub(super) scale: i64,
    pub(super) steps: u32,
    pub(super) smoothing: bool,
    /// INSTCTRL 只在 prep 中生效。
    pub(super) in_prep: bool,
}

/// 指令流游标。
struct Cursor<'b> {
    code: &'b [u8],
    ip: usize,
}

impl Cursor<'_> {
    fn byte(&mut self) -> Exec<u8> {
        let value = *self.code.get(self.ip).ok_or(())?;
        self.ip += 1;
        Ok(value)
    }
    fn word(&mut self) -> Exec<i32> {
        let high = self.byte()?;
        let low = self.byte()?;
        Ok(i32::from(i16::from_be_bytes([high, low])))
    }
}

/// 指令长度（用于跳过 IF/ELSE/FDEF 体）。
fn instruction_length(code: &[u8], ip: usize) -> Exec<usize> {
    let opcode = *code.get(ip).ok_or(())?;
    Ok(match opcode {
        0x40 => 2 + usize::from(*code.get(ip + 1).ok_or(())?),
        0x41 => 2 + 2 * usize::from(*code.get(ip + 1).ok_or(())?),
        0xB0..=0xB7 => 1 + usize::from(opcode - 0xAF),
        0xB8..=0xBF => 1 + 2 * usize::from(opcode - 0xB7),
        _ => 1,
    })
}

impl Machine<'_> {
    fn pop(&mut self) -> Exec<i32> {
        self.stack.pop().ok_or(())
    }

    fn push(&mut self, value: i32) -> Exec<()> {
        if self.stack.len() >= self.max_stack {
            return Err(());
        }
        self.stack.push(value);
        Ok(())
    }

    fn zone(&self, index: usize) -> Exec<&Zone> {
        self.zones.get(index).ok_or(())
    }

    fn point(&self, zone: usize, index: i32) -> Exec<usize> {
        let index = usize::try_from(index).map_err(|_| ())?;
        (index < self.zone(zone)?.current.len())
            .then_some(index)
            .ok_or(())
    }

    fn project(&self, dx: i32, dy: i32) -> i32 {
        dot14(dx, dy, self.state.projection)
    }

    fn dual_project(&self, dx: i32, dy: i32) -> i32 {
        dot14(dx, dy, self.state.dual)
    }

    /// 两点原始位置在对偶投影向量上的距离：twilight 区用原始 26.6 坐标，
    /// 字形区用字体单位坐标投影后再缩放（FreeType v35 的做法）。
    fn original_distance(&self, zone_a: usize, a: usize, zone_b: usize, b: usize) -> i32 {
        if zone_a == 0 || zone_b == 0 {
            let (p, q) = (self.original(zone_a, a), self.original(zone_b, b));
            return self.dual_project(p.0 - q.0, p.1 - q.1);
        }
        let (p, q) = (self.zones[zone_a].units[a], self.zones[zone_b].units[b]);
        mul_fix(dot14(p.0 - q.0, p.1 - q.1, self.state.dual), self.scale)
    }

    fn current(&self, zone: usize, index: usize) -> (i32, i32) {
        self.zones[zone].current[index]
    }

    fn original(&self, zone: usize, index: usize) -> (i32, i32) {
        self.zones[zone].original[index]
    }

    /// 沿自由向量移动，使投影距离改变 `distance`；按自由向量分量标记触碰。
    fn move_point(&mut self, zone: usize, index: usize, distance: i32, touch: bool) {
        let (fx, fy) = self.state.freedom;
        let (px, py) = self.state.projection;
        let mut dot =
            ((i64::from(fx) * i64::from(px) + i64::from(fy) * i64::from(py)) >> 14) as i32;
        if dot.abs() < 0x400 {
            dot = 0x4000;
        }
        let point = &mut self.zones[zone].current[index];
        if fx != 0 {
            point.0 = point.0.wrapping_add(mul_div(distance, fx, dot));
            if touch {
                self.zones[zone].touched[index] |= 1;
            }
        }
        let point = &mut self.zones[zone].current[index];
        if fy != 0 {
            point.1 = point.1.wrapping_add(mul_div(distance, fy, dot));
            if touch {
                self.zones[zone].touched[index] |= 2;
            }
        }
    }

    fn round(&self, value: i32) -> i32 {
        let sign = |magnitude: i32| if value >= 0 { magnitude } else { -magnitude };
        let absolute = value.saturating_abs();
        match self.state.round {
            Round::Grid => sign((absolute + 32) & !63),
            Round::HalfGrid => sign((absolute & !63) + 32),
            Round::DoubleGrid => sign((absolute + 16) & !31),
            Round::DownToGrid => sign(absolute & !63),
            Round::UpToGrid => sign((absolute + 63) & !63),
            Round::Off => value,
            Round::Super | Round::Super45 => {
                let (period, phase, threshold) =
                    (self.state.period, self.state.phase, self.state.threshold);
                if period <= 0 {
                    return value;
                }
                if value >= 0 {
                    let result = (value - phase + threshold).div_euclid(period) * period + phase;
                    if result < 0 { phase } else { result }
                } else {
                    let result =
                        -((-value - phase + threshold).div_euclid(period) * period + phase);
                    if result > 0 { -phase } else { result }
                }
            }
        }
    }

    fn super_round(&mut self, selector: i32, grid: i32) {
        self.state.period = match (selector >> 6) & 3 {
            0 => grid / 2,
            1 => grid,
            2 => grid * 2,
            _ => grid,
        };
        self.state.phase = match (selector >> 4) & 3 {
            0 => 0,
            1 => self.state.period / 4,
            2 => self.state.period / 2,
            _ => self.state.period * 3 / 4,
        };
        let threshold = selector & 15;
        self.state.threshold = if threshold == 0 {
            self.state.period - 1
        } else {
            (threshold - 4) * self.state.period / 8
        };
    }

    pub(super) fn run(&mut self, code: &[u8], depth: u32) -> Exec<()> {
        if depth > CALL_DEPTH {
            return Err(());
        }
        let mut cursor = Cursor { code, ip: 0 };
        while cursor.ip < code.len() {
            self.steps += 1;
            if self.steps > STEP_LIMIT {
                return Err(());
            }
            let start = cursor.ip;
            let opcode = cursor.byte()?;
            self.execute(opcode, &mut cursor, start, depth)?;
        }
        Ok(())
    }

    /// 跳过到匹配的 ELSE（`stop_at_else`）或 EIF；返回停在的指令。
    fn skip_conditional(cursor: &mut Cursor<'_>, stop_at_else: bool) -> Exec<u8> {
        let mut nesting = 0;
        loop {
            let opcode = *cursor.code.get(cursor.ip).ok_or(())?;
            cursor.ip += instruction_length(cursor.code, cursor.ip)?;
            match opcode {
                0x58 => nesting += 1,
                0x1B if nesting == 0 && stop_at_else => return Ok(opcode),
                0x59 if nesting == 0 => return Ok(opcode),
                0x59 => nesting -= 1,
                _ => {}
            }
        }
    }

    /// FDEF/IDEF：记录到 ENDF 为止的函数体。
    fn capture_body(cursor: &mut Cursor<'_>) -> Exec<Rc<[u8]>> {
        let start = cursor.ip;
        loop {
            let opcode = *cursor.code.get(cursor.ip).ok_or(())?;
            if matches!(opcode, 0x2C | 0x89) {
                return Err(());
            }
            let length = instruction_length(cursor.code, cursor.ip)?;
            if opcode == 0x2D {
                let body: Rc<[u8]> = Rc::from(&cursor.code[start..cursor.ip]);
                cursor.ip += length;
                return Ok(body);
            }
            cursor.ip += length;
        }
    }

    fn jump(cursor: &mut Cursor<'_>, start: usize, offset: i32) -> Exec<()> {
        let target = i64::try_from(start).map_err(|_| ())? + i64::from(offset);
        cursor.ip = usize::try_from(target).map_err(|_| ())?;
        if cursor.ip > cursor.code.len() {
            return Err(());
        }
        Ok(())
    }

    fn call(&mut self, function: i32, depth: u32) -> Exec<()> {
        let body = self
            .program
            .functions
            .get(usize::try_from(function).map_err(|_| ())?)
            .and_then(Clone::clone)
            .ok_or(())?;
        self.run(&body, depth + 1)
    }

    fn set_vector_to_line(&mut self, opcode: u8, target: u8) -> Exec<()> {
        let second = self.pop()?;
        let first = self.pop()?;
        let (zp1, zp2) = (self.state.zp[1], self.state.zp[2]);
        let a = self.point(zp1, first)?;
        let b = self.point(zp2, second)?;
        let line = |points: &dyn Fn(usize, usize) -> (i32, i32)| {
            let (p, q) = (points(zp1, a), points(zp2, b));
            let (mut dx, mut dy) = (p.0 - q.0, p.1 - q.1);
            if opcode & 1 != 0 {
                (dx, dy) = (-dy, dx);
            }
            normalize(dx, dy)
        };
        let current = line(&|zone, index| self.current(zone, index));
        match target {
            0 => {
                self.state.projection = current;
                self.state.dual = current;
            }
            1 => self.state.freedom = current,
            _ => {
                self.state.projection = current;
                self.state.dual = line(&|zone, index| self.original(zone, index));
            }
        }
        Ok(())
    }

    fn loop_points(&mut self) -> Exec<Vec<i32>> {
        let count = self.state.loop_count.max(1);
        self.state.loop_count = 1;
        (0..count).map(|_| self.pop()).collect()
    }

    /// SHP/SHC/SHZ 的参考位移：参考点当前与原始位置之差在投影向量上的距离。
    fn shift_reference(&self, opcode: u8) -> Exec<(usize, usize, i32)> {
        let (zone, point) = if opcode & 1 == 0 {
            (self.state.zp[1], self.state.rp[2])
        } else {
            (self.state.zp[0], self.state.rp[1])
        };
        let point = self.point(zone, point as i32)?;
        let (current, original) = (self.current(zone, point), self.original(zone, point));
        Ok((
            zone,
            point,
            self.project(current.0 - original.0, current.1 - original.1),
        ))
    }

    fn execute(
        &mut self,
        opcode: u8,
        cursor: &mut Cursor<'_>,
        start: usize,
        depth: u32,
    ) -> Exec<()> {
        let x_axis = (0x4000, 0);
        let y_axis = (0, 0x4000);
        match opcode {
            0x00 | 0x01 => {
                let axis = if opcode == 1 { x_axis } else { y_axis };
                self.state.freedom = axis;
                self.state.projection = axis;
                self.state.dual = axis;
            }
            0x02 | 0x03 => {
                let axis = if opcode == 3 { x_axis } else { y_axis };
                self.state.projection = axis;
                self.state.dual = axis;
            }
            0x04 | 0x05 => self.state.freedom = if opcode == 5 { x_axis } else { y_axis },
            0x06 | 0x07 => self.set_vector_to_line(opcode, 0)?,
            0x08 | 0x09 => self.set_vector_to_line(opcode, 1)?,
            0x0A | 0x0B => {
                let y = self.pop()?;
                let x = self.pop()?;
                let vector = normalize(x, y);
                if opcode == 0x0A {
                    self.state.projection = vector;
                    self.state.dual = vector;
                } else {
                    self.state.freedom = vector;
                }
            }
            0x0C => {
                let (x, y) = self.state.projection;
                self.push(x)?;
                self.push(y)?;
            }
            0x0D => {
                let (x, y) = self.state.freedom;
                self.push(x)?;
                self.push(y)?;
            }
            0x0E => self.state.freedom = self.state.projection,
            0x0F => {
                let b1 = self.pop()?;
                let b0 = self.pop()?;
                let a1 = self.pop()?;
                let a0 = self.pop()?;
                let point = self.pop()?;
                let [zp0, zp1, zp2] = self.state.zp;
                let (a0, a1) = (self.point(zp1, a0)?, self.point(zp1, a1)?);
                let (b0, b1) = (self.point(zp0, b0)?, self.point(zp0, b1)?);
                let point = self.point(zp2, point)?;
                let (pa0, pa1) = (self.current(zp1, a0), self.current(zp1, a1));
                let (pb0, pb1) = (self.current(zp0, b0), self.current(zp0, b1));
                let (dax, day) = (f64::from(pa1.0 - pa0.0), f64::from(pa1.1 - pa0.1));
                let (dbx, dby) = (f64::from(pb1.0 - pb0.0), f64::from(pb1.1 - pb0.1));
                let denominator = dax * dby - day * dbx;
                let result = if denominator.abs() < 1e-6 {
                    (
                        (pa0.0 + pa1.0 + pb0.0 + pb1.0) / 4,
                        (pa0.1 + pa1.1 + pb0.1 + pb1.1) / 4,
                    )
                } else {
                    let t = ((f64::from(pb0.0 - pa0.0)) * dby - (f64::from(pb0.1 - pa0.1)) * dbx)
                        / denominator;
                    (
                        round_half_even_free(f64::from(pa0.0) + t * dax),
                        round_half_even_free(f64::from(pa0.1) + t * day),
                    )
                };
                self.zones[zp2].current[point] = result;
                self.zones[zp2].touched[point] |= 3;
            }
            0x10..=0x12 => {
                let value = self.pop()?;
                self.state.rp[usize::from(opcode - 0x10)] =
                    usize::try_from(value).map_err(|_| ())?;
            }
            0x13..=0x16 => {
                let zone = self.pop()?;
                if !(0..=1).contains(&zone) {
                    return Err(());
                }
                let zone = zone as usize;
                match opcode {
                    0x13 => self.state.zp[0] = zone,
                    0x14 => self.state.zp[1] = zone,
                    0x15 => self.state.zp[2] = zone,
                    _ => self.state.zp = [zone; 3],
                }
            }
            0x17 => {
                let count = self.pop()?;
                if count < 0 {
                    return Err(());
                }
                self.state.loop_count = count;
            }
            0x18 => self.state.round = Round::Grid,
            0x19 => self.state.round = Round::HalfGrid,
            0x1A => self.state.minimum_distance = self.pop()?,
            0x1B => {
                Self::skip_conditional(cursor, false)?;
            }
            0x1C => {
                let offset = self.pop()?;
                Self::jump(cursor, start, offset)?;
            }
            0x1D => self.state.cvt_cutin = self.pop()?,
            0x1E => self.state.single_width_cutin = self.pop()?,
            0x1F => {
                let value = self.pop()?;
                self.state.single_width = mul_fix(value, self.scale);
            }
            0x20 => {
                let value = *self.stack.last().ok_or(())?;
                self.push(value)?;
            }
            0x21 => {
                self.pop()?;
            }
            0x22 => self.stack.clear(),
            0x23 => {
                let b = self.pop()?;
                let a = self.pop()?;
                self.push(b)?;
                self.push(a)?;
            }
            0x24 => {
                let depth = i32::try_from(self.stack.len()).map_err(|_| ())?;
                self.push(depth)?;
            }
            0x25 | 0x26 => {
                let index = self.pop()?;
                let len = self.stack.len();
                let position = usize::try_from(index)
                    .ok()
                    .filter(|index| (1..=len).contains(index))
                    .map(|index| len - index)
                    .ok_or(())?;
                if opcode == 0x25 {
                    let value = self.stack[position];
                    self.push(value)?;
                } else {
                    let value = self.stack.remove(position);
                    self.push(value)?;
                }
            }
            0x27 => {
                let second = self.pop()?;
                let first = self.pop()?;
                let (zp0, zp1) = (self.state.zp[0], self.state.zp[1]);
                let p1 = self.point(zp1, first)?;
                let p2 = self.point(zp0, second)?;
                let (a, b) = (self.current(zp1, p1), self.current(zp0, p2));
                let distance = self.project(b.0 - a.0, b.1 - a.1) / 2;
                self.move_point(zp1, p1, distance, true);
                self.move_point(zp0, p2, -distance, true);
            }
            0x29 => {
                let point = self.pop()?;
                let zp0 = self.state.zp[0];
                let point = self.point(zp0, point)?;
                if self.state.freedom.0 != 0 {
                    self.zones[zp0].touched[point] &= !1;
                }
                if self.state.freedom.1 != 0 {
                    self.zones[zp0].touched[point] &= !2;
                }
            }
            0x2A => {
                let function = self.pop()?;
                let count = self.pop()?;
                for _ in 0..count.max(0) {
                    self.call(function, depth)?;
                }
            }
            0x2B => {
                let function = self.pop()?;
                self.call(function, depth)?;
            }
            0x2C => {
                let function = self.pop()?;
                let body = Self::capture_body(cursor)?;
                let slot = self
                    .program
                    .functions
                    .get_mut(usize::try_from(function).map_err(|_| ())?)
                    .ok_or(())?;
                *slot = Some(body);
            }
            0x2D => {
                // 函数体以 ENDF 结束：调用方的 run 在函数体末尾自然返回。
                cursor.ip = cursor.code.len();
            }
            0x2E | 0x2F => {
                let point = self.pop()?;
                let zp0 = self.state.zp[0];
                let point = self.point(zp0, point)?;
                let distance = if opcode == 0x2F {
                    let (x, y) = self.current(zp0, point);
                    let projected = self.project(x, y);
                    self.round(projected) - projected
                } else {
                    0
                };
                self.move_point(zp0, point, distance, true);
                self.state.rp[0] = point;
                self.state.rp[1] = point;
            }
            0x30 | 0x31 => self.interpolate_untouched(opcode == 0x31),
            0x32 | 0x33 => {
                let (_, _, distance) = self.shift_reference(opcode)?;
                let zp2 = self.state.zp[2];
                for point in self.loop_points()? {
                    let point = self.point(zp2, point)?;
                    self.move_point(zp2, point, distance, true);
                }
            }
            0x34 | 0x35 => {
                let contour = self.pop()?;
                let (zone, reference, distance) = self.shift_reference(opcode)?;
                let zp2 = self.state.zp[2];
                let ends = &self.zones[zp2].ends;
                let contour = usize::try_from(contour).map_err(|_| ())?;
                let last = *ends.get(contour).ok_or(())?;
                let first = if contour == 0 {
                    0
                } else {
                    ends[contour - 1] + 1
                };
                for point in first..=last {
                    if zone != zp2 || point != reference {
                        self.move_point(zp2, point, distance, true);
                    }
                }
            }
            0x36 | 0x37 => {
                let target = self.pop()?;
                if !(0..=1).contains(&target) {
                    return Err(());
                }
                let target = target as usize;
                let (zone, reference, distance) = self.shift_reference(opcode)?;
                // 字形区的幻影点不随 SHZ 移动。
                let count = self.zones[target].current.len()
                    - if target == 1 {
                        4.min(self.zones[1].current.len())
                    } else {
                        0
                    };
                for point in 0..count {
                    if zone != target || point != reference {
                        self.move_point(target, point, distance, false);
                    }
                }
            }
            0x38 => {
                let distance = self.pop()?;
                let zp2 = self.state.zp[2];
                let (fx, fy) = self.state.freedom;
                for point in self.loop_points()? {
                    let point = self.point(zp2, point)?;
                    let current = &mut self.zones[zp2].current[point];
                    current.0 += mul14(distance, fx);
                    current.1 += mul14(distance, fy);
                    if fx != 0 {
                        self.zones[zp2].touched[point] |= 1;
                    }
                    if fy != 0 {
                        self.zones[zp2].touched[point] |= 2;
                    }
                }
            }
            0x39 => {
                let [zp0, zp1, zp2] = self.state.zp;
                let rp1 = self.point(zp0, self.state.rp[1] as i32)?;
                let rp2 = self.point(zp1, self.state.rp[2] as i32)?;
                let (c1, c2) = (self.current(zp0, rp1), self.current(zp1, rp2));
                let original_range = self.original_distance(zp1, rp2, zp0, rp1);
                let current_range = self.project(c2.0 - c1.0, c2.1 - c1.1);
                for point in self.loop_points()? {
                    let point = self.point(zp2, point)?;
                    let c = self.current(zp2, point);
                    let original_distance = self.original_distance(zp2, point, zp0, rp1);
                    let current_distance = self.project(c.0 - c1.0, c.1 - c1.1);
                    let target = if original_range != 0 {
                        mul_div(original_distance, current_range, original_range)
                    } else {
                        original_distance
                    };
                    self.move_point(zp2, point, target - current_distance, true);
                }
            }
            0x3A | 0x3B => {
                let distance = self.pop()?;
                let point = self.pop()?;
                let (zp0, zp1) = (self.state.zp[0], self.state.zp[1]);
                let reference = self.point(zp0, self.state.rp[0] as i32)?;
                let point = self.point(zp1, point)?;
                if zp1 == 0 {
                    let origin = self.original(zp0, reference);
                    let placed = (
                        origin.0 + mul14(distance, self.state.freedom.0),
                        origin.1 + mul14(distance, self.state.freedom.1),
                    );
                    self.zones[0].original[point] = placed;
                    self.zones[0].current[point] = placed;
                }
                let (c, r) = (self.current(zp1, point), self.current(zp0, reference));
                let current = self.project(c.0 - r.0, c.1 - r.1);
                self.move_point(zp1, point, distance - current, true);
                self.state.rp[1] = reference;
                self.state.rp[2] = point;
                if opcode == 0x3B {
                    self.state.rp[0] = point;
                }
            }
            0x3C => {
                let (zp0, zp1) = (self.state.zp[0], self.state.zp[1]);
                let reference = self.point(zp0, self.state.rp[0] as i32)?;
                for point in self.loop_points()? {
                    let point = self.point(zp1, point)?;
                    let (c, r) = (self.current(zp1, point), self.current(zp0, reference));
                    let distance = self.project(c.0 - r.0, c.1 - r.1);
                    self.move_point(zp1, point, -distance, true);
                }
            }
            0x3D => self.state.round = Round::DoubleGrid,
            0x3E | 0x3F => {
                let entry = self.pop()?;
                let point = self.pop()?;
                let zp0 = self.state.zp[0];
                let point = self.point(zp0, point)?;
                let mut distance = *self
                    .cvt
                    .get(usize::try_from(entry).map_err(|_| ())?)
                    .ok_or(())?;
                if zp0 == 0 {
                    let placed = (
                        mul14(distance, self.state.freedom.0),
                        mul14(distance, self.state.freedom.1),
                    );
                    self.zones[0].original[point] = placed;
                    self.zones[0].current[point] = placed;
                }
                let (x, y) = self.current(zp0, point);
                let original = self.project(x, y);
                if opcode == 0x3F {
                    if (distance - original).abs() > self.state.cvt_cutin {
                        distance = original;
                    }
                    distance = self.round(distance);
                }
                self.move_point(zp0, point, distance - original, true);
                self.state.rp[0] = point;
                self.state.rp[1] = point;
            }
            0x40 | 0x41 | 0xB0..=0xBF => {
                let (count, words) = match opcode {
                    0x40 => (usize::from(cursor.byte()?), false),
                    0x41 => (usize::from(cursor.byte()?), true),
                    0xB0..=0xB7 => (usize::from(opcode - 0xAF), false),
                    _ => (usize::from(opcode - 0xB7), true),
                };
                for _ in 0..count {
                    let value = if words {
                        cursor.word()?
                    } else {
                        i32::from(cursor.byte()?)
                    };
                    self.push(value)?;
                }
            }
            0x42 => {
                let value = self.pop()?;
                let location = self.pop()?;
                *self
                    .storage
                    .get_mut(usize::try_from(location).map_err(|_| ())?)
                    .ok_or(())? = value;
            }
            0x43 => {
                let location = self.pop()?;
                let value = *self
                    .storage
                    .get(usize::try_from(location).map_err(|_| ())?)
                    .ok_or(())?;
                self.push(value)?;
            }
            0x44 | 0x70 => {
                let mut value = self.pop()?;
                let location = self.pop()?;
                if opcode == 0x70 {
                    value = mul_fix(value, self.scale);
                }
                *self
                    .cvt
                    .get_mut(usize::try_from(location).map_err(|_| ())?)
                    .ok_or(())? = value;
            }
            0x45 => {
                let location = self.pop()?;
                let value = *self
                    .cvt
                    .get(usize::try_from(location).map_err(|_| ())?)
                    .ok_or(())?;
                self.push(value)?;
            }
            0x46 | 0x47 => {
                let point = self.pop()?;
                let zp2 = self.state.zp[2];
                let point = self.point(zp2, point)?;
                let value = if opcode == 0x46 {
                    let (x, y) = self.current(zp2, point);
                    self.project(x, y)
                } else {
                    let (x, y) = self.original(zp2, point);
                    self.dual_project(x, y)
                };
                self.push(value)?;
            }
            0x48 => {
                let value = self.pop()?;
                let point = self.pop()?;
                let zp2 = self.state.zp[2];
                let point = self.point(zp2, point)?;
                let (x, y) = self.current(zp2, point);
                let projected = self.project(x, y);
                self.move_point(zp2, point, value - projected, true);
                if zp2 == 0 {
                    self.zones[0].original[point] = self.zones[0].current[point];
                }
            }
            0x49 | 0x4A => {
                let second = self.pop()?;
                let first = self.pop()?;
                let (zp0, zp1) = (self.state.zp[0], self.state.zp[1]);
                let a = self.point(zp0, first)?;
                let b = self.point(zp1, second)?;
                let distance = if opcode == 0x4A {
                    let (p, q) = (self.current(zp0, a), self.current(zp1, b));
                    self.project(p.0 - q.0, p.1 - q.1)
                } else {
                    self.original_distance(zp0, a, zp1, b)
                };
                self.push(distance)?;
            }
            0x4B | 0x4C => self.push(i32::try_from(self.ppem).map_err(|_| ())?)?,
            0x4D => self.state.auto_flip = true,
            0x4E => self.state.auto_flip = false,
            0x4F => {
                self.pop()?;
            }
            0x50..=0x55 => {
                let b = self.pop()?;
                let a = self.pop()?;
                let result = match opcode {
                    0x50 => a < b,
                    0x51 => a <= b,
                    0x52 => a > b,
                    0x53 => a >= b,
                    0x54 => a == b,
                    _ => a != b,
                };
                self.push(i32::from(result))?;
            }
            0x56 | 0x57 => {
                let value = self.pop()?;
                let even = self.round(value) & 127 == 0;
                self.push(i32::from(if opcode == 0x56 { !even } else { even }))?;
            }
            0x58 => {
                if self.pop()? == 0 {
                    Self::skip_conditional(cursor, true)?;
                }
            }
            0x59 => {}
            0x5A => {
                let b = self.pop()?;
                let a = self.pop()?;
                self.push(i32::from(a != 0 && b != 0))?;
            }
            0x5B => {
                let b = self.pop()?;
                let a = self.pop()?;
                self.push(i32::from(a != 0 || b != 0))?;
            }
            0x5C => {
                let value = self.pop()?;
                self.push(i32::from(value == 0))?;
            }
            0x5D | 0x71 | 0x72 => {
                let base = match opcode {
                    0x5D => 0,
                    0x71 => 16,
                    _ => 32,
                };
                let count = self.pop()?;
                let zp0 = self.state.zp[0];
                for _ in 0..count.max(0) {
                    let point = self.pop()?;
                    let argument = self.pop()?;
                    if let Some(amount) = self.delta(argument, base) {
                        let point = self.point(zp0, point)?;
                        self.move_point(zp0, point, amount, true);
                    }
                }
            }
            0x5E => self.state.delta_base = self.pop()?,
            0x5F => {
                let shift = self.pop()?;
                if !(0..=6).contains(&shift) {
                    return Err(());
                }
                self.state.delta_shift = shift;
            }
            0x60 => {
                let b = self.pop()?;
                let a = self.pop()?;
                self.push(a.wrapping_add(b))?;
            }
            0x61 => {
                let b = self.pop()?;
                let a = self.pop()?;
                self.push(a.wrapping_sub(b))?;
            }
            0x62 => {
                let b = self.pop()?;
                let a = self.pop()?;
                if b == 0 {
                    return Err(());
                }
                self.push(mul_div(a, 64, b))?;
            }
            0x63 => {
                let b = self.pop()?;
                let a = self.pop()?;
                self.push(mul_div(a, b, 64))?;
            }
            0x64 => {
                let value = self.pop()?;
                self.push(value.saturating_abs())?;
            }
            0x65 => {
                let value = self.pop()?;
                self.push(value.wrapping_neg())?;
            }
            0x66 => {
                let value = self.pop()?;
                self.push(value & !63)?;
            }
            0x67 => {
                let value = self.pop()?;
                self.push(value.wrapping_add(63) & !63)?;
            }
            0x68..=0x6B => {
                let value = self.pop()?;
                let rounded = self.round(value);
                self.push(rounded)?;
            }
            0x6C..=0x6F => {}
            0x73..=0x75 => {
                let base = match opcode {
                    0x73 => 0,
                    0x74 => 16,
                    _ => 32,
                };
                let count = self.pop()?;
                for _ in 0..count.max(0) {
                    let entry = self.pop()?;
                    let argument = self.pop()?;
                    if let Some(amount) = self.delta(argument, base) {
                        let slot = self
                            .cvt
                            .get_mut(usize::try_from(entry).map_err(|_| ())?)
                            .ok_or(())?;
                        *slot += amount;
                    }
                }
            }
            0x76 => {
                let selector = self.pop()?;
                self.super_round(selector, 64);
                self.state.round = Round::Super;
            }
            0x77 => {
                let selector = self.pop()?;
                // √2/2 网格：周期 45.25 ≈ 46/64 像素的规范近似。
                self.super_round(selector, 46);
                self.state.round = Round::Super45;
            }
            0x78 | 0x79 => {
                let condition = self.pop()?;
                let offset = self.pop()?;
                if (condition != 0) == (opcode == 0x78) {
                    Self::jump(cursor, start, offset)?;
                }
            }
            0x7A => self.state.round = Round::Off,
            0x7C => self.state.round = Round::UpToGrid,
            0x7D => self.state.round = Round::DownToGrid,
            0x7E | 0x7F => {
                self.pop()?;
            }
            0x85 => {
                // FreeType Ins_SCANCTRL：低 8 位为 ppem 阈值，0xFF 恒开、0 恒关；本实现没有旋转与拉伸。
                let value = self.pop()?;
                let threshold = value & 0xFF;
                let ppem = i32::try_from(self.ppem).map_err(|_| ())?;
                if threshold == 0xFF {
                    self.state.scan_control = true;
                } else if threshold == 0 {
                    self.state.scan_control = false;
                } else {
                    if value & 0x100 != 0 && ppem <= threshold {
                        self.state.scan_control = true;
                    }
                    if value & 0x800 != 0 && ppem > threshold {
                        self.state.scan_control = false;
                    }
                }
            }
            0x8D => {
                let value = self.pop()?;
                if value >= 0 {
                    self.state.scan_type = value & 0xFFFF;
                }
            }
            0x80 => {
                let zp0 = self.state.zp[0];
                for point in self.loop_points()? {
                    let point = self.point(zp0, point)?;
                    let on = &mut self.zones[zp0].on_curve[point];
                    *on = !*on;
                }
            }
            0x81 | 0x82 => {
                let high = self.pop()?;
                let low = self.pop()?;
                let zp0 = self.state.zp[0];
                let (low, high) = (self.point(zp0, low)?, self.point(zp0, high)?);
                self.zones[zp0].on_curve[low..=high].fill(opcode == 0x81);
            }
            0x86 | 0x87 => self.set_vector_to_line(opcode, 2)?,
            0x88 => {
                let selector = self.pop()?;
                let mut result = 0;
                if selector & 1 != 0 {
                    result |= 35;
                }
                if selector & 32 != 0 && self.smoothing {
                    result |= 1 << 12;
                }
                self.push(result)?;
            }
            0x89 => {
                let target = self.pop()?;
                let body = Self::capture_body(cursor)?;
                let target = u8::try_from(target).map_err(|_| ())?;
                self.program.instructions.insert(target, body);
            }
            0x8A => {
                let c = self.pop()?;
                let b = self.pop()?;
                let a = self.pop()?;
                self.push(b)?;
                self.push(c)?;
                self.push(a)?;
            }
            0x8B | 0x8C => {
                let b = self.pop()?;
                let a = self.pop()?;
                self.push(if opcode == 0x8B { a.max(b) } else { a.min(b) })?;
            }
            0x8E => {
                let selector = self.pop()?;
                let value = self.pop()?;
                if self.in_prep {
                    if !(1..=3).contains(&selector) {
                        return Err(());
                    }
                    let flag = if value != 0 { selector } else { 0 };
                    self.state.instruct_control = (self.state.instruct_control & !selector) | flag;
                }
            }
            0xC0..=0xDF => self.move_direct_relative(opcode)?,
            0xE0..=0xFF => self.move_indirect_relative(opcode)?,
            _ => {
                let body = self.program.instructions.get(&opcode).cloned().ok_or(())?;
                self.run(&body, depth + 1)?;
            }
        }
        Ok(())
    }

    /// DELTAP/DELTAC 的调整量；本 ppem 不匹配返回 None。
    fn delta(&self, argument: i32, base: i32) -> Option<i32> {
        let ppem = self.state.delta_base + base + ((argument >> 4) & 15);
        if u32::try_from(ppem).ok()? != self.ppem {
            return None;
        }
        let mut step = (argument & 15) - 8;
        if step >= 0 {
            step += 1;
        }
        Some(step * 64 / (1 << self.state.delta_shift))
    }

    /// MDRP[abcde]：以 rp0 为参考，保持原始距离（单宽、取整、最小距离按标志处理）。
    fn move_direct_relative(&mut self, opcode: u8) -> Exec<()> {
        let point = self.pop()?;
        let (zp0, zp1) = (self.state.zp[0], self.state.zp[1]);
        let reference = self.point(zp0, self.state.rp[0] as i32)?;
        let point = self.point(zp1, point)?;
        let mut original = self.original_distance(zp1, point, zp0, reference);
        if (original.abs() - self.state.single_width).abs() < self.state.single_width_cutin {
            original = if original >= 0 {
                self.state.single_width
            } else {
                -self.state.single_width
            };
        }
        let mut distance = if opcode & 4 != 0 {
            self.round(original)
        } else {
            original
        };
        if opcode & 8 != 0 {
            distance = self.minimum(original, distance);
        }
        let (c, rc) = (self.current(zp1, point), self.current(zp0, reference));
        let current = self.project(c.0 - rc.0, c.1 - rc.1);
        self.move_point(zp1, point, distance - current, true);
        self.state.rp[1] = reference;
        self.state.rp[2] = point;
        if opcode & 16 != 0 {
            self.state.rp[0] = point;
        }
        Ok(())
    }

    /// MIRP[abcde]：以 rp0 为参考，把距离设为 CVT 值（cut-in、自动翻转、取整、最小距离）。
    fn move_indirect_relative(&mut self, opcode: u8) -> Exec<()> {
        let entry = self.pop()?;
        let point = self.pop()?;
        let (zp0, zp1) = (self.state.zp[0], self.state.zp[1]);
        let reference = self.point(zp0, self.state.rp[0] as i32)?;
        let point = self.point(zp1, point)?;
        let mut cvt_distance = if entry == -1 {
            0
        } else {
            *self
                .cvt
                .get(usize::try_from(entry).map_err(|_| ())?)
                .ok_or(())?
        };
        if (cvt_distance.abs() - self.state.single_width).abs() < self.state.single_width_cutin {
            cvt_distance = if cvt_distance >= 0 {
                self.state.single_width
            } else {
                -self.state.single_width
            };
        }
        if zp1 == 0 {
            let origin = self.original(zp0, reference);
            let placed = (
                origin.0 + mul14(cvt_distance, self.state.freedom.0),
                origin.1 + mul14(cvt_distance, self.state.freedom.1),
            );
            self.zones[0].original[point] = placed;
            self.zones[0].current[point] = placed;
        }
        let original = self.original_distance(zp1, point, zp0, reference);
        let (c, rc) = (self.current(zp1, point), self.current(zp0, reference));
        let current = self.project(c.0 - rc.0, c.1 - rc.1);
        if self.state.auto_flip && (original ^ cvt_distance) < 0 {
            cvt_distance = -cvt_distance;
        }
        let mut distance = if opcode & 4 != 0 {
            if zp0 == zp1 && (cvt_distance - original).abs() > self.state.cvt_cutin {
                cvt_distance = original;
            }
            self.round(cvt_distance)
        } else {
            cvt_distance
        };
        if opcode & 8 != 0 {
            distance = self.minimum(original, distance);
        }
        self.move_point(zp1, point, distance - current, true);
        self.state.rp[1] = reference;
        self.state.rp[2] = point;
        if opcode & 16 != 0 {
            self.state.rp[0] = point;
        }
        Ok(())
    }

    /// 最小距离：与原始距离同号，绝对值不小于 minimum_distance。
    fn minimum(&self, original: i32, distance: i32) -> i32 {
        let minimum = self.state.minimum_distance;
        if original >= 0 {
            distance.max(minimum)
        } else {
            distance.min(-minimum)
        }
    }

    /// IUP[a]：对每个轮廓中未触碰的点，按两侧已触碰点插值或平移（FreeType `_iup_worker_interpolate`）：
    /// 位置的比较与插值比例用字体单位坐标，区间外的点按较近一侧参考点的位移平移原始坐标。
    fn interpolate_untouched(&mut self, x_axis: bool) {
        let zone = &mut self.zones[1];
        let flag = if x_axis { 1 } else { 2 };
        let coordinate = |point: (i32, i32)| if x_axis { point.0 } else { point.1 };
        let mut start = 0;
        for &end in &zone.ends {
            let touched: Vec<usize> = (start..=end)
                .filter(|index| zone.touched[*index] & flag != 0)
                .collect();
            if touched.is_empty() {
                start = end + 1;
                continue;
            }
            for (position, &first) in touched.iter().enumerate() {
                let second = touched[(position + 1) % touched.len()];
                let (u1, u2) = (
                    coordinate(zone.units[first]),
                    coordinate(zone.units[second]),
                );
                let (o1, o2) = (
                    coordinate(zone.original[first]),
                    coordinate(zone.original[second]),
                );
                let (c1, c2) = (
                    coordinate(zone.current[first]),
                    coordinate(zone.current[second]),
                );
                let ((low_u, low_o, low_c), (high_u, high_o, high_c)) = if u1 <= u2 {
                    ((u1, o1, c1), (u2, o2, c2))
                } else {
                    ((u2, o2, c2), (u1, o1, c1))
                };
                let mut index = if first == end { start } else { first + 1 };
                while index != second {
                    let unit = coordinate(zone.units[index]);
                    let original = coordinate(zone.original[index]);
                    let result = if unit <= low_u {
                        original + (low_c - low_o)
                    } else if unit >= high_u {
                        original + (high_c - high_o)
                    } else {
                        // FreeType：比例为 FT_DivFix(Δcur, Δorus)，再以 FT_MulFix 插值。
                        let ratio = div_fix(i64::from(high_c - low_c), i64::from(high_u - low_u));
                        low_c + mul_fix(unit - low_u, ratio)
                    };
                    if x_axis {
                        zone.current[index].0 = result;
                    } else {
                        zone.current[index].1 = result;
                    }
                    index = if index == end { start } else { index + 1 };
                }
            }
            start = end + 1;
        }
    }
}
