//! “日期/时间 属性”：按 Windows 2000 SP4 简中 `timedate.cpl` 对话框 1（时间和日期）与 2（时区）的控件与布局还原。
//! 对话框单位按宋体 9 磅换算（1 DLU = 1.5 逻辑像素）；草稿采用共享控件，标准 TZif/系统时钟写入在属性表后台应用阶段完成。
use crate::zones::{Transition, ZONES, Zone};
use crate::{Activity, Apply, Page, child};
use calendar::{civil_from_days, days_from_civil, zone};
use liteos_ui::{
    Background, Clock, ColorRole, Date, Draw, Event, EventKind, Layout, PropertySheet, Range, Rect,
    TreeRow, ValidationError, Widget, WidgetKind as Kind, state,
};
use std::{
    cell::Cell,
    rc::Rc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

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
/// timedate.cpl 接受的年份范围。
const YEARS: (i32, i32) = (1980, 2099);
/// 时钟控件边长（ClockWndMain 90 DLU）。
const CLOCK: i32 = 135;

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64)
}
fn local() -> (Date, [u32; 3]) {
    let seconds = zone::to_local(now());
    let date = civil_from_days(seconds.div_euclid(86400));
    let rest = seconds.rem_euclid(86400);
    (
        Date {
            year: date.year as i32,
            month: date.month,
            day: date.day,
        },
        [
            (rest / 3600) as u32,
            (rest % 3600 / 60) as u32,
            (rest % 60) as u32,
        ],
    )
}

/// POSIX TZ 偏移：西为正的小时[:分钟]。
fn posix_offset(bias: i32) -> String {
    let sign = if bias < 0 { "-" } else { "" };
    let (hours, minutes) = (bias.abs() / 60, bias.abs() % 60);
    if minutes == 0 {
        format!("{sign}{hours}")
    } else {
        format!("{sign}{hours}:{minutes:02}")
    }
}

fn posix_transition(transition: Transition) -> String {
    let time = if transition.minute == 0 {
        format!("{}", transition.hour)
    } else {
        format!("{}:{:02}", transition.hour, transition.minute)
    };
    format!(
        "M{}.{}.{}/{}",
        transition.month, transition.week, transition.weekday, time
    )
}

/// 时区的 POSIX TZ 规则（RFC 8536 尾部）；`daylight` 为“根据夏时制自动调节时钟”。
/// Windows 的切换时刻是切换前的本地墙钟时间，与 POSIX 的约定一致。
fn posix_rule(zone: &Zone, daylight: bool) -> String {
    let standard = format!("{}{}", zone.abbreviation.0, posix_offset(zone.bias));
    match zone.rule.filter(|_| daylight) {
        Some((start, end)) => {
            let offset = if zone.daylight_bias == -60 {
                String::new()
            } else {
                posix_offset(zone.bias + zone.daylight_bias)
            };
            format!(
                "{standard}{}{offset},{},{}",
                zone.abbreviation.1,
                posix_transition(start),
                posix_transition(end)
            )
        }
        None => standard,
    }
}

/// 当前 `/etc/localtime` 对应的时区下标与是否启用夏令时：按（偏移，缩写）匹配，缩写不认识时退回第一个同偏移的时区。
fn saved_zone() -> Option<(usize, bool)> {
    let unix = now();
    let offset = zone::local_offset(unix);
    let abbreviation = zone::abbreviation(unix);
    let year = civil_from_days(zone::to_local(unix).div_euclid(86400)).year;
    let probe = |month| {
        days_from_civil(calendar::Date {
            year,
            month,
            day: 15,
        }) * 86400
    };
    let daylight = zone::local_offset(probe(1)) != zone::local_offset(probe(7));
    let standard = |zone: &Zone| i64::from(-zone.bias) * 60;
    let summer = |zone: &Zone| i64::from(-(zone.bias + zone.daylight_bias)) * 60;
    ZONES
        .iter()
        .position(|zone| {
            (zone.abbreviation.0 == abbreviation && standard(zone) == offset)
                || (zone.abbreviation.1 == abbreviation && summer(zone) == offset)
        })
        .or_else(|| {
            ZONES
                .iter()
                .position(|zone| standard(zone) == offset || summer(zone) == offset)
        })
        .map(|index| (index, daylight))
}

