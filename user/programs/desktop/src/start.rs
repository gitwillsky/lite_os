//! 开始菜单：Windows 2000 经典布局的条目、几何与绘制；点击与键盘由 `main` 驱动。
use crate::paint::Art;
use canvas::Rect;
use classic_theme::{Edge, SystemColor as C};
use classic_ui::{Ink, Painter};
use desktop_entry::Entry as Application;

/// 左侧竖幅宽度；条目从它右边开始。
const BANNER: i32 = 24;
const MENU_WIDTH: i32 = 190;
const ROW_HEIGHT: i32 = 36;
const SEPARATOR: i32 = 10;
const FLYOUT_WIDTH: i32 = 200;
const FLYOUT_ROW: i32 = 22;
const FRAME: i32 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place {
    Documents,
    MyComputer,
    RecycleBin,
    ControlPanel,
}

/// 点击一个叶子条目的结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Application(usize),
    Files(Place),
    Tasks,
    Taskbar,
    About,
    Run,
    Lock,
    Logoff,
    Shutdown,
}

pub struct Row {
    pub icon: &'static str,
    pub label: String,
    /// 叶子条目的动作；带子菜单的条目为 `None`。
    pub action: Option<Action>,
    pub submenu: Vec<Row>,
    /// 此条目上方带分隔线。
    pub separator_before: bool,
}

fn leaf(icon: &'static str, label: &str, action: Action) -> Row {
    Row {
        icon,
        label: label.into(),
        action: Some(action),
        submenu: Vec::new(),
        separator_before: false,
    }
}

fn parent(icon: &'static str, label: &str, submenu: Vec<Row>) -> Row {
    Row {
        icon,
        label: label.into(),
        action: None,
        submenu,
        separator_before: false,
    }
}

/// 菜单内容：程序、文档、设置是子菜单；运行与会话操作是叶子。
pub fn rows(apps: &[Application]) -> Vec<Row> {
    let mut programs: Vec<Row> = apps
        .iter()
        .enumerate()
        .map(|(index, app)| leaf("file-app-16", &app.name, Action::Application(index)))
        .collect();
    let mut tasks = leaf("task-manager-16", "任务管理器", Action::Tasks);
    tasks.separator_before = !programs.is_empty();
    programs.push(tasks);
    programs.push(leaf("help-16", "关于 LiteOS", Action::About));
    let mut lock = leaf("lock-32", "锁定计算机", Action::Lock);
    lock.separator_before = true;
    vec![
        parent("programs-32", "程序(P)", programs),
        parent(
            "documents-32",
            "文档(D)",
            vec![leaf(
                "my-documents-16",
                "我的文档",
                Action::Files(Place::Documents),
            )],
        ),
        parent(
            "settings-32",
            "设置(S)",
            vec![
                leaf(
                    "control-panel-16",
                    "控制面板(C)",
                    Action::Files(Place::ControlPanel),
                ),
                leaf("settings-16", "任务栏和「开始」菜单(T)...", Action::Taskbar),
            ],
        ),
        leaf("run-32", "运行(R)...", Action::Run),
        lock,
        leaf("logoff-32", "注销(L)...", Action::Logoff),
        leaf("shutdown-32", "关机(U)...", Action::Shutdown),
    ]
}

/// 菜单当前的悬停/键盘选择。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Selection {
    pub top: usize,
    /// 子菜单内被选中的条目；`None` 表示选择停在顶层。
    pub sub: Option<usize>,
}

/// 一次布局：顶层面板、各顶层条目、展开的子菜单面板与其条目。
pub struct Geometry {
    /// 展开子菜单的顶层序号。
    pub open: Option<usize>,
    pub panel: Rect,
    pub tops: Vec<Rect>,
    pub flyout: Option<(Rect, Vec<Rect>)>,
}

/// 屏幕 `screen` 逻辑像素，菜单贴在左下、任务栏之上；`open` 是展开子菜单的顶层序号。
pub fn geometry(rows: &[Row], screen: (i32, i32), taskbar: i32, open: Option<usize>) -> Geometry {
    let height: i32 = rows
        .iter()
        .map(|r| ROW_HEIGHT + if r.separator_before { SEPARATOR } else { 0 })
        .sum::<i32>()
        + 2 * FRAME;
    let panel = Rect::new(0, screen.1 - taskbar - height, MENU_WIDTH, height);
    let mut y = panel.y + FRAME;
    let tops: Vec<Rect> = rows
        .iter()
        .map(|row| {
            if row.separator_before {
                y += SEPARATOR;
            }
            let rect = Rect::new(panel.x + BANNER, y, MENU_WIDTH - BANNER - FRAME, ROW_HEIGHT);
            y += ROW_HEIGHT;
            rect
        })
        .collect();
    let flyout = open.filter(|i| !rows[*i].submenu.is_empty()).map(|i| {
        let sub = &rows[i].submenu;
        let sub_height: i32 = sub
            .iter()
            .map(|r| FLYOUT_ROW + if r.separator_before { SEPARATOR } else { 0 })
            .sum::<i32>()
            + 4;
        // 子菜单顶端与父条目对齐；放不下时上移，使其底边留在任务栏上方。
        let top = tops[i].y.min(screen.1 - taskbar - sub_height).max(0);
        let rect = Rect::new(panel.right() - 2, top, FLYOUT_WIDTH, sub_height);
        let mut y = rect.y + 2;
        let items = sub
            .iter()
            .map(|r| {
                if r.separator_before {
                    y += SEPARATOR;
                }
                let item = Rect::new(rect.x + 2, y, FLYOUT_WIDTH - 4, FLYOUT_ROW);
                y += FLYOUT_ROW;
                item
            })
            .collect();
        (rect, items)
    });
    Geometry {
        open,
        panel,
        tops,
        flyout,
    }
}

