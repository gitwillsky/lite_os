//! Web 视图左侧面板的内容：按 Windows 2000 SP4 简中 `webvw.dll` 模板（folder/controlp/recycle/nethood/dialup.htt）
//! 与字符串表 5001–5061 生成段落、链接、按钮与磁盘饼图；不持有控件，由 [`crate::view`] 声明成标准控件。
//!
//! 模板中指向 microsoft.com 的链接（Windows Update、支持）没有浏览器可打开，不生成。
use crate::{
    chrome::Action,
    model::{Details, Item, Location, byte_size_text, date_text},
};

/// 面板中的一段内容；`margin` 是 CSS 的上边距（`p` 12、`p.Half` 4、`<br>` 0）。
pub enum Part {
    Text {
        margin: i32,
        text: String,
    },
    /// 选中项名称等 `<b>` 文字。
    Bold {
        margin: i32,
        text: String,
    },
    Link {
        margin: i32,
        text: &'static str,
        target: Location,
    },
    Button {
        margin: i32,
        text: &'static str,
        action: Action,
    },
    /// 磁盘容量：图例与三维饼图。
    Drive {
        total: u64,
        free: u64,
    },
}

const PROMPT: &str = "选定项目可以查看其说明。";
const MULTIPLE: &str = " 个选定的项目。";

/// 面板标题：当前位置的显示名（模板的 `%THISDIRNAME%`）。
pub fn title(location: &Location) -> String {
    location.title()
}