/// 地图的业务配色：原版 MapID 保留区域身份，蓝海绿陆为用户选择的视觉策略。
/// 同一时区的海陆两类区域分别加亮；缺少当前时区时只显示底图。
fn map_palette(selected: Option<usize>) -> [u32; 256] {
    let mut palette = [0x5596c7; 256];
    for zone in &ZONES {
        if let Some(land) = zone.map_regions[1] {
            palette[usize::from(land)] = 0x82ad69;
        }
    }
    if let Some(index) = selected {
        for (region, color) in ZONES[index]
            .map_regions
            .into_iter()
            .zip([0x91cef3, 0xd0df79])
        {
            if let Some(region) = region {
                palette[usize::from(region)] = color;
            }
        }
    }
    palette
}

pub struct DateTime {
    _controls: Vec<Widget>,
    page: Widget,
    month: Widget,
    year: Widget,
    year_spin: Widget,
    calendar: Widget,
    clock: Widget,
    time: [Widget; 3],
    time_spin: Widget,
    /// 微调钮与上下方向键作用的时间字段（最后点击或输入的字段；默认小时）。
    active: Cell<usize>,
    zones: Widget,
    map: Widget,
    daylight: Widget,
    saved_zone: Option<(usize, bool)>,
    date_changed: Rc<Cell<bool>>,
    time_changed: Rc<Cell<bool>>,
}

fn fixed(width: i32, height: i32) -> Layout {
    Layout {
        width,
        height,
        gap: 0,
        ..Default::default()
    }
}

fn spacer(parent: &Widget) -> liteos_ui::Result<Widget> {
    child(
        parent,
        Kind::Column,
        "",
        Layout {
            grow: 1,
            gap: 0,
            ..Default::default()
        },
    )
}

