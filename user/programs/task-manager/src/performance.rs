//! 性能页只提供采样数值与历史曲线；文字、行列、裁剪由共享控件实现。
use crate::{
    child,
    processes::grouped,
    system::{HISTORY, Monitor, Totals},
};
use liteos_ui::{Draw, Layout, Rect, Widget, WidgetKind as Kind, state};
use std::collections::VecDeque;
pub struct View {
    cpu: Widget,
    memory: Widget,
    cpu_history: Widget,
    memory_history: Widget,
    totals: Widget,
    physical: Widget,
    commit: Widget,
    kernel: Widget,
    _labels: Vec<Widget>,
}
fn metric(
    page: &Widget,
    title: &str,
    history: &str,
) -> liteos_ui::Result<(Widget, Widget, Vec<Widget>)> {
    let row = child(
        page,
        Kind::Row,
        "",
        Layout {
            height: 100,
            gap: 8,
            ..Default::default()
        },
    )?;
    let numbers = child(
        &row,
        Kind::Column,
        "",
        Layout {
            width: 110,
            gap: 4,
            ..Default::default()
        },
    )?;
    let caption = child(&numbers, Kind::Label, title, Layout::default())?;
    let value = child(
        &numbers,
        Kind::Label,
        "",
        Layout {
            grow: 1,
            ..Default::default()
        },
    )?;
    let chart = child(
        &row,
        Kind::Column,
        "",
        Layout {
            grow: 1,
            gap: 4,
            ..Default::default()
        },
    )?;
    let chart_caption = child(&chart, Kind::Label, history, Layout::default())?;
    let canvas = child(
        &chart,
        Kind::Canvas,
        "",
        Layout {
            grow: 1,
            ..Default::default()
        },
    )?;
    Ok((value, canvas, vec![caption, chart_caption]))
}
impl View {
    pub fn new(page: &Widget) -> liteos_ui::Result<Self> {
        let (cpu, cpu_history, mut labels) = metric(page, "CPU 使用", "CPU 使用记录")?;
        let (memory, memory_history, more) = metric(page, "内存使用", "内存使用记录")?;
        labels.extend(more);
        let row = child(
            page,
            Kind::Row,
            "",
            Layout {
                grow: 1,
                gap: 12,
                ..Default::default()
            },
        )?;
        let left = child(
            &row,
            Kind::Column,
            "",
            Layout {
                grow: 1,
                gap: 8,
                ..Default::default()
            },
        )?;
        let right = child(
            &row,
            Kind::Column,
            "",
            Layout {
                grow: 1,
                gap: 8,
                ..Default::default()
            },
        )?;
        let totals = child(&left, Kind::Label, "", Layout::default())?;
        let physical = child(&left, Kind::Label, "", Layout::default())?;
        let commit = child(&right, Kind::Label, "", Layout::default())?;
        let kernel = child(&right, Kind::Label, "", Layout::default())?;
        for label in [&totals, &physical, &commit, &kernel] {
            label.set_state(state::WRAP)?;
        }
        Ok(Self {
            cpu,
            memory,
            cpu_history,
            memory_history,
            totals,
            physical,
            commit,
            kernel,
            _labels: labels,
        })
    }
    pub fn update(&self, monitor: &Monitor, totals: Totals) -> liteos_ui::Result<()> {
        let (used, limit) = monitor.memory.commit();
        self.cpu.set_text(&format!("{} %", monitor.cpu_percent))?;
        self.memory.set_text(&format!("{} K", grouped(used)))?;
        self.totals.set_text(&format!(
            "总数\n句柄数  {}\n线程数  {}\n进程数  {}",
            grouped(totals.handles),
            grouped(totals.threads),
            grouped(totals.processes)
        ))?;
        self.physical.set_text(&format!(
            "物理内存(K)\n总共  {}\n可用数  {}\n系统缓存  {}",
            grouped(monitor.memory.total),
            grouped(monitor.memory.available),
            grouped(monitor.memory.cached)
        ))?;
        self.commit.set_text(&format!(
            "认可用量(K)\n总数  {}\n限制  {}\n峰值  {}",
            grouped(used),
            grouped(limit),
            grouped(monitor.commit_peak)
        ))?;
        self.kernel.set_text(&format!(
            "内核内存(K)\n总数  {}",
            grouped(monitor.memory.slab)
        ))?;
        self.charts(monitor)
    }
    pub fn charts(&self, monitor: &Monitor) -> liteos_ui::Result<()> {
        history(&self.cpu_history, &monitor.cpu_history)?;
        history(&self.memory_history, &monitor.memory_history)
    }
}
fn history(widget: &Widget, samples: &VecDeque<u8>) -> liteos_ui::Result<()> {
    let bounds = widget.geometry()?;
    if bounds.width <= 0 || bounds.height <= 0 {
        return Ok(());
    }
    let mut commands = vec![Draw {
        kind: 1,
        rect: Rect::new(0, 0, bounds.width, bounds.height),
        color: 0,
        bold: false,
        text: "",
    }];
    let mut fill = |rect, color| {
        commands.push(Draw {
            kind: 1,
            rect,
            color,
            bold: false,
            text: "",
        })
    };
    for x in (0..bounds.width).step_by(12) {
        fill(Rect::new(x, 0, 1, bounds.height), 0x008000);
    }
    for y in (0..bounds.height).step_by(12) {
        fill(Rect::new(0, y, bounds.width, 1), 0x008000);
    }
    let visible = ((bounds.width / 3) as usize)
        .min(HISTORY)
        .min(samples.len());
    let mut previous = None;
    for (index, value) in samples.iter().skip(samples.len() - visible).enumerate() {
        let x = bounds.width - (visible - index) as i32 * 3;
        let y = bounds.height - 1 - i32::from(*value) * (bounds.height - 1) / 100;
        let (low, high) = previous.map_or((y, y), |before: i32| (before.min(y), before.max(y)));
        fill(Rect::new(x, low, 3, high - low + 1), 0x00ff00);
        previous = Some(y);
    }
    widget.set_drawing(&commands)
}
