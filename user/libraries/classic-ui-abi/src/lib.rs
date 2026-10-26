//! 共享控件运行库第一版二进制契约：只包含目标 C ABI 可表达的布局。
//!
//! 所有调用限定创建 context 的线程；所有借用字符串在调用返回前复制。
//! context 由 create 返回并由 destroy 消费一次；其他函数只借用它。
//! 输出指针必须可写且不与任何输入重叠，输入指针必须在整次调用中有效。
//! 未知 kind/state/event 标签必须返回错误或被旧 SDK 跳过，不转成无效 Rust enum。
#![no_std]

use core::ffi::c_void;

/// ABI 主版本；同版本只允许追加表尾与保留旧语义的扩展。
pub const ABI_VERSION: u32 = 1;
/// 调用成功。
pub const OK: i32 = 0;
/// 无事件或缓冲暂不可写。
pub const PENDING: i32 = 1;
/// 用户取消对话框。
pub const CANCELLED: i32 = 2;
/// 参数或 UTF-8 不合法。
pub const INVALID: i32 = -1;
/// 对象已经销毁或不属于 context。
pub const NOT_FOUND: i32 = -2;
/// 连接、资源或系统操作失败。
pub const FAILED: i32 = -3;
/// 输出缓冲太小；长度输出包含所需字节数。
pub const TOO_SMALL: i32 = -4;

/// 调用期间借用的 UTF-8 字节；len=0 允许 data=null。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Text {
    pub data: *const u8,
    pub len: usize,
}

/// 借用的原始字节；文件路径遵循 Linux 原生字节语义，不强制转换 UTF-8。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Bytes {
    pub data: *const u8,
    pub len: usize,
}

/// 96 DPI 逻辑像素矩形，采用左闭右开坐标。
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Rect {
    /// 从逻辑位置和大小建立矩形；非正宽高可表达空区域。
    pub const fn new(x: i32, y: i32, width: i32, height: i32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
    /// 右侧的开边界，溢出时饱和。
    pub const fn right(self) -> i32 {
        self.x.saturating_add(self.width)
    }
    /// 下侧的开边界，溢出时饱和。
    pub const fn bottom(self) -> i32 {
        self.y.saturating_add(self.height)
    }
}

/// 行列布局规则。width/height=-1 表示按内容；grow 按剩余空间分配。
/// padding/gap/min_width/min_height 均为非负逻辑像素，溢出由父节点裁剪。
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub width: i32,
    pub height: i32,
    pub grow: u32,
    pub min_width: i32,
    pub min_height: i32,
    pub padding: i32,
    pub gap: i32,
}
impl Default for Layout {
    fn default() -> Self {
        Self {
            width: -1,
            height: -1,
            grow: 0,
            min_width: 0,
            min_height: 0,
            padding: 0,
            gap: 4,
        }
    }
}

/// 公共控件标签；只允许此处声明的值创建对象。
pub mod widget {
    pub const COLUMN: u32 = 1;
    pub const ROW: u32 = 2;
    pub const LABEL: u32 = 3;
    pub const BUTTON: u32 = 4;
    pub const EDIT: u32 = 5;
    pub const CHECKBOX: u32 = 6;
    pub const RADIO: u32 = 7;
    pub const LIST: u32 = 8;
    pub const TABS: u32 = 9;
    pub const STATUS: u32 = 10;
    pub const CANVAS: u32 = 11;
    pub const SCROLLBAR: u32 = 12;
    pub const SLIDER: u32 = 13;
    pub const MENU_BAR: u32 = 14;
    pub const TREE: u32 = 15;
    pub const TEXT_EDITOR: u32 = 16;
    pub const IMAGE: u32 = 17;
    pub const GROUP: u32 = 18;
    pub const CALENDAR: u32 = 19;
    pub const POPUP_MENU: u32 = 20;
}
/// 控件状态位；HIDDEN 不参与布局、命中和焦点。
pub mod state {
    pub const DISABLED: u32 = 1;
    pub const CHECKED: u32 = 2;
    pub const HIDDEN: u32 = 4;
    pub const WRAP: u32 = 8;
    pub const PASSWORD: u32 = 16;
    pub const DEFAULT: u32 = 32;
    /// 自绘控件接收 Tab 字符而不进行焦点遍历；终端必须将它交给 PTY 完成命令补全。
    pub const TAB_INPUT: u32 = 64;
    /// 单行标签在布局矩形内右对齐（统计数值列）；超宽时与左对齐一样截断。
    pub const ALIGN_RIGHT: u32 = 128;
}
/// 运行库产生的事件标签，未知标签由 SDK 跳过。
pub mod event {
    pub const CLOSE: u32 = 1;
    pub const ACTIVATE: u32 = 2;
    pub const CHANGED: u32 = 3;
    pub const SELECT: u32 = 4;
    pub const KEY: u32 = 5;
    pub const POINTER: u32 = 6;
    pub const CONFIGURE: u32 = 7;
    pub const FOCUS: u32 = 8;
    pub const FD_READY: u32 = 9;
    pub const BUTTON: u32 = 10;
    pub const WHEEL: u32 = 11;
    pub const DIALOG_DONE: u32 = 12;
    pub const COLUMN: u32 = 13;
    pub const SHEET_VALIDATE: u32 = 14;
    pub const SHEET_APPLY: u32 = 15;
    pub const CLIPBOARD_ERROR: u32 = 16;
    pub const TOPLEVELS_CHANGED: u32 = 17;
    pub const DISMISS: u32 = 18;
    pub const LABEL_EDITED: u32 = 19;
    pub const EXPAND: u32 = 20;
}