/// 指针位置对应的选择；指针不在任何条目上返回 `None`。
pub fn hit(g: &Geometry, point: (i32, i32)) -> Option<Selection> {
    let inside =
        |r: &Rect| (r.x..r.right()).contains(&point.0) && (r.y..r.bottom()).contains(&point.1);
    if let Some((_, items)) = &g.flyout
        && let Some(sub) = items.iter().position(inside)
    {
        return g.open.map(|top| Selection {
            top,
            sub: Some(sub),
        });
    }
    g.tops
        .iter()
        .position(inside)
        .map(|top| Selection { top, sub: None })
}

/// 绘制菜单；`selection` 的 `top` 决定展开哪个子菜单（由调用者传给 [`geometry`]）。
pub fn paint(p: &mut Painter<'_>, art: &Art, rows: &[Row], g: &Geometry, selection: Selection) {
    let inner = p.menu_popup(g.panel);
    if let Some(banner) = art.bitmap("start-banner-1x") {
        // 竖幅 233 高、靠底对齐；菜单更高时上方用幅顶颜色补齐（见 assets.md）。
        let top = inner.bottom() - 233;
        banner.fill_with_corner(p, inner.x + 1, inner.y, (top - inner.y).max(0));
        banner.paint(p, inner.x + 1, top);
    }
    for (index, (row, rect)) in rows.iter().zip(&g.tops).enumerate() {
        if row.separator_before {
            p.edge(
                Rect::new(rect.x + 2, rect.y - SEPARATOR / 2 - 1, rect.width - 4, 2),
                Edge::Etched,
            );
        }
        let highlighted = selection.top == index;
        paint_row(p, art, row, *rect, highlighted, 4, 32);
    }
    if let Some((panel, items)) = &g.flyout {
        p.menu_popup(*panel);
        for (index, (row, rect)) in rows[selection.top].submenu.iter().zip(items).enumerate() {
            if row.separator_before {
                p.edge(
                    Rect::new(rect.x + 2, rect.y - SEPARATOR / 2 - 1, rect.width - 4, 2),
                    Edge::Etched,
                );
            }
            paint_row(p, art, row, *rect, selection.sub == Some(index), 3, 16);
        }
    }
}

fn paint_row(
    p: &mut Painter<'_>,
    art: &Art,
    row: &Row,
    rect: Rect,
    highlighted: bool,
    pad: i32,
    icon: i32,
) {
    let (background, text) = if highlighted {
        (C::Hilight, C::HilightText)
    } else {
        (C::Menu, C::MenuText)
    };
    p.fill(rect, background);
    art.icon(p, row.icon, rect.x + pad, rect.y + (rect.height - icon) / 2);
    let label = row.label.as_str();
    p.text(
        rect.x + pad + icon + 8,
        rect.y + (rect.height - p.line_height()) / 2,
        label,
        Ink::Color(text),
        false,
    );
    if !row.submenu.is_empty() {
        let x = rect.right() - 12;
        let y = rect.y + rect.height / 2 - 4;
        for step in 0..4 {
            p.fill(Rect::new(x + step, y + step, 1, 8 - 2 * step), text);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn menu() -> Vec<Row> {
        rows(&[])
    }

    #[test]
    fn programs_documents_and_settings_open_submenus_and_the_rest_act() {
        let rows = menu();
        let kinds: Vec<_> = rows.iter().map(|r| r.action.is_some()).collect();
        assert_eq!(kinds, [false, false, false, true, true, true, true]);
        assert!(
            rows.iter()
                .all(|r| r.action.is_some() != !r.submenu.is_empty())
        );
    }

    #[test]
    fn rows_stack_upwards_from_the_taskbar_with_a_gap_per_separator() {
        let rows = menu();
        let g = geometry(&rows, (800, 600), 28, None);
        assert_eq!(g.panel.bottom(), 600 - 28);
        assert!(g.tops.windows(2).all(|w| w[0].bottom() <= w[1].y));
        assert_eq!(g.tops[5].y - g.tops[4].bottom(), 0);
        assert_eq!(g.tops[4].y - g.tops[3].bottom(), SEPARATOR);
        assert!(g.flyout.is_none());
    }

    #[test]
    fn the_flyout_aligns_with_its_parent_and_stays_above_the_taskbar() {
        let rows = menu();
        let g = geometry(&rows, (800, 600), 28, Some(0));
        let (panel, items) = g.flyout.as_ref().unwrap();
        assert_eq!(items.len(), rows[0].submenu.len());
        assert!(panel.bottom() <= 600 - 28);
        assert_eq!(panel.x, g.panel.right() - 2);
        let settings = geometry(&rows, (800, 600), 28, Some(2));
        assert_eq!(settings.flyout.unwrap().0.y, settings.tops[2].y);
    }

    #[test]
    fn hit_testing_prefers_the_flyout_and_reports_the_parent_row() {
        let rows = menu();
        let g = geometry(&rows, (800, 600), 28, Some(2));
        let (_, items) = g.flyout.as_ref().unwrap();
        let point = (items[0].x + 5, items[0].y + 5);
        assert_eq!(
            hit(&g, point),
            Some(Selection {
                top: 2,
                sub: Some(0)
            })
        );
        let top = (g.tops[3].x + 5, g.tops[3].y + 5);
        assert_eq!(hit(&g, top), Some(Selection { top: 3, sub: None }));
        assert_eq!(hit(&g, (700, 5)), None);
    }
}
