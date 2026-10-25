//! "日期/时间 属性"：月历、模拟与数字时钟。时间按 UTC 显示且只读。
use crate::{Icons, Page, inside};
use calendar::{civil_from_days, month_grid};
use canvas::Rect;
use classic_theme::{Edge, SystemColor as C};
use classic_ui::{ButtonState, Ink, Painter};

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

pub struct DateTime {
    /// 月历正在显示的年月；用户可翻页，今天的高亮只在对应月份出现。
    year: i64,
    month: u32,
}

impl DateTime {
    pub fn now() -> Self {
        let today = civil_from_days(now_seconds() / 86400);
        Self {
            year: today.year,
            month: today.month,
        }
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

impl Page for DateTime {
    fn title(&self) -> &'static str {
        "日期/时间 属性"
    }
    fn tabs(&self) -> &'static [&'static str] {
        &["日期和时间"]
    }
    fn icon(&self) -> &'static str {
        "date-time"
    }
    fn size(&self) -> (i32, i32) {
        (400, 330)
    }

    fn paint(&self, p: &mut Painter<'_>, page: Rect, _tab: usize, now: i64, _icons: &Icons) {
        let today = civil_from_days(now.div_euclid(86400));
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
            Ink::Color(C::ButtonText),
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
                let is_today =
                    (today.year, today.month, today.day) == (self.year, self.month, *day);
                if is_today {
                    p.fill(cell.inset(1), C::Hilight);
                }
                p.text_centered(
                    cell,
                    &day.to_string(),
                    Ink::Color(if is_today {
                        C::HilightText
                    } else {
                        C::WindowText
                    }),
                    false,
                );
            }
        }
        // 右：时间组（模拟时钟与数字时间）。
        let group = Rect::new(
            page.x + 8 + 7 * CELL.0 + 28,
            page.y + 6,
            page.width - (7 * CELL.0 + 28) - 16,
            8 * CELL.1 + 70,
        );
        p.group_box(group, "时间");
        let seconds = now.rem_euclid(86400);
        let (hour, minute, second) = (seconds / 3600, seconds % 3600 / 60, seconds % 60);
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
        p.text_centered(
            Rect::new(group.x, group.y + 128, group.width, 20),
            &format!("{hour}:{minute:02}:{second:02}"),
            Ink::Color(C::ButtonText),
            false,
        );
        p.text_centered(
            Rect::new(group.x, group.y + 152, group.width, 20),
            &format!("{} 年 {} 月 {} 日", today.year, today.month, today.day),
            Ink::Color(C::ButtonText),
            false,
        );
        p.text(
            page.x + 12,
            page.bottom() - p.line_height() - 8,
            "时区：UTC。系统时钟目前只能查看，不能在这里修改。",
            Ink::Disabled,
            false,
        );
    }

    fn click(&mut self, page: Rect, _tab: usize, point: (i32, i32)) -> bool {
        let [previous, next] = Self::nav_buttons(page);
        if inside(previous, point) {
            self.step_month(-1);
        } else if inside(next, point) {
            self.step_month(1);
        } else {
            return false;
        }
        true
    }
}