/// 事件中的 value 用于选择行标识；text 是一个已提交 Unicode scalar，0 表示没有文本。
/// Key 的 code 是 evdev 键码，flags bit0=pressed bit1=repeat；modifiers=Shift/Ctrl/Alt/Super 四位。
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Event {
    pub window: u64,
    pub node: u64,
    pub kind: u32,
    pub code: u32,
    pub flags: u32,
    pub modifiers: u32,
    pub value: u64,
    pub text: u32,
    pub width: i32,
    pub height: i32,
    pub scale: i32,
    pub x: f64,
    pub y: f64,
}

/// 共享运行库中的控件规格；radio 同 parent 下 group 相同的节点互斥。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Widget {
    pub kind: u32,
    pub state: u32,
    pub group: u64,
    pub layout: Layout,
    pub text: Text,
}

/// 自绘批次：kind=1 填充，2 矩形框，3 文本，4 换行文本、5 等宽文字（8×16 逻辑单元格）；颜色为 0x00RRGGBB。
/// 文本与绘图命令在调用期间复制，不包含裸像素或 Rust 对象。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Draw {
    pub kind: u32,
    pub rect: Rect,
    pub color: u32,
    pub bold: u32,
    pub text: Text,
}

/// 一个列表行；标识非零且在当前列表唯一，列文本用 UTF-8 TAB 分隔。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Row {
    pub id: u64,
    pub text: Text,
}

/// 树节点，父节点须在数组中先出现；state bit 0 指定首次出现时是否展开，bit 1 表示尚未载入的子节点。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct TreeRow {
    pub id: u64,
    pub parent: u64,
    pub text: Text,
    pub state: u32,
}

/// 通用消息对话框结果；buttons 的非零节点按传入按钮组顺序排列。
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct MessageBox {
    pub window: u64,
    pub buttons: [u64; 3],
}

/// 公历日期；合法年份 1..=9999、月份 1..=12、日期须属于该月。
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Date {
    pub year: i32,
    pub month: u32,
    pub day: u32,
}
impl Default for Date {
    fn default() -> Self {
        Self {
            year: 1970,
            month: 1,
            day: 1,
        }
    }
}
/// 列表行图标；kind=0 系统素材名、1 已安装 desktop entry 的 app_id。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RowIcon {
    pub id: u64,
    pub kind: u32,
    pub name: Text,
}

/// 滚动条/滑块范围；maximum 包含端点，page/step 至少为 1，vertical 只能为 0 或 1。
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Range {
    pub minimum: i32,
    pub maximum: i32,
    pub value: i32,
    pub page: i32,
    pub step: i32,
    pub vertical: u32,
}
impl Default for Range {
    fn default() -> Self {
        Self {
            minimum: 0,
            maximum: 100,
            value: 0,
            page: 1,
            step: 1,
            vertical: 0,
        }
    }
}

/// 菜单条目，id 非零；state bit0=禁用，bit1=勾选，bit2=上方分隔线。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct MenuItem {
    pub id: u64,
    pub label: Text,
    pub shortcut: Text,
    pub state: u32,
}
/// 顶层菜单；条目和所有文本仅在 set_menus 调用期间借用。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Menu {
    pub title: Text,
    pub items: *const MenuItem,
    pub count: usize,
}

/// 列表列，id 非零，width 为初始逻辑宽度；sort=0 无标记、1 升序、2 降序。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Column {
    pub id: u64,
    pub title: Text,
    pub width: i32,
    pub sort: u32,
}

/// 属性表的窗口与页面容器标识，其余内置按钮仅由运行库拥有。
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Sheet {
    pub window: u64,
    pub pages: u64,
}

