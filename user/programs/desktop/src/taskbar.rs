//! 任务栏图层：层次/高度/独占区、自动隐藏、任务按钮矩形与悬停提示。
//! 状态仍由 Desktop 唯一持有；本模块只是同一 owner 的任务栏方法。
use super::{Desktop, TASKBAR_HEIGHT, paint};
use classic_ui::{Display, KeyboardInteractivity, Layer, LayerSettings, layer_anchor};
use std::{io, time::Duration};

/// 自动隐藏的任务栏收起后露出的细线高度。
const HIDDEN_HEIGHT: i32 = 2;
/// 指针离开多久后收起自动隐藏的任务栏。
const HIDE_DELAY: Duration = Duration::from_millis(400);

impl Desktop {
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
        display.configure_layer(
            self.taskbar,
            Self::taskbar_layer(&self.settings, self.taskbar_hidden),
        )
    }

    /// 设置文件变了就重读并应用；返回是否需要重画。
    pub(super) fn poll_settings(&mut self, display: &mut Display) -> io::Result<bool> {
        let stamp = desktop_settings::Settings::stamp();
        if stamp == self.settings_stamp {
            return Ok(false);
        }
        self.settings_stamp = stamp;
        self.settings = desktop_settings::Settings::load();
        self.taskbar_hidden = false;
        self.leave_at = None;
        self.apply_taskbar(display)?;
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
            at.elapsed() >= HIDE_DELAY && self.menu.is_none() && self.overlay.is_none()
        }) {
            self.leave_at = None;
            self.taskbar_hidden = true;
            self.apply_taskbar(display)?;
        }
        Ok(())
    }
}
