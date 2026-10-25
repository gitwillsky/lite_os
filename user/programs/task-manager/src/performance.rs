//! "性能"标签页：CPU 与内存的数字表加历史曲线，以及各项总数（Windows 2000 的布局）。
use crate::{
    processes::grouped,
    system::{HISTORY, Monitor, Totals},
};
use canvas::Rect;
use classic_theme::{Edge, Rgb, SystemColor as C};
use classic_ui::{Ink, Painter};
use std::collections::VecDeque;

const GREEN: Rgb = Rgb {
    red: 0,
    green: 255,
    blue: 0,
};
const GRID: Rgb = Rgb {
    red: 0,
    green: 128,
    blue: 0,
};
const BLACK: Rgb = Rgb {
    red: 0,
    green: 0,
    blue: 0,
};
const GRAPH_HEIGHT: i32 = 84;
/// 网格线间距与曲线每个采样的水平步长。
const GRID_STEP: i32 = 12;
const SAMPLE_STEP: i32 = 3;

/// 一组"标题加若干（名称，值）行"：左上角位置、组标题、各行。
type Block<'a> = (i32, i32, &'a str, Vec<(&'a str, String)>);

/// 画整个性能页；`page` 是标签页内部矩形。
pub fn paint(p: &mut Painter<'_>, page: Rect, monitor: &Monitor, totals: Totals) {
    let meter_width = 76;
    let history_x = page.x + 8 + meter_width + 8;
    let history_width = page.right() - 8 - history_x;
    let (used, _) = monitor.memory.commit();
    let rows = [
        (
            "CPU 使用",
            "CPU 使用记录",
            format!("{} %", monitor.cpu_percent),
            u32::from(monitor.cpu_percent),
            &monitor.cpu_history,
        ),
        (
            "内存使用",
            "内存使用记录",
            format!("{}K", grouped(used)),
            monitor.memory_history.back().map_or(0, |v| u32::from(*v)),
            &monitor.memory_history,
        ),
    ];
    for (index, (meter_title, history_title, text, percent, history)) in rows.iter().enumerate() {
        let y = page.y + 8 + index as i32 * (GRAPH_HEIGHT + 6);
        let meter = Rect::new(page.x + 8, y, meter_width, GRAPH_HEIGHT);
        p.group_box(meter, meter_title);
        paint_meter(p, meter, text, *percent);
        let graph = Rect::new(history_x, y, history_width, GRAPH_HEIGHT);
        p.group_box(graph, history_title);
        paint_history(p, graph, history);
    }

    let top = page.y + 8 + 2 * (GRAPH_HEIGHT + 6);
    let width = (page.width - 24) / 2;
    let (left, right) = (page.x + 8, page.x + 16 + width);
    let step = p.line_height() + 3;
    let height = |lines: i32| 18 + lines * step;
    let physical = &monitor.memory;
    let (commit_used, commit_limit) = monitor.memory.commit();
    let blocks: [Block; 4] = [
        (
            left,
            top,
            "总数",
            vec![
                ("句柄数", grouped(totals.handles)),
                ("线程数", grouped(totals.threads)),
                ("进程数", grouped(totals.processes)),
            ],
        ),
        (
            left,
            top + height(3) + 6,
            "物理内存(K)",
            vec![
                ("总共", grouped(physical.total)),
                ("可用数", grouped(physical.available)),
                ("系统缓存", grouped(physical.cached)),
            ],
        ),
        (
            right,
            top,
            "认可用量(K)",
            vec![
                ("总数", grouped(commit_used)),
                ("限制", grouped(commit_limit)),
                ("峰值", grouped(monitor.commit_peak)),
            ],
        ),
        (
            right,
            top + height(3) + 6,
            "内核内存(K)",
            vec![("总数", grouped(physical.slab))],
        ),
    ];
    for (x, y, title, lines) in blocks {
        let group = Rect::new(x, y, width, height(lines.len() as i32));
        p.group_box(group, title);
        for (row, (label, value)) in lines.iter().enumerate() {
            let line_y = group.y + 18 + row as i32 * step - 1;
            p.text(
                group.x + 10,
                line_y,
                label,
                Ink::Color(C::ButtonText),
                false,
            );
            let value_width = (p.text_width(value, false) + p.scale() - 1) / p.scale();
            p.text(
                group.right() - 10 - value_width,
                line_y,
                value,
                Ink::Color(C::ButtonText),
                false,
            );
        }
    }
}

/// 组框内的黑底凹陷面板；返回其内部矩形。
fn panel(p: &mut Painter<'_>, group: Rect) -> Rect {
    let outer = Rect::new(
        group.x + 8,
        group.y + 18,
        group.width - 16,
        group.height - 26,
    );
    let inner = p.edge(outer, Edge::Sunken);
    p.fill_rgb(inner, BLACK);
    inner
}

/// 竖向分段数字表：下方是 `NN %` 数字，上方是自下而上点亮的绿色格子。
fn paint_meter(p: &mut Painter<'_>, group: Rect, text: &str, percent: u32) {
    let inner = panel(p, group);
    let digits = p.line_height() + 4;
    let bar = Rect::new(
        inner.x + 8,
        inner.y + 4,
        inner.width - 16,
        inner.height - digits - 6,
    );
    // 每格 3 像素高、间隔 1 像素；点亮的格数与百分比成正比。
    let cells = bar.height / 4;
    let lit = (cells * percent.min(100) as i32 + 50) / 100;
    for cell in 0..cells {
        let y = bar.bottom() - (cell + 1) * 4;
        let rgb = if cell < lit { GREEN } else { GRID };
        p.fill_rgb(Rect::new(bar.x, y, bar.width, 3), rgb);
    }
    let width = (p.text_width(text, false) + p.scale() - 1) / p.scale();
    p.text_rgb(
        inner.x + (inner.width - width) / 2,
        inner.bottom() - digits + 2,
        text,
        GREEN,
    );
}

/// 历史曲线：暗绿网格，绿色折线从右向左滚动。
fn paint_history(p: &mut Painter<'_>, group: Rect, history: &VecDeque<u8>) {
    let inner = panel(p, group);
    let mut y = inner.bottom() - GRID_STEP;
    while y > inner.y {
        p.fill_rgb(Rect::new(inner.x, y, inner.width, 1), GRID);
        y -= GRID_STEP;
    }
    let mut x = inner.x + GRID_STEP;
    while x < inner.right() {
        p.fill_rgb(Rect::new(x, inner.y, 1, inner.height), GRID);
        x += GRID_STEP;
    }
    let level = |value: u8| inner.bottom() - 1 - i32::from(value) * (inner.height - 1) / 100;
    let visible = ((inner.width / SAMPLE_STEP) as usize)
        .min(HISTORY)
        .min(history.len());
    let mut previous: Option<i32> = None;
    for (index, value) in history.iter().skip(history.len() - visible).enumerate() {
        let x = inner.right() - (visible - index) as i32 * SAMPLE_STEP;
        let y = level(*value);
        let (low, high) = previous.map_or((y, y), |before| (y.min(before), y.max(before)));
        p.fill_rgb(Rect::new(x, low, SAMPLE_STEP, high - low + 1), GREEN);
        previous = Some(y);
    }
}
