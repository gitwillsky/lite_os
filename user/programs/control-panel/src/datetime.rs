//! "日期/时间 属性"：月历、模拟与数字时钟，可修改系统日期、时间与时区。
//!
//! 时间按系统本地时区（`/etc/localtime`）显示。修改日期时间走 `clock_settime`，修改时区就是改写
//! `/etc/localtime`（固定偏移的 TZif，没有夏令时）；两者都需要权限，失败时在页底给出原因。
use crate::{Icons, Page, inside};
use calendar::{civil_from_days, days_from_civil, month_grid, zone};
use canvas::Rect;
use classic_theme::{Edge, SystemColor as C};
use classic_ui::{ButtonState, Ink, Painter, SCROLLBAR_WIDTH, Scroll};

const MONTHS: [&str; 12] = [
    "一月",
    "二月",
    "三月",
    "四月",
    "五月",
    "六月",
    "七月",
    "八月",
    "九月",
    "十月",
    "十一月",
    "十二月",
];
const WEEKDAYS: [&str; 7] = ["日", "一", "二", "三", "四", "五", "六"];
const CELL: (i32, i32) = (24, 17);

/// 可选时区：`(偏移分钟, 缩写, Windows 2000 风格的显示名)`。没有时区数据库，所以只有固定偏移、不含夏令时。
const ZONES: [(i32, &str, &str); 30] = [
    (-720, "UTC-12", "(GMT-12:00) 国际日期变更线西"),
    (-660, "UTC-11", "(GMT-11:00) 中途岛，萨摩亚"),
    (-600, "UTC-10", "(GMT-10:00) 夏威夷"),
    (-540, "UTC-09", "(GMT-09:00) 阿拉斯加"),
    (-480, "UTC-08", "(GMT-08:00) 太平洋时间（美国和加拿大）"),
    (-420, "UTC-07", "(GMT-07:00) 山地时间（美国和加拿大）"),
    (-360, "UTC-06", "(GMT-06:00) 中部时间（美国和加拿大）"),
    (-300, "UTC-05", "(GMT-05:00) 东部时间（美国和加拿大）"),
    (-240, "UTC-04", "(GMT-04:00) 大西洋时间（加拿大）"),
    (-210, "UTC-03:30", "(GMT-03:30) 纽芬兰"),
    (-180, "UTC-03", "(GMT-03:00) 巴西利亚"),
    (-120, "UTC-02", "(GMT-02:00) 中大西洋"),
    (-60, "UTC-01", "(GMT-01:00) 亚速尔群岛"),
    (0, "UTC", "(GMT) 格林尼治标准时间：都柏林，爱丁堡，伦敦"),
    (60, "UTC+01", "(GMT+01:00) 柏林，巴黎，罗马，马德里"),
    (120, "UTC+02", "(GMT+02:00) 雅典，赫尔辛基，开罗"),
    (180, "UTC+03", "(GMT+03:00) 莫斯科，巴格达，科威特"),
    (210, "UTC+03:30", "(GMT+03:30) 德黑兰"),
    (240, "UTC+04", "(GMT+04:00) 阿布扎比，马斯喀特"),
    (270, "UTC+04:30", "(GMT+04:30) 喀布尔"),
    (300, "UTC+05", "(GMT+05:00) 伊斯兰堡，卡拉奇"),
    (330, "UTC+05:30", "(GMT+05:30) 马德拉斯，孟买，新德里"),
    (345, "UTC+05:45", "(GMT+05:45) 加德满都"),
    (360, "UTC+06", "(GMT+06:00) 阿斯塔纳，达卡"),
    (420, "UTC+07", "(GMT+07:00) 曼谷，河内，雅加达"),
    (
        480,
        "CST",
        "(GMT+08:00) 北京，重庆，香港特别行政区，乌鲁木齐",
    ),
    (540, "UTC+09", "(GMT+09:00) 大阪，札幌，东京，首尔"),
    (570, "UTC+09:30", "(GMT+09:30) 阿德莱德，达尔文"),
    (600, "UTC+10", "(GMT+10:00) 悉尼，墨尔本，布里斯班"),
    (720, "UTC+12", "(GMT+12:00) 惠灵顿，奥克兰，斐济"),
];
const ZONE_ROW: i32 = 17;
const ZONE_VISIBLE: i32 = 12;

