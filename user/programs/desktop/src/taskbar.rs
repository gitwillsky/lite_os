//! 任务栏图层：层次/高度/独占区、自动隐藏、任务按钮矩形与悬停提示。
//! 状态仍由 Desktop 唯一持有；本模块只是同一 owner 的任务栏方法。
use super::{Desktop, TASKBAR_HEIGHT, paint};
use crate::{dialogs::Kind, start};
use classic_ui::{
    Display, Event, EventKind, KeyboardInteractivity, Layer, LayerSettings, ToplevelRequest,
    layer_anchor,
};
use std::{
    io,
    time::{Duration, Instant},
};

/// 自动隐藏的任务栏收起后露出的细线高度。
const HIDDEN_HEIGHT: i32 = 2;
/// 指针离开多久后收起自动隐藏的任务栏。
const HIDE_DELAY: Duration = Duration::from_millis(400);

impl Desktop {
    /// 只移动任务按钮的逻辑焦点，Enter 才激活窗口；直接遍历现有快照，不复制任务列表。
    fn cycle_tasks(&mut self, display: &mut Display, reverse: bool) -> io::Result<()> {
        let count = display.toplevels().count();
        if count == 0 {
            self.task_focus = None;
            return Ok(());
        }
        let current = self.task_focus.or_else(|| {
            display
                .toplevels()
                .find(|(_, info)| info.active)
                .map(|(id, _)| id)
        });
        let index =
            current.and_then(|current| display.toplevels().position(|(id, _)| id == current));
        let index = match (index, reverse) {
            (Some(index), false) => (index + 1) % count,
            (Some(index), true) => (index + count - 1) % count,
            (None, false) => 0,
            (None, true) => count - 1,
        };
        self.task_focus = display.toplevels().nth(index).map(|(id, _)| id);
        if self.taskbar_hidden {
            self.taskbar_hidden = false;
            // 键盘展开不会产生 PointerLeave；记录收起期限，焦点存在时暂停收起。
            self.leave_at = Some(Instant::now());
            self.apply_taskbar(display)?;
        }
        self.sync_task_focus(display);
        Ok(())
    }

    /// 窗口集合或任务栏尺寸变化后裁决焦点与分页；带 generation 的句柄不能命中复用的旧编号。
    pub(super) fn sync_task_focus(&mut self, display: &Display) {
        let Some(current) = self.task_focus else {
            return;
        };
        if display.toplevel(current).is_none() {
            self.task_focus = display.toplevels().next().map(|(id, _)| id);
        }
        if let Some(index) = self
            .task_focus
            .and_then(|current| display.toplevels().position(|(id, _)| id == current))
        {
            let width = display.configuration(self.taskbar).map_or(0, |c| c.width);
            self.task_page = index / paint::task_capacity(self, display, width);
        }
    }

    /// 背景 surface 承载外壳的键盘，逻辑焦点可落在另一 surface 的任务按钮上；
    /// 该焦点存在时键盘不能落入背景文件的删除/重命名等命令。
    pub(super) fn task_key(
        &mut self,
        display: &mut Display,
        art: &mut paint::Art,
        event: &Event,
    ) -> io::Result<bool> {
        match event.kind {
            EventKind::Button { pressed: true, .. } => self.task_focus = None,
            EventKind::Focus(false)
                if (event.window == self.background || event.window == self.taskbar)
                    && self.overlay.is_none()
                    && self.menu.visible().is_none() =>
            {
                self.task_focus = None
            }
            EventKind::Configure(_) if event.window == self.taskbar => {
                self.sync_task_focus(display)
            }
            EventKind::ToplevelsChanged => self.sync_task_focus(display),
            _ => {}
        }
        if self.task_focus.is_none()
            || (event.window != self.background && event.window != self.taskbar)
            || self.overlay.is_some()
            || self.menu.visible().is_some()
        {
            return Ok(false);
        }
        let EventKind::Key(key) = event.kind else {
            return Ok(false);
        };
        if !key.pressed || key.modifiers.control || key.modifiers.alt || key.modifiers.super_key {
            return Ok(true);
        }
        match key.code {
            15 => self.cycle_tasks(display, key.modifiers.shift)?,
            103 | 105 => self.cycle_tasks(display, true)?,
            106 | 108 => self.cycle_tasks(display, false)?,
            102 | 107 => {
                self.task_focus = if key.code == 102 {
                    display.toplevels().next().map(|(id, _)| id)
                } else {
                    display.toplevels().last().map(|(id, _)| id)
                };
                self.sync_task_focus(display);
            }
            28 | 57 if !key.repeat => {
                if let Some(id) = self
                    .task_focus
                    .take()
                    .filter(|id| display.toplevel(*id).is_some())
                {
                    display.control_toplevel(id, ToplevelRequest::Activate)?;
                }
            }
            1 => {
                self.task_focus = None;
                let active = display
                    .toplevels()
                    .find(|(_, info)| info.active)
                    .map(|(id, _)| id);
                if let Some(id) = active {
                    display.control_toplevel(id, ToplevelRequest::Activate)?;
                }
            }
            68 | 127 | 139 if !key.repeat && key.modifiers.shift == (key.code == 68) => {
                self.context_menu(display, art, self.taskbar, true)?;
            }
            _ => {}
        }
        Ok(true)
    }

