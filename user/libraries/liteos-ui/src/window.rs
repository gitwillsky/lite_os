//! 安全窗口与控件句柄，稳定 ABI 内部保留所有控件行为。
use crate::{
    Error, Result,
    connection::{Connection, text},
    tasks::Scope,
};
use classic_ui_abi::{self as abi, Layout, Rect};
use std::{
    cell::{Cell, RefCell},
    rc::{Rc, Weak},
};

/// 窗口创建规格；parent 是本连接中的瞬时父窗口关系，不自动承诺模态策略。
pub struct WindowOptions<'a> {
    pub title: &'a str,
    pub app_id: &'a str,
    pub width: i32,
    pub height: i32,
    pub fixed: bool,
    /// 模态窗口要求 parent；运行库按 xdg-dialog 筛选父窗口交互，并发送可选的标准提示。
    pub modal: bool,
    pub parent: Option<&'a Window>,
}
impl<'a> WindowOptions<'a> {
    /// 创建默认可缩放窗口规格；尺寸为逻辑像素。
    pub fn new(title: &'a str, app_id: &'a str, width: i32, height: i32) -> Self {
        Self {
            title,
            app_id,
            width,
            height,
            fixed: false,
            modal: false,
            parent: None,
        }
    }
}
pub(crate) struct WindowState {
    pub connection: Rc<Connection>,
    pub id: u64,
    pub scope: Rc<Scope>,
    closed: Cell<bool>,
    children: RefCell<Vec<Weak<WindowState>>>,
    _parent: Option<Rc<WindowState>>,
}
impl WindowState {
    fn close(&self) -> Result<()> {
        if self.closed.replace(true) {
            return Ok(());
        }
        self.scope.cancel();
        let children = std::mem::take(&mut *self.children.borrow_mut());
        for child in children.into_iter().filter_map(|child| child.upgrade()) {
            let _ = child.close();
        }
        // SAFETY: SDK 持有唯一窗口销毁权，所有孩子已先撤销，不存在回调嵌套。
        self.connection.check(unsafe {
            (self.connection.api.close)(self.connection.context.as_ptr(), self.id)
        })
    }
    pub fn check(&self) -> Result<()> {
        if self.closed.get() {
            Err(Error {
                code: abi::NOT_FOUND,
                message: "window is closed".into(),
            })
        } else {
            Ok(())
        }
    }
}
impl Drop for WindowState {
    fn drop(&mut self) {
        let _ = self.close();
    }
}
/// UI 线程窗口句柄；最后一个 owner 释放时关闭窗口。显式 close 同时撤销作用域任务。
#[derive(Clone)]
pub struct Window {
    pub(crate) inner: Rc<WindowState>,
}
/// 不延长窗口生命周期的引用，适合跨 await 捕获。
#[derive(Clone)]
pub struct WeakWindow {
    inner: Weak<WindowState>,
}
impl WeakWindow {
    /// 窗口仍存在且尚未关闭时返回句柄，否则返回 None。
    pub fn upgrade(&self) -> Option<Window> {
        self.inner
            .upgrade()
            .filter(|inner| !inner.closed.get())
            .map(|inner| Window { inner })
    }
}
impl Window {
    pub(crate) fn new(connection: Rc<Connection>, options: WindowOptions<'_>) -> Result<Self> {
        if let Some(parent) = options.parent {
            parent.inner.check()?;
            if !Rc::ptr_eq(&connection, &parent.inner.connection) {
                return Err(Error {
                    code: abi::INVALID,
                    message: "parent belongs to another UI context".into(),
                });
            }
        }
        let mut id = 0;
        // SAFETY: 字符串有效直到调用返回，输出 id 独占，parent 已验证连接与生命周期。
        connection.check(unsafe {
            (connection.api.window)(
                connection.context.as_ptr(),
                text(options.title),
                text(options.app_id),
                options.width,
                options.height,
                u32::from(options.fixed) | (u32::from(options.modal) << 1),
                options.parent.map_or(0, |p| p.inner.id),
                &mut id,
            )
        })?;
        Ok(Self::adopt(connection, id, options.parent))
    }
    pub(crate) fn adopt(connection: Rc<Connection>, id: u64, parent: Option<&Window>) -> Self {
        let inner = Rc::new(WindowState {
            connection,
            id,
            scope: Rc::new(Scope::new()),
            closed: Cell::new(false),
            children: RefCell::new(Vec::new()),
            _parent: parent.map(|parent| parent.inner.clone()),
        });
        if let Some(parent) = parent {
            parent
                .inner
                .children
                .borrow_mut()
                .push(Rc::downgrade(&inner));
        }
        Self { inner }
    }
    /// 返回稳定窗口标识，用于匹配事件；不能用于另一连接。
    pub fn id(&self) -> u64 {
        self.inner.id
    }
    /// 创建不延长存活期的窗口引用。
    pub fn downgrade(&self) -> WeakWindow {
        WeakWindow {
            inner: Rc::downgrade(&self.inner),
        }
    }
    /// 修改标题；窗口关闭、连接或参数失败返回错误。
    pub fn set_title(&self, title: &str) -> Result<()> {
        self.inner.check()?;
        let c = &self.inner.connection;
        // SAFETY: 连接与文本在调用中有效，本 API 不执行应用回调。
        c.check(unsafe { (c.api.title)(c.context.as_ptr(), self.id(), text(title)) })
    }
    /// 请求进入或退出全屏（标准 xdg_toplevel）；实际尺寸由随后的 Configure 事件给出。
    pub fn set_fullscreen(&self, on: bool) -> Result<()> {
        self.inner.check()?;
        let c = &self.inner.connection;
        // SAFETY: 连接在调用中有效，参数为标量。
        c.check(unsafe { (c.api.fullscreen)(c.context.as_ptr(), self.id(), u32::from(on)) })
    }
    /// 更新标准桌面应用标识与程序图标；非法句柄或协议失败返回错误。
    pub fn set_app_id(&self, value: &str) -> Result<()> {
        self.inner.check()?;
        let c = &self.inner.connection;
        // SAFETY: 文本在同步调用内有效，运行库复制后持有。
        c.check(unsafe { (c.api.app_id)(c.context.as_ptr(), self.id(), text(value)) })
    }
    /// 关闭窗口并取消其任务；重复 close 成功，无副作用。
    pub fn close(&self) -> Result<()> {
        self.inner.close()
    }
    /// 在窗口根布局中创建控件；参数/连接错误不发布半个节点。
    pub fn widget(&self, spec: WidgetSpec<'_>) -> Result<Widget> {
        Widget::new(self.inner.clone(), None, spec)
    }
}