pub struct DateTime {
    /// 月历正在显示的年月；用户可翻页，今天的高亮只在对应月份出现。
    year: i64,
    month: u32,
    /// 用户点选的日期 `(年, 月, 日)`；`None` 表示还没改，按今天。
    picked: Option<(i64, u32, u32)>,
    /// 用户正在编辑的时间 `[时, 分, 秒]`；`Some` 期间数字时钟与指针停止走动，应用后恢复。
    time_edit: Option<[i64; 3]>,
    /// 被选中的时间字段（0 时、1 分、2 秒）。
    field: usize,
    /// 时区列表：当前选中项（`None` 表示 `/etc/localtime` 是表外的自定义偏移）、打开时的选中项与滚动。
    zone: Option<usize>,
    zone_at_open: Option<usize>,
    zone_scroll: Scroll,
    /// 应用失败的原因，显示在页底。
    message: String,
}

impl DateTime {
    pub fn now() -> Self {
        let today = civil_from_days(zone::to_local(now_seconds()).div_euclid(86400));
        let zone_index = {
            let offset = zone::local_offset(now_seconds()) / 60;
            ZONES
                .iter()
                .position(|(minutes, ..)| i64::from(*minutes) == offset)
        };
        Self {
            year: today.year,
            month: today.month,
            picked: None,
            time_edit: None,
            field: 0,
            zone: zone_index,
            zone_at_open: zone_index,
            zone_scroll: Scroll::default(),
            message: String::new(),
        }
    }

    /// 右侧"时间"组的矩形（绘制与命中共用）。
    fn time_group(page: Rect) -> Rect {
        Rect::new(
            page.x + 8 + 7 * CELL.0 + 28,
            page.y + 6,
            page.width - (7 * CELL.0 + 28) - 16,
            8 * CELL.1 + 70,
        )
    }

    /// 数字时间的三个字段与上、下微调按钮。
    fn time_fields(page: Rect) -> ([Rect; 3], [Rect; 2]) {
        let group = Self::time_group(page);
        let width = 3 * 22 + 2 * 6;
        let left = group.x + (group.width - width - 18) / 2;
        let top = group.y + 128;
        let fields = [0, 1, 2].map(|i| Rect::new(left + 2 + i * 28, top + 1, 22, 19));
        let spin = [
            Rect::new(left + width + 2, top, 16, 10),
            Rect::new(left + width + 2, top + 11, 16, 10),
        ];
        (fields, spin)
    }

    /// 日期 `(年, 月, 日)`：点选的，否则今天（本地）。
    fn chosen_date(&self) -> (i64, u32, u32) {
        self.picked.unwrap_or_else(|| {
            let today = civil_from_days(zone::to_local(now_seconds()).div_euclid(86400));
            (today.year, today.month, today.day)
        })
    }

    fn live_time() -> [i64; 3] {
        let seconds = zone::to_local(now_seconds()).rem_euclid(86400);
        [seconds / 3600, seconds % 3600 / 60, seconds % 60]
    }

    /// 微调选中的时间字段（时 0..24、分与秒 0..60，循环）。
    fn spin(&mut self, delta: i64) {
        let mut time = self.time_edit.unwrap_or_else(Self::live_time);
        let modulus = if self.field == 0 { 24 } else { 60 };
        time[self.field] = (time[self.field] + delta).rem_euclid(modulus);
        self.time_edit = Some(time);
    }