    /// 外壳快捷键先于菜单与编辑控件解释；未消费的事件继续原来的输入路径。
    pub(super) fn shell_key(
        &mut self,
        display: &mut Display,
        event: &EventKind,
    ) -> io::Result<bool> {
        if matches!(
            event,
            EventKind::Focus(false) | EventKind::Button { pressed: true, .. }
        ) {
            self.start_key = None;
        }
        let EventKind::Key(key) = event else {
            return Ok(false);
        };
        if matches!(key.code, 125 | 126) {
            if key.pressed && !key.repeat {
                self.start_key =
                    (!key.modifiers.control && !key.modifiers.alt && !key.modifiers.shift)
                        .then_some(key.code);
            } else if !key.pressed && self.start_key.take() == Some(key.code) {
                self.toggle_menu(display)?;
            }
            return Ok(true);
        }
        if !key.pressed {
            return Ok(false);
        }
        self.start_key = None;
        let modifiers = key.modifiers;
        let control_escape =
            key.code == 1 && modifiers.control && !modifiers.alt && !modifiers.super_key;
        let win_command = matches!(key.code, 15 | 18 | 19 | 32 | 33 | 50 | 119)
            && modifiers.super_key
            && !modifiers.alt
            && !modifiers.control
            && (!modifiers.shift || matches!(key.code, 15 | 50));
        if !control_escape && !win_command {
            return Ok(false);
        }
        if key.repeat && key.code != 15 {
            return Ok(true);
        }
        if key.code != 15 {
            self.task_focus = None;
        }
        if control_escape && !modifiers.shift {
            self.toggle_menu(display)?;
        } else {
            if self.menu.visible().is_some() {
                self.toggle_menu(display)?;
            }
            match key.code {
                15 => {
                    self.commit_rename();
                    if self.rename.is_none() {
                        self.cycle_tasks(display, modifiers.shift)?;
                    }
                }
                1 => self.launch_task_manager(),
                18 => self.open_place(start::Place::MyComputer),
                33 => self.open_place(start::Place::Search),
                19 => self.dialog(display, Kind::Run)?,
                32 => display.control_shell(classic_ui::ShellCommand::ToggleDesktop)?,
                50 if modifiers.shift => {
                    display.control_shell(classic_ui::ShellCommand::UndoMinimizeAll)?
                }
                50 => display.control_shell(classic_ui::ShellCommand::MinimizeAll)?,
                119 => self.control_panel("system"),
                _ => unreachable!("matched shell shortcut"),
            }
        }
        Ok(true)
    }

    /// 任务栏图层的设置：层次（总在最前）、高度与独占区（自动隐藏时收起并释放独占区）。
    pub(super) fn taskbar_layer(
        settings: &desktop_settings::Settings,
        hidden: bool,
    ) -> LayerSettings {
        let height = if settings.auto_hide && hidden {
            HIDDEN_HEIGHT
        } else {
            TASKBAR_HEIGHT
        };
        LayerSettings {
            layer: if settings.always_on_top {
                Layer::Top
            } else {
                Layer::Bottom
            },
            height: height as u32,
            anchor: layer_anchor::LEFT | layer_anchor::RIGHT | layer_anchor::BOTTOM,
            // 自动隐藏时窗口使用整个屏幕，任务栏浮在上面；否则为它保留一条。
            exclusive_zone: if settings.auto_hide {
                0
            } else {
                TASKBAR_HEIGHT
            },
            keyboard: KeyboardInteractivity::OnDemand,
            ..Default::default()
        }
    }

    /// 按当前设置重新配置任务栏图层。
    pub(super) fn apply_taskbar(&mut self, display: &mut Display) -> io::Result<()> {
        let mut settings = Self::taskbar_layer(&self.settings, self.taskbar_hidden);
        // 非置顶任务栏取得键盘时也必须可见；结束导航后由事件边界重新应用用户的层次设置。
        if self.task_focus.is_some() {
            settings.layer = Layer::Top;
        }
        display.configure_layer(self.taskbar, settings)
    }

