//! Windows 外壳的映射/断开流程；耗时认证和挂载只在 Background 执行。
use crate::Result;
use liteos_ui::{
    Background, Dialogs, EventKind, Layout, ListOptions, ListStyle, MessageButtons, TreeRow,
    Widget, WidgetKind as Kind, WidgetSpec, Window, WindowOptions, state,
};
use network_drive::{Credentials, Mapping, Store};

/// 文字取自 Windows 2000 简中 netplwiz.dll 对话框 106（映射向导）、124（连接身份）与 mprui.dll 对话框 7002（断开），
/// 其中 `&` 标出快捷键、`<A>` 标出链接范围。
mod text {
    pub const MAP_TITLE: &str = "映射网络驱动器";
    pub const MAP_INTRO: &str = "Windows 可以帮助您连接到共享的网络文件夹，并给该连接指派一个盘符，这样您就可以从“我的电脑”访问该文件夹。";
    pub const MAP_PROMPT: &str = "请为要连接到的连接和文件夹指定盘符:";
    pub const DRIVE: &str = "驱动器(&D):";
    pub const FOLDER: &str = "文件夹(&O):";
    pub const BROWSE: &str = "浏览(&B)...";
    pub const EXAMPLE: &str = "例如: \\\\server\\share";
    pub const RECONNECT: &str = "登录时重新连接(&R)";
    pub const OTHER_USER: &str = "使用<A>其他用户名</A>进行连接。";
    pub const CONNECT_AS_TITLE: &str = "连接身份...";
    pub const CONNECT_AS_PROMPT: &str = "默认情况下，您会连接到 <\\\\server\\share>，身份为 <domain\\user>。要以其他用户身份连接，请在下面输入其用户名和密码。";
    pub const USERNAME: &str = "用户名(&U):";
    pub const PASSWORD: &str = "密码(&P):";
    pub const DISCONNECT_PROMPT: &str = "请选择您要断开的驱动器:";
    pub const DISCONNECT_TITLE: &str = "断开网络驱动器";
    pub const OK: &str = "确定";
    pub const CANCEL: &str = "取消";
}

/// 对话框模板的逻辑像素：Windows 2000 的 DLGTEMPLATE 以 宋体 9 磅的对话框单位给出，1 单位 = 1.5 像素。
const BUTTON_WIDTH: i32 = 75;
/// 映射向导页：模板 294×154 单位，侧边图 88 单位宽；向导按钮条在页面下方。
const PAGE: (i32, i32) = (441, 231);
const BANNER_WIDTH: i32 = 132;
/// 控件左缘相对侧边图的缩进（模板 x=100 对应 150 像素）与标签列宽（驱动器/文件夹到控件的 66 像素）。
const INDENT: i32 = 18;
const CAPTION: i32 = 66;

/// 对话框持有全部控件：`Widget` 析构会删除运行库里的节点，丢弃占位/标签/禁用按钮的句柄会让它们消失。
#[derive(Default)]
struct Held(std::cell::RefCell<Vec<Widget>>);
fn node(
    held: &Held,
    parent: &Widget,
    kind: Kind,
    text: &str,
    width: i32,
    height: i32,
    grow: u32,
) -> liteos_ui::Result<Widget> {
    let mut spec = WidgetSpec::new(kind, text);
    spec.layout = Layout {
        width,
        height,
        grow,
        gap: 0,
        ..Default::default()
    };
    let widget = parent.child(spec)?;
    held.0.borrow_mut().push(widget.clone());
    Ok(widget)
}
fn spacer(held: &Held, parent: &Widget, width: i32, height: i32) -> liteos_ui::Result<Widget> {
    node(held, parent, Kind::Label, "", width, height, 0)
}
/// 固定高度的一行，所有子控件由调用方按模板坐标排布。
fn row(held: &Held, parent: &Widget, height: i32) -> liteos_ui::Result<Widget> {
    node(held, parent, Kind::Row, "", -1, height, 0)
}
/// 单行标签：模板中标签文字基线比同行输入控件低 5 像素，用顶部留白还原。
fn caption(held: &Held, row: &Widget, text: &str, width: i32) -> liteos_ui::Result<Widget> {
    let column = node(held, row, Kind::Column, "", width, -1, 0)?;
    spacer(held, &column, -1, 5)?;
    node(held, &column, Kind::Label, text, -1, 16, 0)
}
/// 拆分资源文本中的 `<A>链接</A>` 标记；缺少标记时整段都是前置文字。
fn anchor(text: &str) -> (&str, &str, &str) {
    match (text.find("<A>"), text.find("</A>")) {
        (Some(start), Some(end)) if start < end => {
            (&text[..start], &text[start + 3..end], &text[end + 4..])
        }
        _ => (text, "", ""),
    }
}
async fn notify(dialogs: &Dialogs, parent: &Window, title: &str, message: &str) -> Result<()> {
    dialogs
        .message(parent, title, message, MessageButtons::Ok)?
        .await?;
    Ok(())
}