/// 可用标准控件；其交互与绘制均在共享运行库中实现。
#[derive(Clone, Copy, Debug)]
#[repr(u32)]
pub enum WidgetKind {
    Column = 1,
    Row,
    Label,
    Button,
    Edit,
    CheckBox,
    Radio,
    List,
    Tabs,
    Status,
    Canvas,
    ScrollBar,
    Slider,
    MenuBar,
    Tree,
    TextEditor,
    Image,
    Group,
    Calendar,
    PopupMenu,
    /// 平面位图工具栏；按钮通过 [`Widget::set_toolbar`] 设置。
    Toolbar,
    /// 带区容器：每个子控件一行，带把手、边线与品牌 logo；忙碌动画用 [`Widget::set_busy`]。
    Rebar,
    /// 超链接文字：激活发出 [`crate::EventKind::Activate`]。
    Link,
    /// 可编辑下拉组合框：项目用 [`Widget::set_tree_rows`]/[`Widget::set_row_icons`] 设置，
    /// 选择发出 Select，收起时 Enter 发出 Activate。
    Combo,
    /// 微调钮：值用 [`Widget::set_range`]，按下或按住时发出 Changed。
    UpDown,
}
/// 系统字体用途，对应主题 `[Fonts]` 中的逻辑字体。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FontRole {
    Caption = 0,
    SmallCaption = 1,
    Menu = 2,
    Status = 3,
    Message = 4,
    Icon = 5,
}

/// 工具栏按钮的种类（comctl32 BTNS_*）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolbarKind {
    Button,
    /// 复选按钮：每次单击切换选中状态。
    Check,
    /// 分体下拉：右侧箭头按下时发出 [`crate::EventKind::Dropdown`]。
    Dropdown,
    /// 整体下拉：整个按钮按下即发出 Dropdown。
    WholeDropdown,
    Separator,
}

/// 一个工具栏按钮。`strip` 是系统位图条名（如 `toolbar`、`ie-toolbar`），`frame` 为帧序号。
pub struct ToolbarButton<'a> {
    pub id: u64,
    pub kind: ToolbarKind,
    pub strip: &'a str,
    pub frame: u32,
    pub text: &'a str,
    /// 选择性文字：在图像右侧显示文字。
    pub show_text: bool,
    pub enabled: bool,
    pub checked: bool,
}