impl DateTime {
    pub fn build(sheet: &PropertySheet) -> liteos_ui::Result<Self> {
        let (date, time) = local();
        let mut controls = Vec::new();
        let page = sheet.page("时间和日期")?;
        let row = child(
            &page,
            Kind::Row,
            "",
            Layout {
                gap: 9,
                ..Default::default()
            },
        )?;
        // “日期(D)”分组：122×125 DLU。
        let date_group = child(
            &row,
            Kind::Group,
            "日期(D)",
            Layout {
                width: 183,
                height: 188,
                padding: 9,
                gap: 10,
                ..Default::default()
            },
        )?;
        let selectors = child(
            &date_group,
            Kind::Row,
            "",
            Layout {
                gap: 10,
                ..Default::default()
            },
        )?;
        let month = child(
            &selectors,
            Kind::Combo,
            MONTHS[date.month as usize - 1],
            fixed(75, 21),
        )?;
        month.set_state(state::DROPDOWN_LIST)?;
        month.set_tree_rows(
            &MONTHS
                .iter()
                .enumerate()
                .map(|(index, text)| TreeRow {
                    id: index as u64 + 1,
                    parent: 0,
                    text,
                    expanded: false,
                    has_children: false,
                })
                .collect::<Vec<_>>(),
        )?;
        month.select(Some(u64::from(date.month)))?;
        let year_box = child(&selectors, Kind::Row, "", fixed(-1, 20))?;
        let year = child(&year_box, Kind::Edit, &date.year.to_string(), fixed(63, 20))?;
        let year_spin = child(&year_box, Kind::UpDown, "", fixed(12, 20))?;
        year_spin.set_range(Range {
            minimum: YEARS.0,
            maximum: YEARS.1,
            value: date.year,
            page: 1,
            step: 1,
            vertical: 1,
        })?;
        let calendar = child(&date_group, Kind::Calendar, "", fixed(162, 120))?;
        calendar.set_date(date)?;

        // “时间(T)”分组：113×125 DLU，时钟 90×90 DLU，下面是 hh:mm:ss 与微调钮。
        let time_group = child(
            &row,
            Kind::Group,
            "时间(T)",
            Layout {
                width: 170,
                height: 188,
                padding: 6,
                gap: 3,
                ..Default::default()
            },
        )?;
        let clock_row = child(&time_group, Kind::Row, "", fixed(-1, CLOCK))?;
        let left = spacer(&clock_row)?;
        let clock = child(&clock_row, Kind::Canvas, "", fixed(CLOCK, CLOCK))?;
        let right = spacer(&clock_row)?;
        let picker_row = child(&time_group, Kind::Row, "", fixed(-1, 18))?;
        let picker_left = spacer(&picker_row)?;
        // 时间框：凹陷白底，内含无边框的时、分、秒字段与冒号（timedate 的复合编辑框）。
        let picker = child(
            &picker_row,
            Kind::Row,
            "",
            Layout {
                width: 106,
                height: 18,
                padding: 1,
                gap: 0,
                ..Default::default()
            },
        )?;
        picker.set_state(state::CLIENT_EDGE)?;
        picker.set_background(Some(0xFF_FFFF), None)?;
        let mut fields = Vec::new();
        for (index, value) in time.iter().enumerate() {
            if index > 0 {
                controls.push(child(&picker, Kind::Label, ":", fixed(6, -1))?);
            }
            let field = child(&picker, Kind::Edit, &format!("{value:02}"), fixed(16, -1))?;
            field.set_state(state::BORDERLESS)?;
            fields.push(field);
        }
        let time_spin = child(&picker_row, Kind::UpDown, "", fixed(11, 18))?;
        let picker_right = spacer(&picker_row)?;
        let saved_zone = saved_zone();
        // “当前时区”在夏令时期间显示夏季时间名（timedate.cpl 的 %s 取当前生效的名称）。
        let abbreviation = zone::abbreviation(now());
        let current = saved_zone.map_or_else(
            || abbreviation.clone(),
            |(index, _)| {
                let zone = &ZONES[index];
                if zone.abbreviation.1 == abbreviation {
                    zone.daylight.to_owned()
                } else {
                    zone.standard.to_owned()
                }
            },
        );
        controls.push(child(
            &page,
            Kind::Label,
            &format!("当前时区: {current}"),
            Layout::default(),
        )?);

        // 时区页：时区组合框、世界地图、夏令时复选框。
        let zones_page = sheet.page("时区")?;
        let zones = child(&zones_page, Kind::Combo, "", fixed(-1, 21))?;
        zones.set_state(state::DROPDOWN_LIST)?;
        zones.set_tree_rows(
            &ZONES
                .iter()
                .enumerate()
                .map(|(index, zone)| TreeRow {
                    id: index as u64 + 1,
                    parent: 0,
                    text: zone.display,
                    expanded: false,
                    has_children: false,
                })
                .collect::<Vec<_>>(),
        )?;
        let map_row = child(&zones_page, Kind::Row, "", fixed(-1, 184))?;
        let map_left = spacer(&map_row)?;
        let map = child(&map_row, Kind::Image, "worldmap-1x", fixed(-1, -1))?;
        let map_right = spacer(&map_row)?;
        let daylight = child(
            &zones_page,
            Kind::CheckBox,
            "根据夏时制自动调节时钟(D)",
            Layout::default(),
        )?;
        controls.extend([
            row,
            date_group,
            selectors,
            year_box,
            time_group,
            clock_row,
            left,
            right,
            picker_row,
            picker_left,
            picker,
            picker_right,
            zones_page,
            map_row,
            map_left,
            map_right,
        ]);
        let page = Self {
            _controls: controls,
            page,
            month,
            year,
            year_spin,
            calendar,
            clock,
            time: fields.try_into().ok().expect("three clock fields"),
            time_spin,
            active: Cell::new(0),
            zones,
            map,
            daylight,
            saved_zone,
            date_changed: Rc::new(Cell::new(false)),
            time_changed: Rc::new(Cell::new(false)),
        };
        page.show_zone(saved_zone)?;
        page.sync_spin()?;
        Ok(page)
    }

    /// 显示时区选择与夏令时复选框：没有夏令时规则的时区复选框不可用（与 timedate.cpl 相同）。
    fn show_zone(&self, zone: Option<(usize, bool)>) -> liteos_ui::Result<()> {
        self.zones.select(zone.map(|(index, _)| index as u64 + 1))?;
        if let Some((index, _)) = zone {
            self.zones.set_text(ZONES[index].display)?;
        }
        let has_rule = zone.is_some_and(|(index, _)| ZONES[index].rule.is_some());
        let mut flags = 0;
        if !has_rule {
            flags |= state::DISABLED;
        }
        if zone.is_none_or(|(_, daylight)| daylight) && has_rule {
            flags |= state::CHECKED;
        }
        self.daylight.set_state(flags)?;
        self.map
            .set_image_palette(&map_palette(zone.map(|(index, _)| index)))
    }

