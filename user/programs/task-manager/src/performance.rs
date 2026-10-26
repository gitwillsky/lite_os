//! 性能页只提供采样数值、仪表与历史曲线；分组框、行列、对齐和裁剪由共享控件实现。
//! 结构按 Windows 2000 任务管理器：上两行是“使用”仪表与“使用记录”曲线的分组框，
//! 下方四个分组以“名称 + 右对齐数值”列出统计。
use crate::{
    child,
    processes::grouped,
    system::{HISTORY, Monitor, Totals},
};
use liteos_ui::{Draw, Layout, Rect, Widget, WidgetKind as Kind, state};
use std::collections::VecDeque;

/// 仪表与曲线的配色：黑底、暗绿网格/未亮段、亮绿数据。
const BLACK: u32 = 0x000000;
const DIM: u32 = 0x008000;
const LIT: u32 = 0x00ff00;
/// 统计行高度（逻辑像素），与单行标签的文字行盒一致。
const STAT_ROW: i32 = 16;

pub struct View {
    cpu_meter: Widget,
    memory_meter: Widget,
    cpu_history: Widget,
    memory_history: Widget,
    /// 句柄数、线程数、进程数。
    totals: [Widget; 3],
    /// 总数、限制、峰值。
    commit: [Widget; 3],
    /// 总数、可用数、系统缓存。
    physical: [Widget; 3],
    /// 总数；内核只发布 Slab，没有分页/未分页拆分，不显示无数据的行。
    kernel: [Widget; 1],
    _containers: Vec<Widget>,
}

fn group(parent: &Widget, title: &str, layout: Layout) -> liteos_ui::Result<Widget> {
    child(parent, Kind::Group, title, layout)
}

fn canvas(parent: &Widget) -> liteos_ui::Result<Widget> {
    child(
        parent,
        Kind::Canvas,
        "",
        Layout {
            grow: 1,
            ..Default::default()
        },
    )
}

/// 一行“使用”仪表加“使用记录”曲线；返回（仪表, 曲线）。
fn metric(
    page: &Widget,
    title: &str,
    history: &str,
    containers: &mut Vec<Widget>,
) -> liteos_ui::Result<(Widget, Widget)> {
    let row = child(
        page,
        Kind::Row,
        "",
        Layout {
            grow: 1,
            gap: 8,
            ..Default::default()
        },
    )?;
    let boxed = Layout {
        padding: 6,
        ..Default::default()
    };
    let usage = group(&row, title, Layout { width: 76, ..boxed })?;
    let record = group(&row, history, Layout { grow: 1, ..boxed })?;
    let meter = canvas(&usage)?;
    let chart = canvas(&record)?;
    containers.extend([row, usage, record]);
    Ok((meter, chart))
}

/// 一个统计分组：每行名称左对齐、数值右对齐；返回各行的数值标签。
fn stats<const N: usize>(
    parent: &Widget,
    title: &str,
    names: [&str; N],
    containers: &mut Vec<Widget>,
) -> liteos_ui::Result<[Widget; N]> {
    let boxed = group(
        parent,
        title,
        Layout {
            padding: 6,
            ..Default::default()
        },
    )?;
    let mut values = Vec::with_capacity(N);
    for name in names {
        let row = child(
            &boxed,
            Kind::Row,
            "",
            Layout {
                height: STAT_ROW,
                ..Default::default()
            },
        )?;
        let cell = Layout {
            grow: 1,
            height: STAT_ROW,
            ..Default::default()
        };
        let label = child(&row, Kind::Label, name, cell)?;
        let value = child(&row, Kind::Label, "", cell)?;
        value.set_state(state::ALIGN_RIGHT)?;
        containers.extend([row, label]);
        values.push(value);
    }
    containers.push(boxed);
    Ok(values
        .try_into()
        .unwrap_or_else(|_| unreachable!("one value per name")))
}

impl View {
    pub fn new(page: &Widget) -> liteos_ui::Result<Self> {
        let mut containers = Vec::new();
        let (cpu_meter, cpu_history) = metric(page, "CPU 使用", "CPU 使用记录", &mut containers)?;
        let (memory_meter, memory_history) =
            metric(page, "内存使用", "内存使用记录", &mut containers)?;
        let row = child(
            page,
            Kind::Row,
            "",
            Layout {
                gap: 8,
                ..Default::default()
            },
        )?;
        let column = |containers: &mut Vec<Widget>| {
            let column = child(
                &row,
                Kind::Column,
                "",
                Layout {
                    grow: 1,
                    gap: 6,
                    ..Default::default()
                },
            )?;
            containers.push(column.clone());
            Ok::<_, liteos_ui::Error>(column)
        };
        let left = column(&mut containers)?;
        let right = column(&mut containers)?;
        let totals = stats(
            &left,
            "总数",
            ["句柄数", "线程数", "进程数"],
            &mut containers,
        )?;
        let physical = stats(
            &left,
            "物理内存 (K)",
            ["总数", "可用数", "系统缓存"],
            &mut containers,
        )?;
        let commit = stats(
            &right,
            "认可用量 (K)",
            ["总数", "限制", "峰值"],
            &mut containers,
        )?;
        let kernel = stats(&right, "内核内存 (K)", ["总数"], &mut containers)?;
        containers.push(row);
        Ok(Self {
            cpu_meter,
            memory_meter,
            cpu_history,
            memory_history,
            totals,
            commit,
            physical,
            kernel,
            _containers: containers,
        })
    }

