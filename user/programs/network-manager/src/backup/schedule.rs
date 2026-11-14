//! 自动备份策略编辑，时间单位仅作表单输入换算。

use crate::{APP_ID, child};
use liteos_ui::{
    Dialogs, EventKind, Layout, TreeRow, Widget, WidgetKind, WidgetSpec, Window, WindowOptions,
    state,
};
use network_config::configuration::BackupPolicy;
use std::num::NonZeroU32;

const UNITS: [(u32, &str); 4] = [(1, "秒"), (60, "分钟"), (3600, "小时"), (86400, "天")];

fn parse_amount(text: &str, multiplier: u32) -> Option<NonZeroU32> {
    NonZeroU32::new(text.trim().parse::<u32>().ok()?.checked_mul(multiplier)?)
}

struct Fields {
    enabled: Widget,
    interval: Widget,
    unit: Widget,
    retain: Widget,
    error: Widget,
}

impl Fields {
    fn enabled(&self) -> liteos_ui::Result<bool> {
        Ok(self.enabled.state()? & state::CHECKED != 0)
    }

    fn update_enabled(&self) -> liteos_ui::Result<()> {
        let value = if self.enabled()? { 0 } else { state::DISABLED };
        for field in [&self.interval, &self.retain] {
            field.set_state(value)?;
        }
        self.unit.set_state(value | state::DROPDOWN_LIST)?;
        Ok(())
    }

    fn read(&self) -> liteos_ui::Result<Option<Option<BackupPolicy>>> {
        if !self.enabled()? {
            return Ok(Some(None));
        }
        let Some(unit) = self
            .unit
            .selection()?
            .and_then(|id| UNITS.get((id - 1) as usize))
        else {
            self.error.set_text("请选择间隔单位。")?;
            return Ok(None);
        };
        let Some(interval_seconds) = parse_amount(&self.interval.text()?, unit.0) else {
            self.error.set_text("请输入有效的正整数间隔。")?;
            self.interval.focus()?;
            return Ok(None);
        };
        let Some(retain) = parse_amount(&self.retain.text()?, 1) else {
            self.error.set_text("保留份数须为有效的正整数。")?;
            self.retain.focus()?;
            return Ok(None);
        };
        Ok(Some(Some(BackupPolicy {
            interval_seconds,
            retain,
        })))
    }
}

/// 编辑自动备份策略。外层 None 取消，内层 None 停用调度。
/// # Errors
/// 窗口或控件操作失败。
pub(super) async fn edit(
    dialogs: &Dialogs,
    parent: &Window,
    current: Option<BackupPolicy>,
) -> liteos_ui::Result<Option<Option<BackupPolicy>>> {
    let mut options = WindowOptions::new("定时备份设置", APP_ID, 410, 225);
    options.parent = Some(parent);
    options.modal = true;
    options.fixed = true;
    let (window, mut events) = dialogs.window(options)?;
    let root = window.widget(WidgetSpec {
        layout: Layout {
            grow: 1,
            padding: 12,
            gap: 8,
            ..Layout::default()
        },
        ..WidgetSpec::new(WidgetKind::Column, "")
    })?;
    let row = Layout {
        gap: 8,
        ..Layout::default()
    };
    let label = Layout {
        width: 120,
        ..Layout::default()
    };
    let edit = Layout {
        width: 95,
        height: 23,
        ..Layout::default()
    };
    let enabled = child(
        &root,
        WidgetKind::CheckBox,
        "启用自动备份",
        Layout::default(),
    )?;
    enabled.set_state(if current.is_some() { state::CHECKED } else { 0 })?;
    let time = child(&root, WidgetKind::Row, "", row)?;
    let _time_label = child(&time, WidgetKind::Label, "备份间隔", label)?;
    let interval = child(&time, WidgetKind::Edit, "", edit)?;
    let unit = child(&time, WidgetKind::Combo, "", edit)?;
    unit.set_state(state::DROPDOWN_LIST)?;
    unit.set_tree_rows(&std::array::from_fn::<_, { UNITS.len() }, _>(|index| {
        TreeRow {
            id: index as u64 + 1,
            parent: 0,
            text: UNITS[index].1,
            expanded: false,
            has_children: false,
        }
    }))?;
    let copies = child(&root, WidgetKind::Row, "", row)?;
    let _copies_label = child(&copies, WidgetKind::Label, "保留自动备份", label)?;
    let retain = child(&copies, WidgetKind::Edit, "", edit)?;
    let _copies_unit = child(&copies, WidgetKind::Label, "份", Layout::default())?;
    let error = child(
        &root,
        WidgetKind::Label,
        "",
        Layout {
            grow: 1,
            ..Layout::default()
        },
    )?;
    error.set_state(state::WRAP)?;
    let actions = child(&root, WidgetKind::Row, "", row)?;
    let _spacer = child(
        &actions,
        WidgetKind::Column,
        "",
        Layout {
            grow: 1,
            ..Layout::default()
        },
    )?;
    let save = child(&actions, WidgetKind::Button, "审阅…", edit)?;
    let cancel = child(&actions, WidgetKind::Button, "取消", edit)?;
    save.set_state(state::DEFAULT)?;

    if let Some(policy) = current {
        let seconds = policy.interval_seconds.get();
        let index = UNITS
            .iter()
            .rposition(|&(scale, _)| seconds.is_multiple_of(scale))
            .unwrap_or(0);
        interval.set_text(&(seconds / UNITS[index].0).to_string())?;
        unit.select(Some(index as u64 + 1))?;
        retain.set_text(&policy.retain.to_string())?;
    } else {
        unit.select(Some(UNITS.len() as u64))?;
    }
    let fields = Fields {
        enabled,
        interval,
        unit,
        retain,
        error,
    };
    fields.update_enabled()?;
    fields.enabled.focus()?;
    while let Some(event) = events.next().await? {
        match event.kind {
            EventKind::Close
            | EventKind::Key {
                code: 1,
                pressed: true,
                ..
            } => break,
            EventKind::Activate { .. } if event.node == cancel.id() => break,
            EventKind::Activate { .. } if event.node == fields.enabled.id() => {
                fields.update_enabled()?;
                fields.error.set_text("")?;
            }
            EventKind::Activate { .. } if event.node == save.id() => {
                if let Some(policy) = fields.read()? {
                    return Ok(Some(policy));
                }
            }
            _ => {}
        }
    }
    Ok(None)
}
