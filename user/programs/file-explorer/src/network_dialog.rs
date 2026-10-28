//! Windows 外壳的映射/断开流程；耗时认证和挂载只在 Background 执行。
use crate::Result;
use liteos_ui::{
    Background, Dialogs, EventKind, Layout, Widget, WidgetKind as Kind, WidgetSpec, Window,
    WindowOptions, state,
};
use network_drive::{Credentials, Mapping, Store};

mod text {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../assets/classic/text/network-drive.rs"
    ));
}

fn child(parent: &Widget, kind: Kind, text: &str) -> liteos_ui::Result<Widget> {
    let mut spec = WidgetSpec::new(kind, text);
    spec.layout = Layout {
        height: 23,
        ..Default::default()
    };
    parent.child(spec)
}
fn field(parent: &Widget, label: &str, initial: &str) -> liteos_ui::Result<Widget> {
    let row = child(parent, Kind::Row, "")?;
    let mut spec = WidgetSpec::new(Kind::Label, label);
    spec.layout = Layout {
        width: 110,
        ..Default::default()
    };
    row.child(spec)?;
    let mut spec = WidgetSpec::new(Kind::Edit, initial);
    spec.layout = Layout {
        grow: 1,
        ..Default::default()
    };
    row.child(spec)
}

pub async fn show(
    dialogs: &Dialogs,
    background: &Background,
    parent: &Window,
    disconnect: bool,
) -> Result<()> {
    let store = background.run(Store::current)?.await?;
    let records = {
        let store = store.clone();
        background.run(move || store.list())?.await?
    };
    let title = if disconnect {
        text::DISCONNECT_TITLE
    } else {
        text::MAP_TITLE
    };
    let mut options = WindowOptions::new(
        title,
        "org.liteos.file-explorer",
        420,
        if disconnect { 235 } else { 350 },
    );
    options.fixed = true;
    options.modal = true;
    options.parent = Some(parent);
    let (window, mut events) = dialogs.window(options)?;
    let mut spec = WidgetSpec::new(Kind::Column, "");
    spec.layout = Layout {
        grow: 1,
        gap: 7,
        padding: 12,
        ..Default::default()
    };
    let root = window.widget(spec)?;
    let prompt = child(
        &root,
        Kind::Label,
        if disconnect {
            text::DISCONNECT_PROMPT
        } else {
            text::MAP_PROMPT
        },
    )?;
    prompt.set_state(state::WRAP)?;
    let initial = if disconnect {
        records.first().map(|record| record.letter)
    } else {
        ('D'..='Z')
            .rev()
            .find(|letter| !records.iter().any(|record| record.letter == *letter))
    };
    let letter = field(
        &root,
        text::DRIVE,
        &initial.map_or_else(String::new, |letter| format!("{letter}:")),
    )?;
    let source;
    let username;
    let password;
    let domain;
    let remember;
    let reconnect;
    let encrypt;
    if disconnect {
        let values = records
            .iter()
            .map(|record| format!("{}:  {}", record.letter, record.source))
            .collect::<Vec<_>>()
            .join("\n");
        let mut spec = WidgetSpec::new(Kind::Label, &values);
        spec.layout = Layout {
            grow: 1,
            ..Default::default()
        };
        root.child(spec)?;
        source = None;
        username = None;
        password = None;
        domain = None;
        remember = None;
        reconnect = None;
        encrypt = None;
    } else {
        source = Some(field(&root, text::FOLDER, "\\\\服务器\\共享")?);
        username = Some(field(&root, text::USERNAME, "")?);
        password = Some(field(&root, text::PASSWORD, "")?);
        password
            .as_ref()
            .expect("password field")
            .set_state(state::PASSWORD)?;
        domain = Some(field(&root, text::DOMAIN, "")?);
        remember = Some(child(&root, Kind::CheckBox, "保存密码（仅文件权限保护）")?);
        reconnect = Some(child(&root, Kind::CheckBox, text::RECONNECT)?);
        encrypt = Some(child(&root, Kind::CheckBox, "要求 SMB 3 加密")?);
    }
    let message = child(&root, Kind::Label, "")?;
    message.set_state(state::WRAP)?;
    let buttons = child(&root, Kind::Row, "")?;
    let ok = child(
        &buttons,
        Kind::Button,
        if disconnect { text::OK } else { "完成" },
    )?;
    ok.set_state(state::DEFAULT)?;
    let cancel = child(&buttons, Kind::Button, text::CANCEL)?;
    letter.focus()?;
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
                let text = letter.text()?;
                let text = text.trim().trim_end_matches(':');
                let selected = if text.chars().count() == 1 {
                    text.chars()
                        .next()
                        .map(|c| c.to_ascii_uppercase())
                        .filter(|c| ('D'..='Z').contains(c))
                } else {
                    None
                };
                let Some(selected) = selected else {
                    message.set_text("请输入 D: 到 Z: 之间的盘符。")?;
                    continue;
                };
                let result = if disconnect {
                    let store = store.clone();
                    background.run(move || store.disconnect(selected))?.await
                } else {
                    let checked = |widget: &Option<Widget>| -> liteos_ui::Result<bool> {
                        Ok(widget.as_ref().expect("map field").state()? & state::CHECKED != 0)
                    };
                    let mapping = Mapping::new(
                        selected,
                        &source.as_ref().expect("source").text()?,
                        checked(&reconnect)?,
                        checked(&encrypt)?,
                    );
                    let mapping = match mapping {
                        Ok(value) => value,
                        Err(error) => {
                            message.set_text(&format!("共享路径无效：{error}"))?;
                            continue;
                        }
                    };
                    let credentials = Credentials {
                        username: username.as_ref().expect("username").text()?,
                        password: password.as_ref().expect("password").text()?,
                        domain: domain.as_ref().expect("domain").text()?,
                    };
                    password.as_ref().expect("password").set_text("")?;
                    let remember = checked(&remember)?;
                    let store = store.clone();
                    background
                        .run(move || store.connect(&mapping, &credentials, remember))?
                        .await
                };
                match result {
                    Ok(()) => return Ok(()),
                    Err(error) => message.set_text(&error.to_string())?,
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