    /// 月历里 `point` 命中的日期。
    fn day_at(&self, page: Rect, point: (i32, i32)) -> Option<u32> {
        let (x0, y0) = Self::calendar_origin(page);
        month_grid(self.year, self.month)
            .iter()
            .enumerate()
            .flat_map(|(row, week)| {
                week.iter().enumerate().filter_map(move |(column, day)| {
                    day.map(|day| {
                        (
                            Rect::new(
                                x0 + column as i32 * CELL.0,
                                y0 + 3 + (row as i32 + 1) * CELL.1,
                                CELL.0,
                                CELL.1,
                            ),
                            day,
                        )
                    })
                })
            })
            .find_map(|(cell, day)| inside(cell, point).then_some(day))
    }

    /// 时区列表的行区域与滚动条。
    fn zone_view(page: Rect) -> (Rect, Rect) {
        let list = Rect::new(
            page.x + 12,
            page.y + 40,
            page.width - 24,
            ZONE_VISIBLE * ZONE_ROW + 4,
        );
        let inner = list.inset(2);
        (
            Rect::new(
                inner.x,
                inner.y,
                inner.width - SCROLLBAR_WIDTH,
                inner.height,
            ),
            Rect::new(
                inner.right() - SCROLLBAR_WIDTH,
                inner.y,
                SCROLLBAR_WIDTH,
                inner.height,
            ),
        )
    }

    fn calendar_origin(page: Rect) -> (i32, i32) {
        (page.x + 16, page.y + 52)
    }

    fn nav_buttons(page: Rect) -> [Rect; 2] {
        [
            Rect::new(page.x + 16 + 7 * CELL.0 - 40, page.y + 22, 18, 18),
            Rect::new(page.x + 16 + 7 * CELL.0 - 20, page.y + 22, 18, 18),
        ]
    }

    fn step_month(&mut self, delta: i32) {
        let index = self.year * 12 + i64::from(self.month) - 1 + i64::from(delta);
        self.year = index.div_euclid(12);
        self.month = index.rem_euclid(12) as u32 + 1;
    }
}

fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// 单位圆上第 `step` 个 1/60 圈（自 12 点顺时针）的方向。
fn direction(step: f64) -> (f64, f64) {
    let angle = step / 60. * std::f64::consts::TAU;
    (angle.sin(), -angle.cos())
}

fn line(p: &mut Painter<'_>, from: (i32, i32), to: (i32, i32), color: C) {
    let (dx, dy) = (to.0 - from.0, to.1 - from.1);
    let steps = dx.abs().max(dy.abs()).max(1);
    for i in 0..=steps {
        p.fill(
            Rect::new(from.0 + dx * i / steps, from.1 + dy * i / steps, 1, 1),
            color,
        );
    }
}

/// 在 `rect` 里画一个微调箭头（`up` 向上）：用逐行变宽的横条拼出三角形。
fn arrow(p: &mut Painter<'_>, rect: Rect, up: bool, pressed: bool) {
    p.push_button(
        rect,
        "",
        ButtonState {
            pressed,
            ..Default::default()
        },
    );
    let (cx, cy) = (rect.x + rect.width / 2, rect.y + rect.height / 2);
    for row in 0..3 {
        let half = if up { row } else { 2 - row };
        let y = cy - 1 + row;
        p.fill(Rect::new(cx - half, y, 2 * half + 1, 1), C::ButtonText);
    }
}