    /// 设置文件变了就重读并应用；返回是否需要重画。
    pub(super) fn poll_settings(&mut self, display: &mut Display) -> io::Result<bool> {
        let stamp = desktop_settings::Settings::stamp();
        if stamp == self.settings_stamp {
            return Ok(false);
        }
        self.settings_stamp = stamp;
        let settings = desktop_settings::Settings::load();
        let layer_changed = settings.always_on_top != self.settings.always_on_top
            || settings.auto_hide != self.settings.auto_hide;
        self.settings = settings;
        if layer_changed {
            self.taskbar_hidden = false;
            self.leave_at = None;
            self.apply_taskbar(display)?;
            self.resize_icons(display);
        }
        Ok(true)
    }

    /// 把每个任务按钮的位置告诉 compositor，最小化与还原动画用它作端点。
    pub(super) fn sync_task_rectangles(&self, display: &mut Display) -> io::Result<()> {
        let Some(c) = display.configuration(self.taskbar) else {
            return Ok(());
        };
        let rects = paint::areas(self, display, self.taskbar, c);
        let ids = paint::task_ids(self, display, c.width);
        for (index, id) in ids.into_iter().enumerate() {
            if let Some(rect) = rects.get(paint::TASKS_FROM + index) {
                display.set_toplevel_rectangle(id, self.taskbar, *rect)?;
            }
        }
        Ok(())
    }

    /// 某个任务栏命中区的提示文字；没有提示的区域为 `None`。
    pub(super) fn tooltip_text(&self, display: &Display, target: usize) -> Option<String> {
        match target {
            paint::CLOCK => {
                let days = calendar::zone::to_local(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .ok()?
                        .as_secs() as i64,
                )
                .div_euclid(86_400);
                let date = calendar::civil_from_days(days);
                let weekday = ["日", "一", "二", "三", "四", "五", "六"]
                    [calendar::weekday(days) as usize % 7];
                Some(format!(
                    "{}年{}月{}日 星期{weekday}",
                    date.year, date.month, date.day
                ))
            }
            paint::NETWORK => self.tray.network.tooltip(&self.settings),
            paint::USB => self.tray.usb.tooltip(),
            paint::VOLUME => Some(match self.volume.value {
                Some(value) if value.muted => "音量：静音".to_owned(),
                Some(value) => format!("音量：{}%", value.left.max(value.right)),
                None => "音量".to_owned(),
            }),
            n if n >= paint::TASKS_FROM => {
                let width = Self::screen(display, self.background).0;
                let ids = paint::task_ids(self, display, width);
                let (_, info) = display
                    .toplevels()
                    .find(|(id, _)| Some(id) == ids.get(n - paint::TASKS_FROM))?;
                Some(info.title.clone())
            }
            _ => None,
        }
    }

    /// 让提示图层与悬停状态一致：到点显示，换了目标或指针离开就撤掉。
    pub(super) fn sync_tooltip(
        &mut self,
        display: &mut Display,
        art: &mut paint::Art,
    ) -> io::Result<()> {
        let wanted = self
            .tooltip
            .visible()
            .and_then(|target| Some((target, self.tooltip_text(display, target)?)));
        let current = self.tooltip_window.as_ref().map(|(_, text)| text.clone());
        if wanted.as_ref().map(|(_, text)| text) == current.as_ref() {
            return Ok(());
        }
        if let Some((window, _)) = self.tooltip_window.take() {
            display.destroy_window(window)?;
        }
        let Some((_, text)) = wanted else {
            return Ok(());
        };
        let (screen_width, _) = Self::screen(display, self.background);
        let scale = display.configuration(self.taskbar).map_or(1, |c| c.scale);
        let (width, height) = art.tooltip_size(&text, scale)?;
        let x = (self.pointer.1 as i32).min(screen_width - width);
        let window = display.create_layer(
            "org.liteos.tooltip",
            LayerSettings {
                layer: Layer::Overlay,
                width: width as u32,
                height: height as u32,
                anchor: layer_anchor::LEFT | layer_anchor::BOTTOM,
                // 位置：左边距取指针横坐标，下边距让它落在任务栏之上。
                margin: [0, 0, TASKBAR_HEIGHT + 4, x.max(0)],
                exclusive_zone: -1,
                ..Default::default()
            },
        )?;
        self.tooltip_window = Some((window, text));
        Ok(())
    }

    /// 自动隐藏：指针进入细线就展开；离开一小会儿且没有菜单或弹出层占着任务栏时收起。
    pub(super) fn tick_auto_hide(&mut self, display: &mut Display) -> io::Result<()> {
        if !self.settings.auto_hide {
            return Ok(());
        }
        if self.leave_at.is_some_and(|at| {
            at.elapsed() >= HIDE_DELAY
                && self.menu.visible().is_none()
                && self.overlay.is_none()
                && self.task_focus.is_none()
        }) {
            self.leave_at = None;
            self.taskbar_hidden = true;
            self.apply_taskbar(display)?;
        }
        Ok(())
    }
}