/// “连接身份...”输入；`user` 可写成 `域\用户`。
struct Identity {
    user: String,
    password: String,
    remember: bool,
    encrypt: bool,
}

/// 窗口 `连接身份...`（netplwiz 对话框 124）；确定返回输入，取消返回 None。
/// “保存密码”与“要求 SMB 3 加密”是 LiteOS 的扩展项，没有 Windows 2000 对应物，排在密码行之后。
async fn connect_as(
    dialogs: &Dialogs,
    parent: &Window,
    share: &str,
    current: &Identity,
) -> Result<Option<Identity>> {
    let mut options =
        WindowOptions::new(text::CONNECT_AS_TITLE, "org.liteos.file-explorer", 318, 196);
    options.fixed = true;
    options.modal = true;
    options.parent = Some(parent);
    let (window, mut events) = dialogs.window(options)?;
    let held = Held::default();
    let mut spec = WidgetSpec::new(Kind::Column, "");
    spec.layout = Layout {
        grow: 1,
        gap: 0,
        ..Default::default()
    };
    let root = window.widget(spec)?;
    spacer(&held, &root, -1, 9)?;
    let intro = row(&held, &root, 38)?;
    spacer(&held, &intro, 10, -1)?;
    let me = std::env::var("USER").unwrap_or_default();
    let prompt = text::CONNECT_AS_PROMPT
        .replace("<\\\\server\\share>", share)
        .replace("<domain\\user>", &me);
    let label = node(&held, &intro, Kind::Label, &prompt, -1, -1, 1)?;
    label.set_state(state::WRAP)?;
    spacer(&held, &root, -1, 10)?;
    let user_row = row(&held, &root, 21)?;
    spacer(&held, &user_row, 10, -1)?;
    caption(&held, &user_row, text::USERNAME, 78)?;
    let user = node(&held, &user_row, Kind::Edit, &current.user, 137, 21, 0)?;
    spacer(&held, &user_row, 7, -1)?;
    node(
        &held,
        &user_row,
        Kind::Button,
        text::BROWSE,
        BUTTON_WIDTH,
        21,
        0,
    )?
    .set_state(state::DISABLED)?;
    spacer(&held, &root, -1, 8)?;
    let pass_row = row(&held, &root, 21)?;
    spacer(&held, &pass_row, 10, -1)?;
    caption(&held, &pass_row, text::PASSWORD, 78)?;
    let password = node(&held, &pass_row, Kind::Edit, &current.password, 137, 21, 0)?;
    password.set_state(state::PASSWORD)?;
    spacer(&held, &root, -1, 8)?;
    let remember_row = row(&held, &root, 15)?;
    spacer(&held, &remember_row, 88, -1)?;
    let remember = node(
        &held,
        &remember_row,
        Kind::CheckBox,
        "保存密码（仅文件权限保护）",
        -1,
        15,
        1,
    )?;
    remember.set_state(if current.remember { state::CHECKED } else { 0 })?;
    spacer(&held, &root, -1, 4)?;
    let encrypt_row = row(&held, &root, 15)?;
    spacer(&held, &encrypt_row, 88, -1)?;
    let encrypt = node(
        &held,
        &encrypt_row,
        Kind::CheckBox,
        "要求 SMB 3 加密",
        -1,
        15,
        1,
    )?;
    encrypt.set_state(if current.encrypt { state::CHECKED } else { 0 })?;
    spacer(&held, &root, -1, 10)?;
    let buttons = row(&held, &root, 21)?;
    spacer(&held, &buttons, -1, -1)?.set_layout(Layout {
        grow: 1,
        gap: 0,
        ..Default::default()
    })?;
    let ok = node(&held, &buttons, Kind::Button, text::OK, BUTTON_WIDTH, 21, 0)?;
    ok.set_state(state::DEFAULT)?;
    spacer(&held, &buttons, 6, -1)?;
    let cancel = node(
        &held,
        &buttons,
        Kind::Button,
        text::CANCEL,
        BUTTON_WIDTH,
        21,
        0,
    )?;
    spacer(&held, &buttons, 10, -1)?;
    user.focus()?;
    while let Some(event) = events.next().await? {
        match event.kind {
            EventKind::Close
            | EventKind::Key {
                code: 1,
                pressed: true,
                ..
            } => return Ok(None),
            EventKind::Activate { .. } if event.node == cancel.id() => return Ok(None),
            EventKind::Activate { .. } if event.node == ok.id() => {
                let checked = |widget: &Widget| -> liteos_ui::Result<bool> {
                    Ok(widget.state()? & state::CHECKED != 0)
                };
                return Ok(Some(Identity {
                    user: user.text()?,
                    password: password.text()?,
                    remember: checked(&remember)?,
                    encrypt: checked(&encrypt)?,
                }));
            }
            _ => {}
        }
    }
    Ok(None)
}