    pub fn update(&self, monitor: &Monitor, totals: Totals) -> liteos_ui::Result<()> {
        let (used, limit) = monitor.memory.commit();
        let set = |widgets: &[Widget], values: &[u64]| {
            widgets
                .iter()
                .zip(values)
                .try_for_each(|(widget, value)| widget.set_text(&grouped(*value)))
        };
        set(
            &self.totals,
            &[totals.handles, totals.threads, totals.processes],
        )?;
        set(&self.commit, &[used, limit, monitor.commit_peak])?;
        set(
            &self.physical,
            &[
                monitor.memory.total,
                monitor.memory.available,
                monitor.memory.cached,
            ],
        )?;
        set(&self.kernel, &[monitor.memory.slab])?;
        self.charts(monitor)
    }

    /// 仪表与曲线依赖控件几何，采样或 configure 后都按同一份监视数据重画。
    pub fn charts(&self, monitor: &Monitor) -> liteos_ui::Result<()> {
        let (used, limit) = monitor.memory.commit();
        meter(
            &self.cpu_meter,
            monitor.cpu_percent,
            &format!("{} %", monitor.cpu_percent),
        )?;
        let percent = (used.saturating_mul(100) / limit.max(1)).min(100) as u8;
        meter(&self.memory_meter, percent, &format!("{used}K"))?;
        history(&self.cpu_history, &monitor.cpu_history)?;
        history(&self.memory_history, &monitor.memory_history)
    }
}

fn fill(rect: Rect, color: u32) -> Draw<'static> {
    Draw {
        kind: 1,
        rect,
        color,
        bold: false,
        text: "",
    }
}

/// Windows 2000 的 LED 柱状仪表：两列 2 像素高的发光段，自下而上按百分比点亮，底部居中显示读数。
fn meter(widget: &Widget, percent: u8, reading: &str) -> liteos_ui::Result<()> {
    const TEXT: i32 = 16;
    const SEGMENT: i32 = 3;
    let bounds = widget.geometry()?;
    if bounds.width <= 0 || bounds.height <= 0 {
        return Ok(());
    }
    let mut commands = vec![fill(Rect::new(0, 0, bounds.width, bounds.height), BLACK)];
    let bar_width = (bounds.width - 16).clamp(0, 33);
    let column = (bar_width - 1) / 2;
    let left = (bounds.width - bar_width) / 2;
    let bottom = bounds.height - TEXT - 2;
    let rows = ((bottom - 4) / SEGMENT).max(0);
    let lit = (i32::from(percent.min(100)) * rows + 50) / 100;
    for row in 0..rows {
        let y = bottom - (row + 1) * SEGMENT + 1;
        let color = if row < lit { LIT } else { DIM };
        commands.push(fill(Rect::new(left, y, column, SEGMENT - 1), color));
        commands.push(fill(
            Rect::new(left + column + 1, y, column, SEGMENT - 1),
            color,
        ));
    }
    commands.push(Draw {
        kind: 6,
        rect: Rect::new(0, bounds.height - TEXT - 1, bounds.width, TEXT),
        color: LIT,
        bold: false,
        text: reading,
    });
    widget.set_drawing(&commands)
}

fn history(widget: &Widget, samples: &VecDeque<u8>) -> liteos_ui::Result<()> {
    let bounds = widget.geometry()?;
    if bounds.width <= 0 || bounds.height <= 0 {
        return Ok(());
    }
    let mut commands = vec![fill(Rect::new(0, 0, bounds.width, bounds.height), BLACK)];
    for x in (0..bounds.width).step_by(12) {
        commands.push(fill(Rect::new(x, 0, 1, bounds.height), DIM));
    }
    for y in (0..bounds.height).step_by(12) {
        commands.push(fill(Rect::new(0, y, bounds.width, 1), DIM));
    }
    let visible = ((bounds.width / 3) as usize)
        .min(HISTORY)
        .min(samples.len());
    let mut previous = None;
    for (index, value) in samples.iter().skip(samples.len() - visible).enumerate() {
        let x = bounds.width - (visible - index) as i32 * 3;
        let y = bounds.height - 1 - i32::from(*value) * (bounds.height - 1) / 100;
        let (low, high) = previous.map_or((y, y), |before: i32| (before.min(y), before.max(y)));
        commands.push(fill(Rect::new(x, low, 3, high - low + 1), LIT));
        previous = Some(y);
    }
    widget.set_drawing(&commands)
}