/// “另请参阅:”与其后的链接（WVCoord 的 Links 区）。
fn see_also(parts: &mut Vec<Part>, links: &[(&'static str, Location)]) {
    parts.push(Part::Text {
        margin: 12,
        text: "另请参阅:".into(),
    });
    for (text, target) in links {
        parts.push(Part::Link {
            margin: 0,
            text,
            target: target.clone(),
        });
    }
}

/// 单个选中项的说明：名称粗体，其后按位置给出类型、日期、大小或备注。
fn describe(parts: &mut Vec<Part>, item: &Item) {
    parts.push(Part::Bold {
        margin: 12,
        text: item.name.clone(),
    });
    match &item.details {
        Details::Comment(comment) => parts.push(Part::Text {
            margin: 12,
            text: comment.clone(),
        }),
        Details::Drive(capacity) => {
            parts.push(Part::Text {
                margin: 0,
                text: item.kind.clone(),
            });
            if let Some((total, free)) = capacity {
                parts.push(Part::Drive {
                    total: *total,
                    free: *free,
                });
            }
        }
        Details::Trashed { original, deleted } => {
            parts.push(Part::Text {
                margin: 0,
                text: item.kind.clone(),
            });
            parts.push(Part::Text {
                margin: 12,
                text: format!("原位置: {}", original.display()),
            });
            if let Some(deleted) = deleted {
                parts.push(Part::Text {
                    margin: 0,
                    text: format!("删除日期: {}", date_text(*deleted)),
                });
            }
            if let Some(size) = item.size {
                parts.push(Part::Text {
                    margin: 0,
                    text: format!("大小: {}", byte_size_text(size)),
                });
            }
        }
        Details::Connection { status, device, .. } => {
            parts.push(Part::Text {
                margin: 0,
                text: item.kind.clone(),
            });
            parts.push(Part::Text {
                margin: 12,
                text: status.clone(),
            });
            parts.push(Part::Text {
                margin: 0,
                text: device.clone(),
            });
        }
        Details::None => {
            if !item.kind.is_empty() {
                parts.push(Part::Text {
                    margin: 0,
                    text: item.kind.clone(),
                });
            }
            if let Some(modified) = item.modified {
                parts.push(Part::Text {
                    margin: 12,
                    text: format!("修改时间: {}", date_text(modified)),
                });
            }
            if let Some(size) = item.size {
                parts.push(Part::Text {
                    margin: 0,
                    text: format!("大小: {}", byte_size_text(size)),
                });
            }
            if item.target.is_none() && item.path.is_some() {
                parts.push(Part::Text {
                    margin: 12,
                    text: "属性: (正常)".into(),
                });
            }
        }
    }
}

/// 多选：“N 个选定的项目。”，有文件大小时附“总计文件大小:”（5002、5004）。
fn multiple(parts: &mut Vec<Part>, selected: &[&Item], size_label: &str) {
    parts.push(Part::Text {
        margin: 12,
        text: format!("{}{MULTIPLE}", selected.len()),
    });
    let sizes: Vec<u64> = selected.iter().filter_map(|item| item.size).collect();
    if !sizes.is_empty() {
        parts.push(Part::Text {
            margin: 12,
            text: format!("{size_label}{}", byte_size_text(sizes.iter().sum())),
        });
    }
}

/// 面板正文。`items` 是当前位置的全部条目，`selected` 是选中的条目。
pub fn parts(
    location: &Location,
    home: &std::path::Path,
    items: &[&Item],
    selected: &[&Item],
) -> Vec<Part> {
    let mut parts = Vec::new();
    let documents = Location::Directory(home.to_owned());
    match location {
        Location::ControlPanel => {
            // controlp.htt
            parts.push(Part::Text {
                margin: 0,
                text: "使用“控制面板”中的设置来个人化您的计算机。".into(),
            });
            match selected {
                [] => parts.push(Part::Text {
                    margin: 12,
                    text: PROMPT.into(),
                }),
                [item] => describe(&mut parts, item),
                _ => multiple(&mut parts, selected, "总计文件大小: "),
            }
        }
        location if location.is_recycle_bin() => {
            // recycle.htt
            parts.push(Part::Text {
                margin: 0,
                text: "该文件夹包含已删除的文件和文件夹。".into(),
            });
            match selected {
                [] if items.is_empty() => parts.push(Part::Text {
                    margin: 12,
                    text: "回收站中没有项目。".into(),
                }),
                [] => {
                    parts.push(Part::Text {
                        margin: 12,
                        text: "要永久删除项目释放磁盘空间，请单击:".into(),
                    });
                    parts.push(Part::Button {
                        margin: 4,
                        text: "清空回收站",
                        action: Action::EmptyTrash,
                    });
                    parts.push(Part::Text {
                        margin: 12,
                        text: "要将所有项恢复到原位置，请单击:".into(),
                    });
                    parts.push(Part::Button {
                        margin: 4,
                        text: "全部恢复",
                        action: Action::RestoreAll,
                    });
                }
                [item] => describe(&mut parts, item),
                _ => multiple(&mut parts, selected, "总计大小: "),
            }
            if !selected.is_empty() {
                parts.push(Part::Text {
                    margin: 12,
                    text: "要将该项目移至其原来的位置，请单击: ".into(),
                });
                parts.push(Part::Button {
                    margin: 4,
                    text: "还    原",
                    action: Action::Restore,
                });
            }
        }
        Location::NetworkPlaces => {
            // nethood.htt
            parts.push(Part::Text {
                margin: 0,
                text: "用该文件夹打开其他计算机上的文件和文件夹，并安装网络打印机。".into(),
            });
            parts.push(Part::Text {
                margin: 12,
                text: "要在您的计算机上设置网络，请单击".into(),
            });
            parts.push(Part::Link {
                margin: 0,
                text: "网络和拨号连接",
                target: Location::NetworkConnections,
            });
            match selected {
                [] => parts.push(Part::Text {
                    margin: 12,
                    text: PROMPT.into(),
                }),
                [item] => describe(&mut parts, item),
                _ => multiple(&mut parts, selected, "总计文件大小: "),
            }
            see_also(
                &mut parts,
                &[("我的文档", documents), ("我的电脑", Location::MyComputer)],
            );
        }
        Location::NetworkConnections => {
            // dialup.htt；“新建连接”向导、网络标识与添加网络组件没有对应程序，不生成。
            parts.push(Part::Text {
                margin: 0,
                text: "该文件夹包含本机的网络连接信息。".into(),
            });
            parts.push(Part::Text {
                margin: 12,
                text: "要打开某连接，请单击其图标。".into(),
            });
            match selected {
                [] => parts.push(Part::Text {
                    margin: 12,
                    text: PROMPT.into(),
                }),
                [item] => describe(&mut parts, item),
                _ => multiple(&mut parts, selected, "总计文件大小: "),
            }
        }
        _ => {
            // folder.htt（WVCoord 原生填充）：我的电脑、整个网络与普通目录。
            match selected {
                [] => parts.push(Part::Text {
                    margin: 0,
                    text: PROMPT.into(),
                }),
                [item] => {
                    describe(&mut parts, item);
                    if let Some(Part::Bold { margin, .. }) = parts.first_mut() {
                        *margin = 0;
                    }
                }
                _ => multiple(&mut parts, selected, "总计文件大小: "),
            }
            if let Some(Part::Text { margin, .. }) = parts.first_mut() {
                *margin = 0;
            }
            if *location == Location::MyComputer {
                see_also(
                    &mut parts,
                    &[
                        ("我的文档", documents),
                        ("网上邻居", Location::NetworkPlaces),
                        ("网络和拨号连接", Location::NetworkConnections),
                    ],
                );
            } else if matches!(location, Location::Directory(_)) {
                see_also(
                    &mut parts,
                    &[
                        ("我的文档", documents),
                        ("网上邻居", Location::NetworkPlaces),
                        ("我的电脑", Location::MyComputer),
                    ],
                );
            }
        }
    }
    parts
}

/// 磁盘饼图的颜色（webvw 磁盘信息与 shell32 驱动器属性相同）：已用为蓝、可用为品红，侧面取半亮度。
pub const USED: u32 = 0x0000FF;
pub const FREE: u32 = 0xFF00FF;
const USED_SIDE: u32 = 0x000080;
const FREE_SIDE: u32 = 0x800080;

/// 饼图尺寸：椭圆宽 120、高 48，厚度 10。
pub const PIE_WIDTH: i32 = 120;
const PIE_FACE: i32 = 48;
const PIE_DEPTH: i32 = 10;
pub const PIE_HEIGHT: i32 = PIE_FACE + PIE_DEPTH;

/// 三维饼图的填充矩形（x, y, 宽, 高, 颜色）：先画下半圆的侧面，再画顶面；已用部分从 3 点钟方向逆时针展开。
pub fn pie(total: u64, free: u64) -> Vec<(i32, i32, i32, i32, u32)> {
    let used = total.saturating_sub(free);
    let fraction = if total == 0 {
        0.0
    } else {
        used as f64 / total as f64
    };
    let (a, b) = (f64::from(PIE_WIDTH) / 2.0, f64::from(PIE_FACE) / 2.0);
    let angle_of = |x: f64, y: f64| {
        let angle = (-(y - b) / b).atan2((x - a) / a);
        if angle < 0.0 {
            angle + std::f64::consts::TAU
        } else {
            angle
        }
    };
    let is_used = |angle: f64| angle < fraction * std::f64::consts::TAU;
    let mut rects = Vec::new();
    for x in 0..PIE_WIDTH {
        let dx = (f64::from(x) + 0.5 - a) / a;
        if dx.abs() > 1.0 {
            continue;
        }
        let edge = b + b * (1.0 - dx * dx).sqrt();
        let color = if is_used(angle_of(f64::from(x) + 0.5, edge)) {
            USED_SIDE
        } else {
            FREE_SIDE
        };
        let top = (edge - 0.5).round() as i32;
        rects.push((x, top.min(PIE_FACE - 1), 1, PIE_DEPTH + 1, color));
    }
    for y in 0..PIE_FACE {
        let mut run: Option<(i32, u32)> = None;
        for x in 0..=PIE_WIDTH {
            let (px, py) = (f64::from(x) + 0.5, f64::from(y) + 0.5);
            let inside = x < PIE_WIDTH && ((px - a) / a).powi(2) + ((py - b) / b).powi(2) <= 1.0;
            let color = inside.then(|| {
                if is_used(angle_of(px, py)) {
                    USED
                } else {
                    FREE
                }
            });
            match (run, color) {
                (Some((start, current)), Some(color)) if current == color => {
                    run = Some((start, current));
                }
                (Some((start, current)), _) => {
                    rects.push((start, y, x - start, 1, current));
                    run = color.map(|color| (x, color));
                }
                (None, Some(color)) => run = Some((x, color)),
                (None, None) => {}
            }
        }
    }
    rects
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_disk_is_all_free_and_a_full_disk_all_used() {
        let colors =
            |total, free| -> Vec<u32> { pie(total, free).into_iter().map(|rect| rect.4).collect() };
        assert!(
            colors(100, 100)
                .iter()
                .all(|color| [FREE, FREE_SIDE].contains(color))
        );
        assert!(
            colors(100, 0)
                .iter()
                .all(|color| [USED, USED_SIDE].contains(color))
        );
        let half = colors(100, 50);
        assert!(half.contains(&USED) && half.contains(&FREE));
    }
}