    fn selected_zone(&self) -> liteos_ui::Result<Option<(usize, bool)>> {
        let daylight = self.daylight.state()? & state::CHECKED != 0;
        Ok(self
            .zones
            .selection()?
            .map(|id| (id as usize - 1, daylight)))
    }

    /// 让时间微调钮的范围与值对应当前字段。
    fn sync_spin(&self) -> liteos_ui::Result<()> {
        let index = self.active.get();
        let maximum = if index == 0 { 23 } else { 59 };
        let value = self.time[index]
            .text()?
            .trim()
            .parse::<i32>()
            .unwrap_or(0)
            .clamp(0, maximum);
        self.time_spin.set_range(Range {
            minimum: 0,
            maximum,
            value,
            page: 1,
            step: 1,
            vertical: 1,
        })
    }

    fn set_field(&self, index: usize, value: i32) -> liteos_ui::Result<()> {
        let maximum = if index == 0 { 24 } else { 60 };
        self.time[index].set_text(&format!("{:02}", value.rem_euclid(maximum)))?;
        self.time_changed.set(true);
        self.sync_spin()
    }

    /// 月份或年份改变：日历换到该月，日不超过该月天数。
    fn show_month(&self, year: i32, month: u32) -> liteos_ui::Result<()> {
        let current = self.calendar.date()?;
        let day = current
            .day
            .min(calendar::days_in_month(i64::from(year), month));
        self.calendar.set_date(Date { year, month, day })?;
        self.date_changed.set(true);
        Ok(())
    }

    fn times(&self) -> Result<[u32; 3], ValidationError> {
        let mut values = [0; 3];
        for index in 0..3 {
            values[index] = self.time[index]
                .text()
                .ok()
                .and_then(|text| text.trim().parse::<u32>().ok())
                .filter(|value| *value < if index == 0 { 24 } else { 60 })
                .ok_or_else(|| ValidationError {
                    page: Some(self.page.clone()),
                    control: Some(self.time[index].clone()),
                    message: if index == 0 {
                        "小时必须在 0 到 23 之间。"
                    } else {
                        "分和秒必须在 0 到 59 之间。"
                    }
                    .into(),
                })?;
        }
        Ok(values)
    }
}

fn fill(commands: &mut Vec<Draw<'static>>, rect: Rect, color: u32) {
    commands.push(Draw {
        kind: 1,
        rect,
        color,
        bold: false,
        text: "",
    });
}

/// 扫描线填充凸多边形（时钟指针）。
fn polygon(commands: &mut Vec<Draw<'static>>, points: &[(f64, f64)], color: u32) {
    let top = points.iter().map(|p| p.1).fold(f64::MAX, f64::min).floor() as i32;
    let bottom = points.iter().map(|p| p.1).fold(f64::MIN, f64::max).ceil() as i32;
    for y in top..=bottom {
        let line = f64::from(y) + 0.5;
        let mut crossings: Vec<f64> = points
            .iter()
            .zip(points.iter().cycle().skip(1))
            .filter(|(a, b)| (a.1 <= line) != (b.1 <= line))
            .map(|(a, b)| a.0 + (line - a.1) * (b.0 - a.0) / (b.1 - a.1))
            .collect();
        crossings.sort_by(f64::total_cmp);
        for pair in crossings.chunks(2) {
            if let [left, right] = pair {
                let (left, right) = (left.round() as i32, right.round() as i32);
                fill(
                    commands,
                    Rect::new(left, y, (right - left).max(1), 1),
                    color,
                );
            }
        }
    }
}

