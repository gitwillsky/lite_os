//! 属性表绑定只路由业务验证/应用结果，按钮、阶段、焦点与错误展示由共享运行库拥有。
use crate::{
    Application, Error, Layout, Result, Widget, WidgetKind, WidgetSpec, Window, connection::text,
};
use classic_ui_abi as abi;
use std::rc::Rc;
/// 运行库发出的验证令牌，不能手工构造或用作应用阶段令牌。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ValidationToken(pub(crate) u64);
/// 验证通过后运行库发出的应用令牌，防止误把验证结果当作设置已应用。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ApplyToken(pub(crate) u64);
/// 业务验证失败的位置与说明；page/control 必须属于当前属性表。
pub struct ValidationError {
    pub page: Option<Widget>,
    pub control: Option<Widget>,
    pub message: String,
}
/// 页面业务模型的验证接口；验证不得提交系统设置，全部页面通过后才进入应用阶段。
pub trait ValidatePage {
    fn validate(&self) -> std::result::Result<(), ValidationError>;
}
/// 通用属性表，提供页面、OK/Cancel/Apply、校验阶段与错误焦点；不拥有应用的持久设置。
pub struct PropertySheet {
    window: Window,
    pages: Widget,
}
impl PropertySheet {
    /// 创建固定尺寸属性表；parent 可为空，创建或资源错误返回 Err。
    pub fn new(
        app: &mut Application,
        parent: Option<&Window>,
        title: &str,
        width: i32,
        height: i32,
    ) -> Result<Self> {
        if let Some(parent) = parent {
            parent.inner.check()?;
            if !Rc::ptr_eq(&app.connection, &parent.inner.connection) {
                return Err(Error {
                    code: abi::INVALID,
                    message: "parent belongs to another application".into(),
                });
            }
        }
        let mut handles = abi::Sheet::default();
        // SAFETY: 所有输入和输出在调用中有效，父窗口生命周期已验证；运行库原子发布完整树。
        app.connection.check(unsafe {
            (app.connection.api.sheet)(
                app.connection.context.as_ptr(),
                parent.map_or(0, Window::id),
                text(title),
                width,
                height,
                &mut handles,
            )
        })?;
        let window = Window::adopt(app.connection.clone(), handles.window, parent);
        app.windows
            .borrow_mut()
            .insert(window.id(), window.downgrade());
        let pages = Widget::adopt(window.inner.clone(), handles.pages);
        Ok(Self { window, pages })
    }
    /// 返回窗口以匹配事件、建立作用域任务或显式关闭。
    pub fn window(&self) -> &Window {
        &self.window
    }
    /// 添加一页，返回页面列容器；其孩子使用标准 Widget API 创建。
    pub fn page(&self, title: &str) -> Result<Widget> {
        let mut spec = WidgetSpec::new(WidgetKind::Column, title);
        spec.layout = Layout {
            grow: 1,
            padding: 8,
            gap: 6,
            ..Default::default()
        };
        self.pages.child(spec)
    }
    /// 声明业务草稿是否已变化；正在校验/应用时返回错误。
    pub fn set_dirty(&self, dirty: bool) -> Result<()> {
        self.window.inner.check()?;
        let c = &self.window.inner.connection;
        // SAFETY: 当前线程借用有效 context，bool 显式编码为 u32。
        c.check(unsafe {
            (c.api.sheet_dirty)(c.context.as_ptr(), self.window.id(), u32::from(dirty))
        })
    }
    /// 顺序校验所有页面，首次失败定位对应页与控件；不会调用任何应用副作用。
    pub fn validate_all(&self, token: ValidationToken, pages: &[&dyn ValidatePage]) -> Result<()> {
        for page in pages {
            if let Err(error) = page.validate() {
                return self.validated(token, Err(error));
            }
        }
        self.validated(token, Ok(()))
    }
    /// 交付同步或异步业务校验结果；成功后由运行库另发 Apply 事件。
    pub fn validated(
        &self,
        token: ValidationToken,
        result: std::result::Result<(), ValidationError>,
    ) -> Result<()> {
        self.window.inner.check()?;
        let (accepted, page, control, message) = match result {
            Ok(()) => (true, 0, 0, String::new()),
            Err(error) => {
                for widget in error.page.iter().chain(error.control.iter()) {
                    let window = widget.window();
                    if window.id() != self.window.id()
                        || !Rc::ptr_eq(&window.inner.connection, &self.window.inner.connection)
                    {
                        return Err(Error {
                            code: abi::INVALID,
                            message: "validation target belongs to another sheet".into(),
                        });
                    }
                }
                (
                    false,
                    error.page.as_ref().map_or(0, Widget::id),
                    error.control.as_ref().map_or(0, Widget::id),
                    error.message,
                )
            }
        };
        let c = &self.window.inner.connection;
        // SAFETY: 页面与控件归属已验证，文本在调用内复制，令牌由对应运行库事件生成。
        c.check(unsafe {
            (c.api.sheet_validated)(
                c.context.as_ptr(),
                self.window.id(),
                token.0,
                u32::from(accepted),
                page,
                control,
                text(&message),
            )
        })
    }
    /// 交付设置应用结果；成功清除草稿状态，OK 请求在成功后关闭，失败保留草稿并展示说明。
    /// 已提交的外部副作用由业务模型处理，框架不承诺跨时钟/文件的回滚。
    pub fn applied(
        &self,
        token: ApplyToken,
        result: std::result::Result<(), String>,
    ) -> Result<()> {
        self.window.inner.check()?;
        let (success, message) = match result {
            Ok(()) => (true, String::new()),
            Err(error) => (false, error),
        };
        let c = &self.window.inner.connection;
        // SAFETY: 令牌和窗口来自本连接，运行库核对在途序号后推进状态。
        c.check(unsafe {
            (c.api.sheet_applied)(
                c.context.as_ptr(),
                self.window.id(),
                token.0,
                u32::from(success),
                text(&message),
            )
        })
    }
}
impl Drop for PropertySheet {
    fn drop(&mut self) {
        let _ = self.window.close();
    }
}
