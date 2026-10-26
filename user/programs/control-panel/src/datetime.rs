//! 日期、时间与时区草稿采用共享控件；标准 TZif/系统时钟写入在属性表后台应用阶段完成。
use crate::{Activity, Apply, Page, child, group, label};
use calendar::{civil_from_days, days_from_civil, zone};
use liteos_ui::{
    Background, Clock, Date, Draw, Event, EventKind, Layout, PropertySheet, Rect, ValidationError,
    Widget, WidgetKind as Kind,
};
use std::{
    cell::Cell,
    rc::Rc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
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
pub struct DateTime {
    _controls: Vec<Widget>,
    page: Widget,
    calendar: Widget,
    time: [Widget; 3],
    steps: Vec<(Widget, usize, i32)>,
    zones: Widget,
    saved_zone: Option<u64>,
    date_changed: Rc<Cell<bool>>,
    time_changed: Rc<Cell<bool>>,
    clock: Widget,
}
impl DateTime {
    pub fn build(sheet: &PropertySheet) -> liteos_ui::Result<Self> {
        let page = sheet.page("日期和时间")?;
        let row = child(
            &page,
            Kind::Row,
            "",
            Layout {
                grow: 1,
                gap: 12,
                ..Default::default()
            },
        )?;
        let calendar = child(
            &row,
            Kind::Calendar,
            "",
            Layout {
                grow: 1,
                min_width: 210,
                ..Default::default()
            },
        )?;
        let time_group = group(&row, "时间")?;
        let clock = child(
            &time_group,
            Kind::Canvas,
            "",
            Layout {
                height: 140,
                ..Default::default()
            },
        )?;
        let (date, time) = local();
        calendar.set_date(date)?;
        let mut controls = vec![row, time_group.clone()];
        let mut fields = Vec::new();
        let mut steps = Vec::new();
        for (index, title) in ["时：", "分：", "秒："].into_iter().enumerate() {
            let row = child(
                &time_group,
                Kind::Row,
                "",
                Layout {
                    height: 26,
                    gap: 4,
                    ..Default::default()
                },
            )?;
            controls.push(child(
                &row,
                Kind::Label,
                title,
                Layout {
                    width: 34,
                    ..Default::default()
                },
            )?);
            fields.push(child(
                &row,
                Kind::Edit,
                &format!("{:02}", time[index]),
                Layout {
                    width: 42,
                    ..Default::default()
                },
            )?);
            for (title, delta) in [("−", -1), ("+", 1)] {
                steps.push((
                    child(
                        &row,
                        Kind::Button,
                        title,
                        Layout {
                            width: 28,
                            ..Default::default()
                        },
                    )?,
                    index,
                    delta,
                ));
            }
            controls.push(row);
        }
        let zones_page = sheet.page("时区")?;
        controls.push(label(
            &zones_page,
            "选择所在时区。以下时区使用固定 UTC 偏移，不自动调整夏令时。",
        )?);
        let zones = child(
            &zones_page,
            Kind::List,
            "",
            Layout {
                grow: 1,
                ..Default::default()
            },
        )?;
        zones.set_rows(
            &ZONES
                .iter()
                .enumerate()
                .map(|(index, (_, _, title))| (index as u64 + 1, *title))
                .collect::<Vec<_>>(),
        )?;
        let saved_zone = ZONES
            .iter()
            .position(|(minutes, _, _)| i64::from(*minutes) * 60 == zone::local_offset(now()))
            .map(|index| index as u64 + 1);
        zones.select(saved_zone)?;
        if saved_zone.is_none() {
            controls.push(label(&zones_page, "当前时区不在列表中。")?);
        }
        controls.push(zones_page);
        Ok(Self {
            _controls: controls,
            page,
            calendar,
            time: fields.try_into().ok().expect("three clock fields"),
            steps,
            zones,
            saved_zone,
            date_changed: Rc::new(Cell::new(false)),
            time_changed: Rc::new(Cell::new(false)),
            clock,
        })
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
fn line(commands: &mut Vec<Draw<'static>>, from: (i32, i32), to: (i32, i32), color: u32) {
    let (mut x, mut y) = from;
    let dx = (to.0 - x).abs();
    let sx = if x < to.0 { 1 } else { -1 };
    let dy = -(to.1 - y).abs();
    let sy = if y < to.1 { 1 } else { -1 };
    let mut error = dx + dy;
    loop {
        commands.push(Draw {
            kind: 1,
            rect: Rect::new(x, y, 1, 1),
            color,
            bold: false,
            text: "",
        });
        if (x, y) == to {
            break;
        }
        let twice = 2 * error;
        if twice >= dy {
            error += dy;
            x += sx;
        }
        if twice <= dx {
            error += dx;
            y += sy;
        }
    }
}
fn paint_clock(clock: &Widget, time: [u32; 3]) -> liteos_ui::Result<()> {
    let area = clock.geometry()?;
    if area.width <= 0 || area.height <= 0 {
        return Ok(());
    }
    let radius = ((area.width.min(area.height) - 10) / 2).max(1);
    let center = (area.width / 2, area.height / 2);
    let mut commands = vec![Draw {
        kind: 1,
        rect: Rect::new(0, 0, area.width, area.height),
        color: clock.color(liteos_ui::ColorRole::Control)?,
        bold: false,
        text: "",
    }];
    for y in -radius..=radius {
        let x = ((radius * radius - y * y) as f64).sqrt() as i32;
        commands.push(Draw {
            kind: 1,
            rect: Rect::new(center.0 - x, center.1 + y, 2 * x + 1, 1),
            color: 0xffffff,
            bold: false,
            text: "",
        });
    }
    let point = |step: f64, length: i32| {
        let angle = step * std::f64::consts::TAU / 60.0 - std::f64::consts::FRAC_PI_2;
        (
            center.0 + (angle.cos() * f64::from(length)) as i32,
            center.1 + (angle.sin() * f64::from(length)) as i32,
        )
    };
    for tick in 0..60 {
        line(
            &mut commands,
            point(f64::from(tick), radius - 3),
            point(f64::from(tick), radius - if tick % 5 == 0 { 9 } else { 5 }),
            0,
        );
    }
    line(
        &mut commands,
        center,
        point(
            f64::from(time[0] % 12) * 5.0 + f64::from(time[1]) / 12.0,
            radius * 5 / 10,
        ),
        0,
    );
    line(
        &mut commands,
        center,
        point(f64::from(time[1]), radius * 8 / 10),
        0,
    );
    line(
        &mut commands,
        center,
        point(f64::from(time[2]), radius * 8 / 10),
        0x808080,
    );
    clock.set_drawing(&commands)
}
impl Page for DateTime {
    fn event(&mut self, event: &Event) -> liteos_ui::Result<Option<Vec<String>>> {
        if matches!(event.kind, EventKind::Changed { .. }) {
            if event.node == self.calendar.id() {
                self.date_changed.set(true);
            }
            if self.time.iter().any(|field| field.id() == event.node) {
                self.time_changed.set(true);
            }
        }
        if matches!(event.kind, EventKind::Activate { .. })
            && let Some((_, index, delta)) = self
                .steps
                .iter()
                .find(|(button, _, _)| button.id() == event.node)
        {
            let maximum = if *index == 0 { 24 } else { 60 };
            let value = self.time[*index].text()?.parse::<i32>().unwrap_or(0);
            self.time[*index].set_text(&format!("{:02}", (value + delta).rem_euclid(maximum)))?;
            self.time_changed.set(true);
        }
        if let Ok(time) = self.times() {
            paint_clock(&self.clock, time)?;
        }
        Ok(None)
    }
    fn dirty(&self) -> liteos_ui::Result<bool> {
        Ok(self.date_changed.get()
            || self.time_changed.get()
            || self.zones.selection()? != self.saved_zone)
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
        let selected = self.zones.selection()?;
        let zone_change = (selected != self.saved_zone)
            .then_some(selected)
            .flatten()
            .and_then(|id| ZONES.get(id.saturating_sub(1) as usize))
            .copied();
        Ok(Box::new(move || {
            if let Some((minutes, abbreviation, _)) = zone_change {
                zone::write_fixed(minutes * 60, abbreviation)
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
        self.saved_zone = self.zones.selection()?;
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