/// Windows 2000 timedate 的时钟：按钮面底，12 个凸起小方块做整点刻度、其余分钟为阴影色点，
/// 时针与分针是黑色菱形指针，秒针为阴影色细针。
fn paint_clock(clock: &Widget, time: [u32; 3]) -> liteos_ui::Result<()> {
    let area = clock.geometry()?;
    if area.width <= 0 || area.height <= 0 {
        return Ok(());
    }
    let face = clock.color(ColorRole::Control)?;
    let (light, shadow) = (0xFF_FFFF, 0x80_8080);
    let radius = f64::from(area.width.min(area.height)) / 2.0 - 4.0;
    let center = (f64::from(area.width) / 2.0, f64::from(area.height) / 2.0);
    let mut commands = Vec::new();
    fill(
        &mut commands,
        Rect::new(0, 0, area.width, area.height),
        face,
    );
    let at = |step: f64, length: f64| {
        let angle = step * std::f64::consts::TAU / 60.0 - std::f64::consts::FRAC_PI_2;
        (
            center.0 + angle.cos() * length,
            center.1 + angle.sin() * length,
        )
    };
    for tick in 0..60 {
        let (x, y) = at(f64::from(tick), radius - 2.0);
        let (x, y) = (x.round() as i32, y.round() as i32);
        if tick % 5 == 0 {
            // 凸起小方块：高光在左上，阴影在右下。
            fill(&mut commands, Rect::new(x - 2, y - 2, 4, 4), shadow);
            fill(&mut commands, Rect::new(x - 2, y - 2, 3, 3), light);
            fill(&mut commands, Rect::new(x - 1, y - 1, 2, 2), face);
        } else {
            fill(&mut commands, Rect::new(x, y, 1, 1), shadow);
        }
    }
    let hand =
        |commands: &mut Vec<Draw<'static>>, step: f64, length: f64, width: f64, color: u32| {
            let tip = at(step, length);
            let tail = at(step + 30.0, width * 1.5);
            let left = at(step - 15.0, width);
            let right = at(step + 15.0, width);
            polygon(commands, &[tip, right, tail, left], color);
        };
    let hours = f64::from(time[0] % 12) * 5.0 + f64::from(time[1]) / 12.0;
    hand(&mut commands, hours, radius * 0.5, 4.0, 0);
    hand(&mut commands, f64::from(time[1]), radius * 0.78, 3.0, 0);
    let second = at(f64::from(time[2]), radius * 0.82);
    polygon(
        &mut commands,
        &[
            second,
            (center.0 + 0.5, center.1),
            (center.0 - 0.5, center.1),
        ],
        shadow,
    );
    clock.set_drawing(&commands)
}