/// 文件类型筛选；extensions 是不含前导点的扩展名（如 wav、tar.gz），空列表显示全部文件。
/// 本结构和所有文本仅借用同步创建调用，运行库复制内容；手工输入路径不受显示筛选限制。
#[repr(C)]
pub struct FileFilter {
    pub label: Text,
    pub extensions: *const Text,
    pub count: usize,
}

/// 稳定函数表。前两个字段固定，新增能力只能追加到表尾。
///
/// # Safety
/// 调用方遵守模块级指针/线程契约；返回的句柄不可伪造，destroy 后不能再次使用。
/// read_text/error_text 允许 buffer=null 且 capacity=0 进行长度查询，written 始终非空。
/// step 的 wake_fd=-1 表示没有外部唤醒，否则借用一个有效可读 fd，运行库不关闭它。
#[repr(C)]
pub struct Api {
    pub version: u32,
    pub size: u32,
    pub create: unsafe extern "C" fn(*mut *mut c_void) -> i32,
    pub destroy: unsafe extern "C" fn(*mut c_void),
    pub step: unsafe extern "C" fn(*mut c_void, i32, i32, *mut Event) -> i32,
    pub window: unsafe extern "C" fn(*mut c_void, Text, Text, i32, i32, u32, u64, *mut u64) -> i32,
    pub close: unsafe extern "C" fn(*mut c_void, u64) -> i32,
    pub title: unsafe extern "C" fn(*mut c_void, u64, Text) -> i32,
    pub widget: unsafe extern "C" fn(*mut c_void, u64, u64, *const Widget, *mut u64) -> i32,
    pub remove: unsafe extern "C" fn(*mut c_void, u64, u64) -> i32,
    pub text: unsafe extern "C" fn(*mut c_void, u64, u64, Text) -> i32,
    pub read_text: unsafe extern "C" fn(*mut c_void, u64, u64, *mut u8, usize, *mut usize) -> i32,
    pub state: unsafe extern "C" fn(*mut c_void, u64, u64, u32) -> i32,
    pub layout: unsafe extern "C" fn(*mut c_void, u64, u64, Layout) -> i32,
    pub rows: unsafe extern "C" fn(*mut c_void, u64, u64, *const Row, usize) -> i32,
    pub draw: unsafe extern "C" fn(*mut c_void, u64, u64, *const Draw, usize) -> i32,
    pub watch: unsafe extern "C" fn(*mut c_void, i32, *mut u64) -> i32,
    pub arm: unsafe extern "C" fn(*mut c_void, u64, u32) -> i32,
    pub unwatch: unsafe extern "C" fn(*mut c_void, u64) -> i32,
    pub clipboard_get: unsafe extern "C" fn(*mut c_void, *mut i32) -> i32,
    pub clipboard_set: unsafe extern "C" fn(*mut c_void, Text) -> i32,
    pub range: unsafe extern "C" fn(*mut c_void, u64, u64, Range) -> i32,
    pub read_range: unsafe extern "C" fn(*mut c_void, u64, u64, *mut Range) -> i32,
    pub file_dialog: unsafe extern "C" fn(*mut c_void, u64, u32, Bytes, Text, *mut u64) -> i32,
    pub dialog_result: unsafe extern "C" fn(*mut c_void, u64, *mut u8, usize, *mut usize) -> i32,
    pub menus: unsafe extern "C" fn(*mut c_void, u64, u64, *const Menu, usize) -> i32,
    pub columns: unsafe extern "C" fn(*mut c_void, u64, u64, *const Column, usize) -> i32,
    pub sheet: unsafe extern "C" fn(*mut c_void, u64, Text, i32, i32, *mut Sheet) -> i32,
    pub sheet_dirty: unsafe extern "C" fn(*mut c_void, u64, u32) -> i32,
    pub sheet_validated: unsafe extern "C" fn(*mut c_void, u64, u64, u32, u64, u64, Text) -> i32,
    pub sheet_applied: unsafe extern "C" fn(*mut c_void, u64, u64, u32, Text) -> i32,
    pub error_text: unsafe extern "C" fn(*mut c_void, *mut u8, usize, *mut usize) -> i32,
    /// 读取当前控件矩形，必要时更新排版；未 configure 返回零矩形，不提交像素。
    pub geometry: unsafe extern "C" fn(*mut c_void, u64, u64, *mut Rect) -> i32,
    /// foreign-toplevel 完整快照的应用局部标识；written 是元素数而非字节数。
    pub toplevel_ids: unsafe extern "C" fn(*mut c_void, *mut u64, usize, *mut usize) -> i32,
    /// field=0 标题、1 app_id；输出遵守 read_text 的 UTF-8 复制契约。
    pub toplevel_text:
        unsafe extern "C" fn(*mut c_void, u64, u32, *mut u8, usize, *mut usize) -> i32,
    /// state bits：最大化=1、最小化=2、活动=4、全屏=8。
    pub toplevel_state: unsafe extern "C" fn(*mut c_void, u64, *mut u32) -> i32,
    /// action=0 激活、1 关闭、2 最小化、3 还原、4 最大化、5 取消最大化。
    pub toplevel_action: unsafe extern "C" fn(*mut c_void, u64, u32) -> i32,
    /// 列表/标签页当前稳定选择，零表示没有选择；不返回应用侧镜像。
    pub selection: unsafe extern "C" fn(*mut c_void, u64, u64, *mut u64) -> i32,
    /// 设置列表/标签页选择；零清除列表选择，非零必须是已有条目。
    pub select: unsafe extern "C" fn(*mut c_void, u64, u64, u64) -> i32,
    /// 原子替换树数据；节点标识唯一、父先子后，旧节点保留展开状态。
    pub tree_rows: unsafe extern "C" fn(*mut c_void, u64, u64, *const TreeRow, usize) -> i32,
    /// 编辑命令：1 剪切、2 复制、3 异步粘贴、4 删除、5 全选、6 插入 UTF-8。
    pub edit_command: unsafe extern "C" fn(*mut c_void, u64, u64, u32, Text) -> i32,
    /// 标准消息对话框；buttons=0 确定、1 确定/取消、2 是/否/取消、3 是/否。
    pub message_box:
        unsafe extern "C" fn(*mut c_void, u64, Text, Text, u32, *mut MessageBox) -> i32,
    /// 设置/读取 Calendar；日期按公历验证。
    pub date: unsafe extern "C" fn(*mut c_void, u64, u64, Date) -> i32,
    pub read_date: unsafe extern "C" fn(*mut c_void, u64, u64, *mut Date) -> i32,
    /// 原子设置当前行图标，未知行或类型返回错误；图像像素始终由运行库拥有。
    pub row_icons: unsafe extern "C" fn(*mut c_void, u64, u64, *const RowIcon, usize) -> i32,