/// 窗口底部的向导按钮条：模板之外由 PropertySheet 添加，右对齐、间距 6 像素。
fn wizard_buttons(held: &Held, root: &Widget) -> liteos_ui::Result<(Widget, Widget)> {
    let line = node(held, root, Kind::Row, "", -1, 2, 0)?;
    line.set_state(state::CLIENT_EDGE)?;
    spacer(held, root, -1, 10)?;
    let buttons = row(held, root, 21)?;
    spacer(held, &buttons, -1, -1)?.set_layout(Layout {
        grow: 1,
        gap: 0,
        ..Default::default()
    })?;
    node(
        held,
        &buttons,
        Kind::Button,
        "< 上一步(&B)",
        BUTTON_WIDTH,
        21,
        0,
    )?
    .set_state(state::DISABLED)?;
    let ok = node(held, &buttons, Kind::Button, "完成", BUTTON_WIDTH, 21, 0)?;
    ok.set_state(state::DEFAULT)?;
    spacer(held, &buttons, 6, -1)?;
    let cancel = node(
        held,
        &buttons,
        Kind::Button,
        text::CANCEL,
        BUTTON_WIDTH,
        21,
        0,
    )?;
    spacer(held, &buttons, 10, -1)?;
    spacer(held, root, -1, 10)?;
    Ok((ok, cancel))
}