/// 控件规格；文本在创建时复制，group 只用于同父节点下的 radio 互斥。
pub struct WidgetSpec<'a> {
    pub kind: WidgetKind,
    pub text: &'a str,
    pub layout: Layout,
    pub state: u32,
    pub group: u64,
}
impl<'a> WidgetSpec<'a> {
    /// 以自动尺寸建立规格；布局与状态可通过字段修改。
    pub fn new(kind: WidgetKind, text: &'a str) -> Self {
        Self {
            kind,
            text,
            layout: Layout::default(),
            state: 0,
            group: 0,
        }
    }
}
struct WidgetState {
    window: Rc<WindowState>,
    id: u64,
    _parent: Option<Rc<WidgetState>>,
    scope: Rc<Scope>,
}
impl Drop for WidgetState {
    fn drop(&mut self) {
        self.scope.cancel();
        if self.window.check().is_ok() {
            let c = &self.window.connection;
            // SAFETY: 控件独占回收权；parent 的 Rc 保证父节点不早于仍存活的孩子析构。
            let _ = unsafe { (c.api.remove)(c.context.as_ptr(), self.window.id, self.id) };
        }
    }
}
/// 类型安全的控件 owner；最后一个 owner 析构时删除节点，其生命周期包含父窗口。
#[derive(Clone)]
pub struct Widget {
    inner: Rc<WidgetState>,
}
/// 列表图标来源；只引用系统资源，不把应用像素或分配器传给运行库。
pub enum Icon<'a> {
    /// 系统经典资源的名称。
    Resource(&'a str),
    /// 已安装应用的 app_id，由其 Desktop Entry 查图标。
    Application(&'a str),
    /// Desktop Entry / MIME 声明的主题图标名或绝对 PNG/BMP 路径。
    Theme(&'a str),
}
/// 行图标声明，id 必须已存在于当前列表/树。
pub struct RowIcon<'a> {
    pub id: u64,
    pub source: Icon<'a>,
}
/// 自绘内容可查询的主题语义色，值始终来自共享资源上下文。
#[derive(Clone, Copy)]
#[repr(u32)]
pub enum ColorRole {
    WindowText,
    Window,
    Control,
    Desktop,
    Highlight,
    HighlightText,
    DisabledText,
}
/// 列表查看方式，几何、命中、键盘、滚动与绘制全部由运行库拥有。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum ListStyle {
    Details,
    LargeIcons,
    SmallIcons,
    List,
}
/// 列表的选择策略。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ListOptions {
    /// 允许多选（Ctrl/Shift 与框选由运行库处理）。
    pub multiple: bool,
    /// 详细资料视图整行高亮（comctl32 LVS_EX_FULLROWSELECT）；默认只高亮首列标签。
    pub full_row_select: bool,
}
/// 列正文的对齐方式。
#[derive(Clone, Copy)]
#[repr(u32)]
pub enum ColumnAlignment {
    Left,
    Center,
    Right,
}
/// 标准编辑动作；多行和单行控件使用同一命令契约。
pub enum EditCommand<'a> {
    Cut,
    Copy,
    Paste,
    Delete,
    SelectAll,
    Insert(&'a str),
}
/// 一个自绘命令，坐标相对 Canvas 客户区；文本在 set_drawing 返回前复制。
/// kind=1 填充、2 边框、3 普通文字、4 换行文字、5 系统等宽文字（8×16 逻辑像素，宽字符两格）、
/// 6 在矩形内水平垂直居中的单行文字（超宽截断）、7 墙纸预览（矩形代表整个屏幕，text 为图片路径，
/// color 为显示方式 0 居中、1 平铺、2 拉伸；居中与平铺按屏幕比例缩小原图，图片无法解码时不绘制）。
pub struct Draw<'a> {
    pub kind: u32,
    pub rect: Rect,
    pub color: u32,
    pub bold: bool,
    pub text: &'a str,
}
/// 树节点数据；标识应在刷新前后稳定，父先子后。expanded 仅在节点首次出现时生效。
pub struct TreeRow<'a> {
    pub id: u64,
    pub parent: u64,
    pub text: &'a str,
    pub expanded: bool,
    /// 子节点尚未载入时也显示展开入口；Expand 事件由业务异步提供子节点数据。
    pub has_children: bool,
}
/// 列表列；相同 id 的用户列宽在后续数据更新中保留。排序由应用负责，Windows 2000 的表头不画排序标记。
pub struct Column<'a> {
    pub id: u64,
    pub title: &'a str,
    pub width: i32,
}
/// 菜单条目，state 采用 disabled=1、checked=2、separator_before=4、default=8（粗体默认项）。
///
/// `parent` 为 0 是菜单的直接条目；否则是排在前面的父条目 id，父条目因此成为级联子菜单
/// （只展开、不产生 Activate），嵌套最多 [`abi::MENU_DEPTH`] 层。
pub struct MenuItem<'a> {
    pub id: u64,
    pub label: &'a str,
    pub shortcut: &'a str,
    pub state: u32,
    pub parent: u64,
}
/// 菜单栏规格，内容在 set_menus 调用期间借用。
pub struct Menu<'a> {
    pub title: &'a str,
    pub items: &'a [MenuItem<'a>],
}
impl Widget {
    fn new(
        window: Rc<WindowState>,
        parent: Option<Rc<WidgetState>>,
        spec: WidgetSpec<'_>,
    ) -> Result<Self> {
        window.check()?;
        let c = &window.connection;
        let mut id = 0;
        let value = abi::Widget {
            kind: spec.kind as u32,
            text: text(spec.text),
            state: spec.state,
            group: spec.group,
            layout: spec.layout,
        };
        // SAFETY: 规格、字符串与输出在调用中有效，parent 由相同 window 的子节点入口传入。
        c.check(unsafe {
            (c.api.widget)(
                c.context.as_ptr(),
                window.id,
                parent.as_ref().map_or(0, |p| p.id),
                &value,
                &mut id,
            )
        })?;
        Ok(Self::owned(window, id, parent))
    }
    pub(crate) fn adopt(window: Rc<WindowState>, id: u64) -> Self {
        Self::owned(window, id, None)
    }
    fn owned(window: Rc<WindowState>, id: u64, parent: Option<Rc<WidgetState>>) -> Self {
        let scope = Rc::new(Scope::new());
        window.scope.observe(scope.clone());
        Self {
            inner: Rc::new(WidgetState {
                window,
                id,
                _parent: parent,
                scope,
            }),
        }
    }
    pub(crate) fn scope(&self) -> &Scope {
        &self.inner.scope
    }
    pub(crate) fn window(&self) -> Window {
        Window {
            inner: self.inner.window.clone(),
        }
    }
    /// 返回控件标识，与事件中的 node 字段匹配。
    pub fn id(&self) -> u64 {
        self.inner.id
    }
    /// 在本布局容器中创建孩子；非容器返回错误。
    pub fn child(&self, spec: WidgetSpec<'_>) -> Result<Self> {
        Self::new(self.inner.window.clone(), Some(self.inner.clone()), spec)
    }
    fn call(&self, operation: impl FnOnce(&Connection, u64, u64) -> i32) -> Result<()> {
        self.inner.window.check()?;
        let c = &self.inner.window.connection;
        c.check(operation(c, self.inner.window.id, self.id()))
    }
    /// 替换标签或编辑内容；运行库重新排版并请求重绘。
    pub fn set_text(&self, value: &str) -> Result<()> {
        self.call(|c, w, id| {
            // SAFETY: call 保证窗口存活且同线程，文本在本次调用内有效。
            unsafe { (c.api.text)(c.context.as_ptr(), w, id, text(value)) }
        })
    }
    /// 读取编辑草稿或标签；返回应用拥有的 String。
    pub fn text(&self) -> Result<String> {
        self.inner.window.check()?;
        let c = &self.inner.window.connection;
        c.read(|buffer, len, written| {
            // SAFETY: 输出由 read 独占，context 和节点标识来自本句柄。
            unsafe {
                (c.api.read_text)(
                    c.context.as_ptr(),
                    self.inner.window.id,
                    self.id(),
                    buffer,
                    len,
                    written,
                )
            }
        })
    }
    /// 替换控件状态位；未知状态位返回错误。
    pub fn set_state(&self, value: u32) -> Result<()> {
        self.call(|c, w, id| {
            // SAFETY: call 保证句柄存活，状态由运行库验证。
            unsafe { (c.api.state)(c.context.as_ptr(), w, id, value) }
        })
    }
    /// 查询运行库拥有的控件当前状态位，不在应用维护第二份交互状态。
    pub fn state(&self) -> Result<u32> {
        let mut state = 0;
        self.call(|c, w, id| {
            // SAFETY: 输出独占，句柄和 context 在同步调用内有效。
            unsafe { (c.api.read_state)(c.context.as_ptr(), w, id, &mut state) }
        })?;
        Ok(state)
    }
    /// 修改布局规则；非法尺寸返回错误。
    pub fn set_layout(&self, value: Layout) -> Result<()> {
        self.call(|c, w, id| {
            // SAFETY: call 保证句柄存活，布局以值传递。
            unsafe { (c.api.layout)(c.context.as_ptr(), w, id, value) }
        })
    }
    /// 用 256 个 0xRRGGBB 颜色替换 Image 的调色板；尺寸和区域下标保持不变。
    /// 非 Image、非下标位图或颜色越界返回错误且旧图像不变；未安装的可选图像保持空白。
    pub fn set_image_palette(&self, palette: &[u32; 256]) -> Result<()> {
        self.call(|c, w, id| {
            // SAFETY: call 保活句柄，定长数组在同步调用中有效；运行库不保留输入指针。
            unsafe {
                (c.api.image_palette)(c.context.as_ptr(), w, id, palette.as_ptr(), palette.len())
            }
        })
    }
    /// 设置滚动条/滑块范围；非法范围或错误控件类型返回错误。
    pub fn set_range(&self, value: abi::Range) -> Result<()> {
        self.call(|c, w, id| {
            // SAFETY: 窗口存活，范围以值传递，由运行库校验。
            unsafe { (c.api.range)(c.context.as_ptr(), w, id, value) }
        })
    }
    /// 读取运行库拥有的当前范围值，不在应用端维护第二份控件状态。
    pub fn range(&self) -> Result<abi::Range> {
        let mut value = abi::Range::default();
        self.call(|c, w, id| {
            // SAFETY: 输出位置独占，调用返回后应用持有独立值。
            unsafe { (c.api.read_range)(c.context.as_ptr(), w, id, &mut value) }
        })?;
        Ok(value)
    }
    /// 替换菜单数据；命令以 Activate.value 返回，禁用项不激活。
    pub fn set_menus(&self, values: &[Menu<'_>]) -> Result<()> {
        let items: Vec<Vec<_>> = values
            .iter()
            .map(|menu| {
                menu.items
                    .iter()
                    .map(|item| abi::MenuItem {
                        id: item.id,
                        label: text(item.label),
                        shortcut: text(item.shortcut),
                        state: item.state,
                        parent: item.parent,
                    })
                    .collect()
            })
            .collect();
        let menus: Vec<_> = values
            .iter()
            .zip(&items)
            .map(|(menu, items)| abi::Menu {
                title: text(menu.title),
                items: items.as_ptr(),
                count: items.len(),
            })
            .collect();
        self.call(|c, w, id| {
            // SAFETY: 两级数组与所有字符串在同步调用期间存活，运行库复制后持有。
            unsafe { (c.api.menus)(c.context.as_ptr(), w, id, menus.as_ptr(), menus.len()) }
        })
    }
    /// 读取共享运行库中的列表/标签页选择；没有选择返回 None，错误控件类型返回 Err。
    pub fn selection(&self) -> Result<Option<u64>> {
        let mut value = 0;
        self.call(|c, w, id| {
            // SAFETY: 输出独占，call 校验窗口存活。
            unsafe { (c.api.selection)(c.context.as_ptr(), w, id, &mut value) }
        })?;
        Ok((value != 0).then_some(value))
    }
    /// 设置已有条目的选择；None 清除列表选择，不触发业务动作或复制第二份选择状态。
    pub fn select(&self, value: Option<u64>) -> Result<()> {
        self.call(|c, w, id| {
            // SAFETY: 句柄由同一连接创建，运行库验证条目标识。
            unsafe { (c.api.select)(c.context.as_ptr(), w, id, value.unwrap_or(0)) }
        })
    }
    /// 设置列表列元数据；列点击以 Column 事件返回，排序和数据读取仍由业务模型决定。
    pub fn set_columns(&self, values: &[Column<'_>]) -> Result<()> {
        let columns: Vec<_> = values
            .iter()
            .map(|column| abi::Column {
                id: column.id,
                title: text(column.title),
                width: column.width,
            })
            .collect();
        self.call(|c, w, id| {
            // SAFETY: 所有列和标题在同步调用中有效，运行库完整复制后提交。
            unsafe { (c.api.columns)(c.context.as_ptr(), w, id, columns.as_ptr(), columns.len()) }
        })
    }
    /// 原子替换列表数据，标识非零且唯一；仅可见行参与绘制。
    pub fn set_rows(&self, values: &[(u64, &str)]) -> Result<()> {
        let rows: Vec<_> = values
            .iter()
            .map(|(id, value)| abi::Row {
                id: *id,
                text: text(value),
            })
            .collect();
        self.call(|c, w, id| {
            // SAFETY: 数组与引用的原始字符串在同步调用内存活，运行库复制后返回。
            unsafe { (c.api.rows)(c.context.as_ptr(), w, id, rows.as_ptr(), rows.len()) }
        })
    }
    /// 标签/链接改用某个系统字体用途派生的字体；`points`、`weight` 为 None 时沿用该用途的值。
    pub fn set_font(&self, role: FontRole, points: Option<u16>, weight: Option<u16>) -> Result<()> {
        self.call(|c, w, id| {
            // SAFETY: 句柄有效，参数为整数，运行库验证范围后保存描述。
            unsafe {
                (c.api.set_font)(
                    c.context.as_ptr(),
                    w,
                    id,
                    role as u32,
                    u32::from(points.unwrap_or(0)),
                    u32::from(weight.unwrap_or(0)),
                )
            }
        })
    }
    /// 状态栏分格宽度：正数固定宽，至多一格 `-1` 占满剩余；空切片恢复按文字分格。
    /// 格图标用 [`Widget::set_row_icons`]，`RowIcon.id` 为从 1 起的格序号。
    pub fn set_status_parts(&self, parts: &[i32]) -> Result<()> {
        self.call(|c, w, id| {
            // SAFETY: 宽度数组在同步调用期间存活，运行库校验后复制。
            unsafe { (c.api.status_parts)(c.context.as_ptr(), w, id, parts.as_ptr(), parts.len()) }
        })
    }
    /// Column/Row 背景：`rgb`（0xRRGGBB）填底，`bitmap`（系统界面位图名）不缩放、不平铺地画在左上角。
    pub fn set_background(&self, rgb: Option<u32>, bitmap: Option<&str>) -> Result<()> {
        let name = text(bitmap.unwrap_or(""));
        self.call(|c, w, id| {
            // SAFETY: 名称在同步调用期间有效，运行库复制后加载位图。
            unsafe {
                (c.api.background)(
                    c.context.as_ptr(),
                    w,
                    id,
                    rgb.unwrap_or(abi::NO_COLOR),
                    name,
                )
            }
        })
    }
    /// ReBar 品牌 logo 的忙碌动画：导航或读取期间开启，完成后关闭。
    pub fn set_busy(&self, busy: bool) -> Result<()> {
        self.call(|c, w, id| {
            // SAFETY: 句柄有效，参数为整数，调用不保留任何借用。
            unsafe { (c.api.rebar_busy)(c.context.as_ptr(), w, id, u32::from(busy)) }
        })
    }
    /// 原子替换工具栏按钮；悬停、按下、复选切换与下拉锚点由共享运行库持有。
    pub fn set_toolbar(&self, buttons: &[ToolbarButton<'_>]) -> Result<()> {
        use abi::toolbar as style;
        let items: Vec<_> = buttons
            .iter()
            .map(|button| abi::ToolbarItem {
                id: button.id,
                strip: text(button.strip),
                frame: button.frame,
                text: text(button.text),
                style: match button.kind {
                    ToolbarKind::Button => 0,
                    ToolbarKind::Check => style::CHECK,
                    ToolbarKind::Dropdown => style::DROPDOWN,
                    ToolbarKind::WholeDropdown => style::WHOLE_DROPDOWN,
                    ToolbarKind::Separator => style::SEPARATOR,
                } | if button.show_text {
                    style::SHOW_TEXT
                } else {
                    0
                },
                state: if button.enabled { 0 } else { style::DISABLED }
                    | if button.checked { style::CHECKED } else { 0 },
            })
            .collect();
        self.call(|c, w, id| {
            // SAFETY: 数组与文本仅借用本次同步调用；运行库验证后复制，失败不发布部分数据。
            unsafe { (c.api.toolbar_items)(c.context.as_ptr(), w, id, items.as_ptr(), items.len()) }
        })
    }
    /// 原子替换树视图数据，展开、选择、命中和滚动由共享运行库持有。
    pub fn set_tree_rows(&self, values: &[TreeRow<'_>]) -> Result<()> {
        let rows: Vec<_> = values
            .iter()
            .map(|row| abi::TreeRow {
                id: row.id,
                parent: row.parent,
                text: text(row.text),
                state: u32::from(row.expanded) | (u32::from(row.has_children) << 1),
            })
            .collect();
        self.call(|c, w, id| {
            // SAFETY: 数组与文本仅借用本次同步调用；运行库验证后复制，失败不发布部分数据。
            unsafe { (c.api.tree_rows)(c.context.as_ptr(), w, id, rows.as_ptr(), rows.len()) }
        })
    }
    /// 向编辑控件发送语义命令；粘贴由统一 FD 循环异步推进，失败通过 ClipboardError 通知。
    pub fn edit(&self, command: EditCommand<'_>) -> Result<()> {
        let (command, value) = match command {
            EditCommand::Cut => (1, ""),
            EditCommand::Copy => (2, ""),
            EditCommand::Paste => (3, ""),
            EditCommand::Delete => (4, ""),
            EditCommand::SelectAll => (5, ""),
            EditCommand::Insert(text) => (6, text),
        };
        self.call(|c, w, id| {
            // SAFETY: 句柄有效，文本仅借用同步调用，运行库复制或立即应用。
            unsafe { (c.api.edit_command)(c.context.as_ptr(), w, id, command, text(value)) }
        })
    }
    /// 设置公历日期；不属于 Calendar 或日期非法返回错误，不发送业务 Changed。
    pub fn set_date(&self, value: abi::Date) -> Result<()> {
        self.call(|c, w, id| {
            // SAFETY: 日期以值传递，运行库验证范围与控件类型。
            unsafe { (c.api.date)(c.context.as_ptr(), w, id, value) }
        })
    }
    /// 查询 Calendar 当前选定的公历日期。
    pub fn date(&self) -> Result<abi::Date> {
        let mut date = abi::Date::default();
        self.call(|c, w, id| {
            // SAFETY: 输出独占，控件与 context 在同步调用内有效。
            unsafe { (c.api.read_date)(c.context.as_ptr(), w, id, &mut date) }
        })?;
        Ok(date)
    }
    /// 原子替换行图标；像素由共享资源上下文缓存，数据刷新后须重新指定新行的资源。
    pub fn set_row_icons(&self, values: &[RowIcon<'_>]) -> Result<()> {
        let icons: Vec<_> = values
            .iter()
            .map(|icon| {
                let (kind, value) = match icon.source {
                    Icon::Resource(value) => (0, value),
                    Icon::Application(value) => (1, value),
                    Icon::Theme(value) => (2, value),
                };
                abi::RowIcon {
                    id: icon.id,
                    kind,
                    name: text(value),
                }
            })
            .collect();
        self.call(|c, w, id| {
            // SAFETY: 数组和字符串在同步调用内有效，运行库只保留自己的资源引用。
            unsafe { (c.api.row_icons)(c.context.as_ptr(), w, id, icons.as_ptr(), icons.len()) }
        })
    }
    /// 设置列表显示方式与选择策略；视图切换重置滚动，多选由共享运行库管理。
    pub fn set_list_style(&self, style: ListStyle, options: ListOptions) -> Result<()> {
        let mut flags = 0;
        if options.multiple {
            flags |= abi::list::MULTIPLE;
        }
        if options.full_row_select {
            flags |= abi::list::FULL_ROW;
        }
        self.call(|c, w, id| {
            // SAFETY: 枚举映射公开整数标签，运行库验证控件与标志位。
            unsafe { (c.api.list_style)(c.context.as_ptr(), w, id, style as u32, flags) }
        })
    }
    /// 查询实际选择集合，与键盘焦点条目分开；不在应用维护多选镜像。
    pub fn selected_rows(&self) -> Result<Vec<u64>> {
        let mut count = 0;
        self.call(|c, w, id| {
            // SAFETY: 零容量查询只写元素数。
            let status = unsafe {
                (c.api.read_selection)(
                    c.context.as_ptr(),
                    w,
                    id,
                    std::ptr::null_mut(),
                    0,
                    &mut count,
                )
            };
            if status == abi::TOO_SMALL {
                abi::OK
            } else {
                status
            }
        })?;
        if count > 1_000_000 {
            return Err(Error {
                code: abi::INVALID,
                message: "selection count exceeds ABI limit".into(),
            });
        }
        let mut values = vec![0; count];
        self.call(|c, w, id| {
            // SAFETY: 向本连接独占提供完整 u64 数组；两次查询之间不派发事件。
            unsafe {
                (c.api.read_selection)(
                    c.context.as_ptr(),
                    w,
                    id,
                    values.as_mut_ptr(),
                    values.len(),
                    &mut count,
                )
            }
        })?;
        values.truncate(count);
        Ok(values)
    }
    /// 设置选择集合；未知条目、多选未开启或窗口关闭时返回错误。
    pub fn select_rows(&self, values: &[u64]) -> Result<()> {
        self.call(|c, w, id| {
            // SAFETY: 数组仅借用同步调用，运行库先验证再复制。
            unsafe {
                (c.api.write_selection)(c.context.as_ptr(), w, id, values.as_ptr(), values.len())
            }
        })
    }
    /// 设置已有列的正文对齐，不改变列宽/排序和表头交互。
    pub fn set_column_alignment(&self, column: u64, alignment: ColumnAlignment) -> Result<()> {
        self.call(|c, w, id| {
            // SAFETY: 标识和值只在运行库解释，不暴露内部表头对象。
            unsafe { (c.api.column_alignment)(c.context.as_ptr(), w, id, column, alignment as u32) }
        })
    }
    /// 查询当前指针命中的条目，空白为 None；不需要在应用复制指针或命中几何。
    pub fn pointed_row(&self) -> Result<Option<u64>> {
        let mut value = 0;
        self.call(|c, w, id| {
            // SAFETY: 输出独占，运行库查询同一份布局和指针。
            unsafe { (c.api.pointed_row)(c.context.as_ptr(), w, id, &mut value) }
        })?;
        Ok((value != 0).then_some(value))
    }
    /// 为已有行开始原地重命名，初始值由业务提供；提交产生 LabelEdited，草稿通过 text 读取。
    pub fn edit_label(&self, row: u64, value: &str) -> Result<()> {
        self.call(|c, w, id| {
            // SAFETY: 初始文本在同步调用内有效，运行库复制后管理草稿与焦点。
            unsafe { (c.api.edit_label)(c.context.as_ptr(), w, id, row, 0, text(value)) }
        })
    }
    /// 完成后台重命名结果；accepted=false 保留用户输入并恢复编辑，true 回收草稿。
    pub fn finish_label_edit(&self, accepted: bool) -> Result<()> {
        self.call(|c, w, id| {
            // SAFETY: 仅变更当前控件拥有的编辑阶段，不访问已返回的字符串。
            unsafe {
                (c.api.edit_label)(
                    c.context.as_ptr(),
                    w,
                    id,
                    0,
                    if accepted { 1 } else { 2 },
                    text(""),
                )
            }
        })
    }
    /// 读取列表当前查看方式，未知的新标签返回错误。
    pub fn list_style(&self) -> Result<ListStyle> {
        let mut mode = 0;
        self.call(|c, w, id| {
            // SAFETY: 输出独占，运行库只复制当前视图标签。
            unsafe { (c.api.read_list_style)(c.context.as_ptr(), w, id, &mut mode) }
        })?;
        match mode {
            0 => Ok(ListStyle::Details),
            1 => Ok(ListStyle::LargeIcons),
            2 => Ok(ListStyle::SmallIcons),
            3 => Ok(ListStyle::List),
            _ => Err(Error::invalid("unknown list style")),
        }
    }
    /// 将键盘焦点交给本控件；隐藏、禁用或非活动标签页目标返回错误。
    pub fn focus(&self) -> Result<()> {
        self.call(|c, w, id| {
            // SAFETY: 句柄由同一窗口创建，不转移 Rust 对象或借用。
            unsafe { (c.api.focus)(c.context.as_ptr(), w, id) }
        })
    }
    /// 当前逻辑矩形；必要时更新排版，不提交像素。未收到 configure 时为零矩形。
    pub fn geometry(&self) -> Result<Rect> {
        let mut rect = Rect::default();
        self.call(|c, w, id| {
            // SAFETY: 输出为调用者独占，运行库只复制标量，不执行应用代码。
            unsafe { (c.api.geometry)(c.context.as_ptr(), w, id, &mut rect) }
        })?;
        Ok(rect)
    }
    /// 查询当前主题的 0xRRGGBB 语义色；不要求应用读取主题文件。
    pub fn color(&self, role: ColorRole) -> Result<u32> {
        let mut color = 0;
        self.call(|c, _, _| {
            // SAFETY: 角色以稳定整数表示，输出独占，仅复制资源值。
            unsafe { (c.api.color)(c.context.as_ptr(), role as u32, &mut color) }
        })?;
        Ok(color)
    }
    /// 替换 Canvas 的媒体帧：`width`×`height` 个设备像素的 0x00RRGGBB（行优先），运行库复制后 1:1 绘在显示列表之下。
    /// 帧尺寸应等于 [`Widget::geometry`] 的逻辑矩形乘以窗口 scale；`(0, 0, &[])` 清除帧。
    pub fn set_frame(&self, width: u32, height: u32, pixels: &[u32]) -> Result<()> {
        self.call(|c, w, id| {
            // SAFETY: 像素切片在同步调用内有效，运行库只读并复制。
            unsafe {
                (c.api.canvas_frame)(
                    c.context.as_ptr(),
                    w,
                    id,
                    width,
                    height,
                    pixels.as_ptr(),
                    pixels.len(),
                )
            }
        })
    }
    /// 原子替换 Canvas 显示列表；不会把 Rust Painter 或像素分配器跨库传递。
    pub fn set_drawing(&self, values: &[Draw<'_>]) -> Result<()> {
        let commands: Vec<_> = values
            .iter()
            .map(|v| abi::Draw {
                kind: v.kind,
                rect: v.rect,
                color: v.color,
                bold: u32::from(v.bold),
                text: text(v.text),
            })
            .collect();
        self.call(|c, w, id| {
            // SAFETY: 命令和文本保持有效到同步调用结束，运行库拥有复制内容。
            unsafe { (c.api.draw)(c.context.as_ptr(), w, id, commands.as_ptr(), commands.len()) }
        })
    }
}