impl Page for DateTime {
    fn event(&mut self, event: &Event) -> liteos_ui::Result<Option<crate::Response>> {
        let node = event.node;
        let field = self.time.iter().position(|field| field.id() == node);
        match event.kind {
            EventKind::Select { row } if node == self.month.id() => {
                let year = self.calendar.date()?.year;
                self.show_month(year, row as u32)?;
            }
            EventKind::Select { .. } if node == self.zones.id() => {
                // 换时区时按新时区是否有夏令时规则刷新复选框，默认勾选。
                let selected = self.selected_zone()?.map(|(index, _)| (index, true));
                self.show_zone(selected)?;
            }
            EventKind::Changed { value } if node == self.year_spin.id() => {
                self.year.set_text(&value.to_string())?;
                let month = self.calendar.date()?.month;
                self.show_month(value as i32, month)?;
            }
            EventKind::Changed { .. } if node == self.year.id() => {
                if let Ok(year) = self.year.text()?.trim().parse::<i32>()
                    && (YEARS.0..=YEARS.1).contains(&year)
                {
                    let month = self.calendar.date()?.month;
                    self.show_month(year, month)?;
                    self.year_spin.set_range(Range {
                        minimum: YEARS.0,
                        maximum: YEARS.1,
                        value: year,
                        page: 1,
                        step: 1,
                        vertical: 1,
                    })?;
                }
            }
            EventKind::Changed { .. } if node == self.calendar.id() => {
                // 日历中用方向键跨月时，月份组合框与年份随之更新。
                let date = self.calendar.date()?;
                self.month.select(Some(u64::from(date.month)))?;
                self.month.set_text(MONTHS[date.month as usize - 1])?;
                self.year.set_text(&date.year.to_string())?;
                self.date_changed.set(true);
            }
            EventKind::Changed { value } if node == self.time_spin.id() => {
                self.set_field(self.active.get(), value as i32)?;
            }
            EventKind::Changed { .. } if field.is_some() => {
                self.time_changed.set(true);
                self.sync_spin()?;
            }
            EventKind::Button { pressed: true, .. } if field.is_some() => {
                self.active.set(field.expect("time field"));
                self.sync_spin()?;
            }
            EventKind::Key {
                code,
                pressed: true,
                ..
            } if field.is_some() => {
                let index = field.expect("time field");
                self.active.set(index);
                // UDS_ARROWKEYS：上下方向键调整当前字段。
                if matches!(code, 103 | 108) {
                    let value = self.time[index].text()?.trim().parse::<i32>().unwrap_or(0);
                    self.set_field(index, value + if code == 103 { 1 } else { -1 })?;
                } else {
                    self.sync_spin()?;
                }
            }
            _ => {}
        }
        if let Ok(time) = self.times() {
            paint_clock(&self.clock, time)?;
        }
        Ok(None)
    }
    fn dirty(&self) -> liteos_ui::Result<bool> {
        Ok(self.date_changed.get()
            || self.time_changed.get()
            || self.selected_zone()? != self.saved_zone)
    }
    fn validate(&self) -> Result<(), ValidationError> {
        self.times().map(drop)
    }
    fn apply(&self) -> liteos_ui::Result<Apply> {
        let date = self.calendar.date()?;
        let time = self
            .times()
            .map_err(|error| liteos_ui::Error::invalid(error.message))?;
        let clock_change = self.date_changed.get() || self.time_changed.get();
        let time_change = self.time_changed.get();
        let selected = self.selected_zone()?;
        let zone_change = selected.filter(|selected| Some(*selected) != self.saved_zone);
        Ok(Box::new(move || {
            if let Some((index, daylight)) = zone_change {
                let zone = &ZONES[index];
                zone::write_zone(
                    -zone.bias * 60,
                    zone.abbreviation.0,
                    Some(&posix_rule(zone, daylight)),
                )
                .map_err(|error| format!("无法更改时区：{error}"))?;
            }
            if clock_change {
                let time = if time_change { time } else { local().1 };
                let days = days_from_civil(calendar::Date {
                    year: i64::from(date.year),
                    month: date.month,
                    day: date.day,
                });
                let local = days * 86400
                    + i64::from(time[0]) * 3600
                    + i64::from(time[1]) * 60
                    + i64::from(time[2]);
                let unix = zone::from_local_checked(local).map_err(|error| error.to_string())?;
                zone::set_clock(unix).map_err(|error| format!("无法设置系统时间：{error}"))?;
            }
            Ok(())
        }))
    }
    fn committed(&mut self) -> liteos_ui::Result<()> {
        self.date_changed.set(false);
        self.time_changed.set(false);
        self.saved_zone = self.selected_zone()?;
        Ok(())
    }
    fn activity(&self, _background: Background, clock: Clock) -> Option<Activity> {
        let fields = self.time.clone();
        let calendar = self.calendar.clone();
        let preview = self.clock.clone();
        let date_changed = self.date_changed.clone();
        let time_changed = self.time_changed.clone();
        Some(Box::pin(async move {
            loop {
                let (date, time) = local();
                if !date_changed.get() {
                    let _ = calendar.set_date(date);
                }
                // 用户改过时间后，字段与时钟显示草稿，不再随系统时间走。
                if !time_changed.get() {
                    for (field, value) in fields.iter().zip(time) {
                        let _ = field.set_text(&format!("{value:02}"));
                    }
                    let _ = paint_clock(&preview, time);
                }
                let Ok(wait) = clock.sleep(Duration::from_secs(1)) else {
                    break;
                };
                wait.await;
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_rules_become_posix_tz_strings() {
        let pacific = ZONES
            .iter()
            .find(|zone| zone.abbreviation.0 == "PST")
            .unwrap();
        assert_eq!(posix_rule(pacific, true), "PST8PDT,M4.1.0/2,M10.5.0/2");
        assert_eq!(posix_rule(pacific, false), "PST8");
        let china = ZONES
            .iter()
            .find(|zone| zone.standard == "中国标准时间")
            .unwrap();
        assert_eq!(posix_rule(china, true), "CST-8");
        assert_eq!(posix_offset(-330), "-5:30");
    }
}