async fn disconnect(
    dialogs: &Dialogs,
    background: &Background,
    parent: &Window,
    store: &Store,
    records: &[Mapping],
) -> Result<()> {
    // 模板 244×118 单位：提示 (7,9)、列表 (7,22,230×65)、按钮 (133,95)/(187,95)。
    let mut options =
        WindowOptions::new(text::DISCONNECT_TITLE, "org.liteos.file-explorer", 366, 177);
    options.fixed = true;
    options.modal = true;
    options.parent = Some(parent);
    let (window, mut events) = dialogs.window(options)?;
    let held = Held::default();
    let mut spec = WidgetSpec::new(Kind::Column, "");
    spec.layout = Layout {
        grow: 1,
        gap: 0,
        ..Default::default()
    };
    let root = window.widget(spec)?;
    spacer(&held, &root, -1, 13)?;
    let prompt = row(&held, &root, 14)?;
    spacer(&held, &prompt, 10, -1)?;
    node(
        &held,
        &prompt,
        Kind::Label,
        text::DISCONNECT_PROMPT,
        -1,
        -1,
        1,
    )?;
    spacer(&held, &root, -1, 6)?;
    let list_row = row(&held, &root, 98)?;
    spacer(&held, &list_row, 10, -1)?;
    let list = node(&held, &list_row, Kind::List, "", 345, 98, 0)?;
    list.set_state(state::CLIENT_EDGE)?;
    list.set_list_style(
        ListStyle::Details,
        ListOptions {
            multiple: true,
            full_row_select: true,
        },
    )?;
    let rows: Vec<(u64, String)> = records
        .iter()
        .map(|record| {
            (
                u64::from(record.letter),
                format!("{}:  {}", record.letter, record.source),
            )
        })
        .collect();
    list.set_rows(
        &rows
            .iter()
            .map(|(id, text)| (*id, text.as_str()))
            .collect::<Vec<_>>(),
    )?;
    if let Some((id, _)) = rows.first() {
        list.select_rows(&[*id])?;
    }
    spacer(&held, &root, -1, 11)?;
    let buttons = row(&held, &root, 22)?;
    spacer(&held, &buttons, -1, -1)?.set_layout(Layout {
        grow: 1,
        gap: 0,
        ..Default::default()
    })?;
    let ok = node(&held, &buttons, Kind::Button, text::OK, BUTTON_WIDTH, 22, 0)?;
    ok.set_state(state::DEFAULT)?;
    spacer(&held, &buttons, 6, -1)?;
    let cancel = node(
        &held,
        &buttons,
        Kind::Button,
        text::CANCEL,
        BUTTON_WIDTH,
        22,
        0,
    )?;
    list.focus()?;
    while let Some(event) = events.next().await? {
        match event.kind {
            EventKind::Close
            | EventKind::Key {
                code: 1,
                pressed: true,
                ..
            } => return Ok(()),
            EventKind::Activate { .. } if event.node == cancel.id() => return Ok(()),
            EventKind::Activate { .. } if event.node == ok.id() => {
                let letters: Vec<char> = list
                    .selected_rows()?
                    .into_iter()
                    .filter_map(|id| u32::try_from(id).ok().and_then(char::from_u32))
                    .collect();
                if letters.is_empty() {
                    continue;
                }
                let mut failure = None;
                for letter in letters {
                    let store = store.clone();
                    if let Err(error) = background.run(move || store.disconnect(letter))?.await {
                        failure = Some(error.to_string());
                        break;
                    }
                }
                match failure {
                    None => return Ok(()),
                    Some(message) => {
                        notify(dialogs, &window, text::DISCONNECT_TITLE, &message).await?
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

pub async fn show(
    dialogs: &Dialogs,
    background: &Background,
    parent: &Window,
    disconnect_mode: bool,
) -> Result<()> {
    let store = background.run(Store::current)?.await?;
    let records = {
        let store = store.clone();
        background.run(move || store.list())?.await?
    };
    if disconnect_mode {
        return disconnect(dialogs, background, parent, &store, &records).await;
    }
    let mut options = WindowOptions::new(
        text::MAP_TITLE,
        "org.liteos.file-explorer",
        PAGE.0,
        PAGE.1 + 2 + 10 + 21 + 10,
    );
    options.fixed = true;
    options.modal = true;
    options.parent = Some(parent);
    let (window, mut events) = dialogs.window(options)?;
    let held = Held::default();
    let mut spec = WidgetSpec::new(Kind::Column, "");
    spec.layout = Layout {
        grow: 1,
        gap: 0,
        ..Default::default()
    };
    let root = window.widget(spec)?;
    let body = row(&held, &root, PAGE.1)?;
    let banner = node(&held, &body, Kind::Column, "", BANNER_WIDTH, PAGE.1, 0)?;
    banner.set_background(None, Some("network-drive-banner-1x"))?;
    let page = node(&held, &body, Kind::Column, "", -1, -1, 1)?;
    spacer(&held, &page, -1, 10)?;
    let intro = row(&held, &page, 36)?;
    spacer(&held, &intro, INDENT, -1)?;
    node(&held, &intro, Kind::Label, text::MAP_INTRO, -1, -1, 1)?.set_state(state::WRAP)?;
    spacer(&held, &page, -1, 11)?;
    let prompt = row(&held, &page, 18)?;
    spacer(&held, &prompt, INDENT, -1)?;
    node(&held, &prompt, Kind::Label, text::MAP_PROMPT, -1, -1, 1)?.set_state(state::WRAP)?;
    spacer(&held, &page, -1, 12)?;
    let drive_row = row(&held, &page, 21)?;
    spacer(&held, &drive_row, INDENT, -1)?;
    caption(&held, &drive_row, text::DRIVE, CAPTION)?;
    let drive = node(&held, &drive_row, Kind::Combo, "", 141, 21, 0)?;
    drive.set_state(state::DROPDOWN_LIST)?;
    // 下拉列出尚未映射的 D: 到 Z:，默认选最后一个空闲盘符（Windows 从 Z: 往前分配）。
    let free: Vec<char> = ('D'..='Z')
        .filter(|letter| !records.iter().any(|record| record.letter == *letter))
        .collect();
    let names: Vec<String> = free.iter().map(|letter| format!("{letter}:")).collect();
    drive.set_tree_rows(
        &free
            .iter()
            .zip(&names)
            .map(|(letter, name)| TreeRow {
                id: u64::from(*letter),
                parent: 0,
                text: name,
                expanded: true,
                has_children: false,
            })
            .collect::<Vec<_>>(),
    )?;
    if let Some(letter) = free.last() {
        drive.select(Some(u64::from(*letter)))?;
        drive.set_text(&format!("{letter}:"))?;
    }
    spacer(&held, &page, -1, 3)?;
    let folder_row = row(&held, &page, 21)?;
    spacer(&held, &folder_row, INDENT, -1)?;
    caption(&held, &folder_row, text::FOLDER, CAPTION)?;
    let folder = node(&held, &folder_row, Kind::Combo, "", 141, 21, 0)?;
    spacer(&held, &folder_row, 4, -1)?;
    // 没有网络邻居枚举，“浏览”无事可做；保留控件位置与禁用外观，避免伪造一个空浏览器。
    node(
        &held,
        &folder_row,
        Kind::Button,
        text::BROWSE,
        BUTTON_WIDTH,
        21,
        0,
    )?
    .set_state(state::DISABLED)?;
    spacer(&held, &page, -1, 6)?;
    let example = row(&held, &page, 14)?;
    spacer(&held, &example, INDENT + CAPTION, -1)?;
    node(&held, &example, Kind::Label, text::EXAMPLE, -1, -1, 1)?;
    spacer(&held, &page, -1, 4)?;
    let reconnect_row = row(&held, &page, 15)?;
    spacer(&held, &reconnect_row, INDENT + CAPTION, -1)?;
    let reconnect = node(
        &held,
        &reconnect_row,
        Kind::CheckBox,
        text::RECONNECT,
        -1,
        15,
        1,
    )?;
    spacer(&held, &page, -1, 6)?;
    let other_row = row(&held, &page, 14)?;
    spacer(&held, &other_row, INDENT + CAPTION, -1)?;
    let (before, link, after) = anchor(text::OTHER_USER);
    node(&held, &other_row, Kind::Label, before, 26, -1, 0)?;
    let other = node(&held, &other_row, Kind::Link, link, 62, -1, 0)?;
    node(&held, &other_row, Kind::Label, after, -1, -1, 1)?;
    let (ok, cancel) = wizard_buttons(&held, &root)?;
    folder.focus()?;
    let mut identity = Identity {
        user: String::new(),
        password: String::new(),
        remember: false,
        encrypt: false,
    };
    while let Some(event) = events.next().await? {
        match event.kind {
            EventKind::Close
            | EventKind::Key {
                code: 1,
                pressed: true,
                ..
            } => return Ok(()),
            EventKind::Activate { .. } if event.node == cancel.id() => return Ok(()),
            EventKind::Activate { .. } if event.node == other.id() => {
                if let Some(value) =
                    connect_as(dialogs, &window, &folder.text()?, &identity).await?
                {
                    identity = value;
                }
            }
            EventKind::Activate { .. } if event.node == ok.id() => {
                let text = drive.text()?;
                let selected = text
                    .trim()
                    .trim_end_matches(':')
                    .chars()
                    .next()
                    .filter(|c| ('D'..='Z').contains(c));
                let Some(selected) = selected else {
                    notify(
                        dialogs,
                        &window,
                        text::MAP_TITLE,
                        "请输入 D: 到 Z: 之间的盘符。",
                    )
                    .await?;
                    continue;
                };
                let mapping = Mapping::new(
                    selected,
                    &folder.text()?,
                    reconnect.state()? & state::CHECKED != 0,
                    identity.encrypt,
                );
                let mapping = match mapping {
                    Ok(value) => value,
                    Err(error) => {
                        notify(
                            dialogs,
                            &window,
                            text::MAP_TITLE,
                            &format!("共享路径无效：{error}"),
                        )
                        .await?;
                        continue;
                    }
                };
                // “域\用户”按 Windows 写法拆开；没有反斜杠时没有域。
                let (domain, username) = identity
                    .user
                    .split_once('\\')
                    .map_or(("", identity.user.as_str()), |(domain, user)| {
                        (domain, user)
                    });
                let credentials = Credentials {
                    username: username.to_owned(),
                    password: std::mem::take(&mut identity.password),
                    domain: domain.to_owned(),
                };
                let remember = identity.remember;
                let store = store.clone();
                let result = background
                    .run(move || store.connect(&mapping, &credentials, remember))?
                    .await;
                match result {
                    Ok(()) => return Ok(()),
                    Err(error) => {
                        notify(dialogs, &window, text::MAP_TITLE, &error.to_string()).await?
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

impl crate::Session {
    /// 恢复实际连接后再进入挂载目录；不能把离线目录展示为空共享。
    pub(super) async fn open_network_drive(&self, letter: char) -> Result<()> {
        let path = self
            .background
            .run(move || Store::current()?.activate(letter))?
            .await?;
        self.navigate(
            crate::model::Location::Directory(path),
            crate::browser::Visit::Push,
            true,
        )
        .await
    }
    /// 映射对话框关闭后刷新当前列表与命名空间；网络副作用由 Store 独占。
    pub(super) async fn network_drive_dialog(&self, disconnect: bool) -> Result<()> {
        show(
            &self.dialogs,
            &self.background,
            &self.view.window,
            disconnect,
        )
        .await?;
        self.refresh().await
    }
}