impl Page for DateTime {
    fn title(&self) -> &'static str {
        "日期/时间 属性"
    }
    fn tabs(&self) -> &'static [&'static str] {
        &["日期和时间", "时区"]
    }
    fn icon(&self) -> &'static str {
        "date-time"
    }
    fn size(&self) -> (i32, i32) {
        (400, 330)
    }

    fn paint(&self, p: &mut Painter<'_>, page: Rect, tab: usize, now: i64, _icons: &Icons) {
        let ink = Ink::Color(C::ButtonText);
        if tab == 1 {
            self.paint_zones(p, page);
            return;
        }
        let local = zone::to_local(now);
        let today = civil_from_days(local.div_euclid(86400));
        let (picked_year, picked_month, picked_day) = self.chosen_date();
        // 左：日期组（月份标题、翻页、星期行与月历）。
        let (x0, y0) = Self::calendar_origin(page);
        p.group_box(
            Rect::new(page.x + 8, page.y + 6, 7 * CELL.0 + 16, 8 * CELL.1 + 70),
            "日期",
        );
        p.text(
            x0,
            page.y + 25,
            &format!("{} {}", MONTHS[self.month as usize - 1], self.year),
            ink,
            true,
        );
        for (rect, label) in Self::nav_buttons(page).into_iter().zip(["<", ">"]) {
            p.push_button(rect, label, ButtonState::default());
        }
        let grid_frame = Rect::new(x0 - 1, y0 - 1, 7 * CELL.0 + 2, 7 * CELL.1 + 2 + 3);
        let inner = p.edge(grid_frame, Edge::Sunken);
        p.fill(inner, C::Window);
        for (column, name) in WEEKDAYS.iter().enumerate() {
            let cell = Rect::new(x0 + column as i32 * CELL.0, y0, CELL.0, CELL.1);
            p.text_centered(cell, name, Ink::Color(C::WindowText), false);
        }
        p.fill(
            Rect::new(x0 + 2, y0 + CELL.1, 7 * CELL.0 - 4, 1),
            C::ButtonShadow,
        );
        for (row, week) in month_grid(self.year, self.month).iter().enumerate() {
            for (column, day) in week.iter().enumerate() {
                let Some(day) = day else { continue };
                let cell = Rect::new(
                    x0 + column as i32 * CELL.0,
                    y0 + 3 + (row as i32 + 1) * CELL.1,
                    CELL.0,
                    CELL.1,
                );
                let chosen =
                    (picked_year, picked_month, picked_day) == (self.year, self.month, *day);
                if chosen {
                    p.fill(cell.inset(1), C::Hilight);
                } else if (today.year, today.month, today.day) == (self.year, self.month, *day) {
                    p.outline(cell.inset(1), C::WindowText);
                }
                p.text_centered(
                    cell,
                    &day.to_string(),
                    Ink::Color(if chosen {
                        C::HilightText
                    } else {
                        C::WindowText
                    }),
                    false,
                );
            }
        }
        // 右：时间组（模拟时钟与可编辑的数字时间）。
        let group = Self::time_group(page);
        p.group_box(group, "时间");
        let [hour, minute, second] = self.time_edit.unwrap_or_else(|| {
            let seconds = local.rem_euclid(86400);
            [seconds / 3600, seconds % 3600 / 60, seconds % 60]
        });
        let center = (group.x + group.width / 2, group.y + 70);
        let radius = 48;
        for row in -radius..=radius {
            let half = (f64::from(radius * radius - row * row)).sqrt() as i32;
            p.fill(
                Rect::new(center.0 - half, center.1 + row, 2 * half + 1, 1),
                C::Window,
            );
        }
        for tick in 0..60 {
            let (dx, dy) = direction(f64::from(tick));
            let (inner_r, color) = if tick % 5 == 0 {
                (41., C::WindowText)
            } else {
                (45., C::ButtonShadow)
            };
            let at = |r: f64| {
                (
                    center.0 + (dx * r).round() as i32,
                    center.1 + (dy * r).round() as i32,
                )
            };
            line(p, at(inner_r), at(47.), color);
        }
        let hand = |p: &mut Painter<'_>, step: f64, length: f64, color: C| {
            let (dx, dy) = direction(step);
            let end = (
                center.0 + (dx * length).round() as i32,
                center.1 + (dy * length).round() as i32,
            );
            line(p, center, end, color);
        };
        hand(
            p,
            (hour % 12) as f64 * 5. + minute as f64 / 12.,
            28.,
            C::WindowText,
        );
        hand(p, minute as f64 + second as f64 / 60., 38., C::WindowText);
        hand(p, second as f64, 40., C::ButtonShadow);
        // 数字时间：三个字段放在凹陷框里，选中的字段反白，右侧是上下微调。
        let (fields, spin) = Self::time_fields(page);
        let frame = Rect::new(
            fields[0].x - 3,
            fields[0].y - 1,
            fields[2].right() - fields[0].x + 6,
            21,
        );
        let box_inner = p.edge(frame, Edge::Sunken);
        p.fill(box_inner, C::Window);
        for (index, (rect, value)) in fields.iter().zip([hour, minute, second]).enumerate() {
            let selected = self.field == index;
            if selected {
                p.fill(*rect, C::Hilight);
            }
            p.text_centered(
                *rect,
                &format!("{value:02}"),
                Ink::Color(if selected {
                    C::HilightText
                } else {
                    C::WindowText
                }),
                false,
            );
            if index < 2 {
                p.text(
                    rect.right() + 1,
                    rect.y + 2,
                    ":",
                    Ink::Color(C::WindowText),
                    false,
                );
            }
        }
        arrow(p, spin[0], true, false);
        arrow(p, spin[1], false, false);
        p.text_centered(
            Rect::new(group.x, group.y + 152, group.width, 20),
            &format!("{} 年 {} 月 {} 日", picked_year, picked_month, picked_day),
            ink,
            false,
        );
        let footer = if self.message.is_empty() {
            let zone_name = self.zone.map_or_else(
                || zone::abbreviation(now),
                |index| ZONES[index].2.to_owned(),
            );
            format!("当前时区：{zone_name}")
        } else {
            self.message.clone()
        };
        p.text_fit(
            page.x + 12,
            page.bottom() - p.line_height() - 8,
            page.width - 24,
            &footer,
            if self.message.is_empty() {
                Ink::Disabled
            } else {
                ink
            },
            false,
        );
    }

    fn click(&mut self, page: Rect, tab: usize, point: (i32, i32)) -> bool {
        self.message.clear();
        if tab == 1 {
            let (rows, bar) = Self::zone_view(page);
            let content = ZONES.len() as i32 * ZONE_ROW;
            if inside(bar, point) {
                self.zone_scroll.press(
                    bar,
                    true,
                    (content, ZONE_VISIBLE * ZONE_ROW, ZONE_ROW),
                    point,
                );
            } else if inside(rows, point) {
                let index =
                    ((point.1 - rows.y - 2 + self.zone_scroll.position) / ZONE_ROW).max(0) as usize;
                if index < ZONES.len() {
                    self.zone = Some(index);
                }
            }
            return true;
        }
        let [previous, next] = Self::nav_buttons(page);
        if inside(previous, point) {
            self.step_month(-1);
        } else if inside(next, point) {
            self.step_month(1);
        } else if let Some(day) = self.day_at(page, point) {
            self.picked = Some((self.year, self.month, day));
        } else {
            let (fields, spin) = Self::time_fields(page);
            if let Some(index) = fields.iter().position(|rect| inside(*rect, point)) {
                self.field = index;
            } else if inside(spin[0], point) {
                self.spin(1);
            } else if inside(spin[1], point) {
                self.spin(-1);
            } else {
                return false;
            }
        }
        true
    }

    fn wheel(&mut self, page: Rect, tab: usize, point: (i32, i32), notches: i32) {
        if tab == 1 {
            let (rows, bar) = Self::zone_view(page);
            if inside(rows, point) || inside(bar, point) {
                let content = ZONES.len() as i32 * ZONE_ROW;
                self.zone_scroll
                    .wheel(notches, ZONE_ROW, content - ZONE_VISIBLE * ZONE_ROW);
            }
            return;
        }
        // 在数字时间框上滚轮就是微调选中的字段（向上滚增加）。
        let (fields, spin) = Self::time_fields(page);
        if fields.iter().chain(&spin).any(|rect| inside(*rect, point)) {
            self.spin(-i64::from(notches));
        }
    }

    fn motion(&mut self, page: Rect, tab: usize, point: (i32, i32)) {
        if tab == 1 {
            let (_, bar) = Self::zone_view(page);
            let content = ZONES.len() as i32 * ZONE_ROW;
            self.zone_scroll
                .drag(bar, true, (content, ZONE_VISIBLE * ZONE_ROW), point);
        }
    }

    fn release(&mut self) {
        self.zone_scroll.release();
    }

    fn dirty(&self) -> bool {
        self.picked.is_some() || self.time_edit.is_some() || self.zone != self.zone_at_open
    }

    fn apply(&mut self) {
        self.message.clear();
        // 先改时区再设时钟：时钟的“本地时间”要按新时区换算成 UTC。
        if self.zone != self.zone_at_open
            && let Some(index) = self.zone
        {
            let (minutes, abbreviation, _) = ZONES[index];
            match zone::write_fixed(minutes * 60, abbreviation) {
                Ok(()) => self.zone_at_open = self.zone,
                Err(error) => {
                    self.message = format!("无法更改时区：{error}");
                    return;
                }
            }
        }
        if self.picked.is_some() || self.time_edit.is_some() {
            let (year, month, day) = self.chosen_date();
            let [hour, minute, second] = self.time_edit.unwrap_or_else(Self::live_time);
            let days = days_from_civil(calendar::Date { year, month, day });
            let unix = zone::from_local(days * 86400 + hour * 3600 + minute * 60 + second);
            match zone::set_clock(unix) {
                Ok(()) => {
                    self.picked = None;
                    self.time_edit = None;
                }
                Err(error) => self.message = format!("无法设置系统时间：{error}"),
            }
        }
    }
}