    /// 读取控件当前状态位；草稿仍由运行库拥有。
    pub read_state: unsafe extern "C" fn(*mut c_void, u64, u64, *mut u32) -> i32,

    /// 查询当前主题的语义颜色；0 窗口文字、1 窗口、2 控件面、3 桌面、4 高亮、5 高亮文字、6 禁用文字。
    pub color: unsafe extern "C" fn(*mut c_void, u32, *mut u32) -> i32,
    /// 更新标准 xdg_toplevel app_id 与对应程序图标。
    pub app_id: unsafe extern "C" fn(*mut c_void, u64, Text) -> i32,
    /// mode=0 详细、1 大图标、2 小图标、3 列表；multiple 为 0/1。
    pub list_style: unsafe extern "C" fn(*mut c_void, u64, u64, u32, u32) -> i32,
    /// 实际多选集合，以元素数协商输出容量；单选返回零或一个条目。
    pub read_selection:
        unsafe extern "C" fn(*mut c_void, u64, u64, *mut u64, usize, *mut usize) -> i32,
    pub write_selection: unsafe extern "C" fn(*mut c_void, u64, u64, *const u64, usize) -> i32,
    /// 列正文对齐：0 左、1 中、2 右。
    pub column_alignment: unsafe extern "C" fn(*mut c_void, u64, u64, u64, u32) -> i32,
    /// 由同一控件几何返回指针当前命中的稳定行 ID，零表示空白。
    pub pointed_row: unsafe extern "C" fn(*mut c_void, u64, u64, *mut u64) -> i32,

    /// 在当前指针处显示 PopupMenu；位置由同一 Tree 的输入状态拥有。
    pub popup: unsafe extern "C" fn(*mut c_void, u64, u64) -> i32,
    /// 列表原地重命名：action=0 开始，1 接受，2 恢复编辑；开始时复制 text，结果可通过 read_text 查询。
    pub edit_label: unsafe extern "C" fn(*mut c_void, u64, u64, u64, u32, Text) -> i32,
    /// 读取列表查看方式，不复制为应用可写状态。
    pub read_list_style: unsafe extern "C" fn(*mut c_void, u64, u64, *mut u32) -> i32,
    /// 聚焦可见、可用的控件，可在首次 configure 前登记意图。
    pub focus: unsafe extern "C" fn(*mut c_void, u64, u64) -> i32,
    /// 带文件类型筛选的共享对话框；filter 非空，只借用本次调用。
    pub file_dialog_filtered: unsafe extern "C" fn(
        *mut c_void,
        u64,
        u32,
        Bytes,
        Text,
        *const FileFilter,
        *mut u64,
    ) -> i32,
}
