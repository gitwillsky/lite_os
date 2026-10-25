//! 按窗口的 `app_id` 取程序图标：任务栏按钮、任务管理器列表与 Alt+Tab 面板共用。

use std::{collections::HashMap, path::Path, rc::Rc};

use crate::{Bitmap, Painter};

const APPLICATIONS: &str = "/usr/share/applications";
const UI_DIRECTORY: &str = "/usr/share/liteos/ui";
/// 没有登记图标的程序用的通用应用图标。
const GENERIC: &str = "file-app";

/// 已加载的某个尺寸的程序图标。
pub struct AppIcons {
    by_app_id: HashMap<String, Rc<Bitmap>>,
    generic: Option<Rc<Bitmap>>,
}

impl AppIcons {
    /// 读取已安装桌面条目里 `StartupWMClass=` 与 `Icon=` 对应的 `<icon>-<size>.bmp`，以及通用图标。
    ///
    /// 缺失或损坏的图标不报错，该程序退回通用图标，通用图标也缺失则不画。
    pub fn load(size: u32) -> Self {
        let directory = Path::new(UI_DIRECTORY);
        let load = |name: &str| {
            Bitmap::load(&directory.join(format!("{name}-{size}.bmp")))
                .ok()
                .map(Rc::new)
        };
        let by_app_id = desktop_entry::icons_by_app_id(Path::new(APPLICATIONS))
            .into_iter()
            .filter_map(|(app_id, icon)| Some((app_id, load(&icon)?)))
            .collect();
        Self {
            by_app_id,
            generic: load(GENERIC),
        }
    }

    /// 窗口 `app_id` 的图标；没有登记时是通用图标。
    pub fn get(&self, app_id: &str) -> Option<&Bitmap> {
        self.by_app_id
            .get(app_id)
            .or(self.generic.as_ref())
            .map(Rc::as_ref)
    }

    /// 取得共享图标 owner，供多个窗口装饰保活同一份解码像素；没有素材时返回 None。
    pub fn owned(&self, app_id: &str) -> Option<Rc<Bitmap>> {
        self.by_app_id
            .get(app_id)
            .or(self.generic.as_ref())
            .cloned()
    }

    /// 在逻辑坐标 `(x, y)` 画 `app_id` 的图标。
    pub fn paint(&self, painter: &mut Painter<'_>, app_id: &str, x: i32, y: i32) {
        if let Some(bitmap) = self.get(app_id) {
            bitmap.paint(painter, x, y);
        }
    }
}