impl DateTime {
    /// "时区"页：说明文字与可滚动的时区列表。
    fn paint_zones(&self, p: &mut Painter<'_>, page: Rect) {
        let ink = Ink::Color(C::ButtonText);
        p.text_fit(
            page.x + 12,
            page.y + 14,
            page.width - 24,
            "选择所在时区。系统没有时区数据库，因此只有固定偏移、不含夏令时。",
            ink,
            false,
        );
        let list = Rect::new(
            page.x + 12,
            page.y + 40,
            page.width - 24,
            ZONE_VISIBLE * ZONE_ROW + 4,
        );
        let inner = p.edge(list, Edge::Sunken);
        p.fill(inner, C::Window);
        let (rows, bar) = Self::zone_view(page);
        p.clipped(rows, |p| {
            for (index, (_, _, name)) in ZONES.iter().enumerate() {
                let y = rows.y + 2 + index as i32 * ZONE_ROW - self.zone_scroll.position;
                let chosen = self.zone == Some(index);
                if chosen {
                    p.fill(
                        Rect::new(rows.x + 1, y, rows.width - 2, ZONE_ROW),
                        C::Hilight,
                    );
                }
                p.text_fit(
                    rows.x + 4,
                    y + (ZONE_ROW - p.line_height()) / 2,
                    rows.width - 8,
                    name,
                    Ink::Color(if chosen {
                        C::HilightText
                    } else {
                        C::WindowText
                    }),
                    false,
                );
            }
        });
        let content = ZONES.len() as i32 * ZONE_ROW;
        p.scrollbar(
            bar,
            true,
            (
                content - 1,
                ZONE_VISIBLE * ZONE_ROW,
                self.zone_scroll.position,
            ),
            self.zone_scroll.pressed(),
        );
        if self.zone.is_none() {
            p.text_fit(
                page.x + 12,
                list.bottom() + 8,
                page.width - 24,
                "当前时区不在列表中（自定义偏移）。",
                Ink::Disabled,
                false,
            );
        }
    }
}
