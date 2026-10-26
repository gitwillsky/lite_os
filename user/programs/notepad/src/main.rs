//! 记事本只管理文件和保存决策；多行编辑与通用对话框由共享运行库提供。
use liteos_ui::{
    Application, Background, ControlFlow, Dialogs, EditCommand, EventKind, FileDialogMode, Layout,
    Menu, MenuItem, MessageButtons, MessageResult, Widget, WidgetKind as Kind, WidgetSpec, Window,
    WindowOptions,
};
use std::{
    cell::{Cell, RefCell},
    io::{self, Read},
    path::{Path, PathBuf},
    process::ExitCode,
    rc::Rc,
};
const MAX_BYTES: u64 = 1 << 20;
#[derive(Clone, Copy)]
enum Command {
    New,
    Open,
    Save,
    SaveAs,
    Exit,
    About,
}
#[derive(Default)]
struct FileState {
    path: Option<PathBuf>,
    revision: u64,
    saved: u64,
}
struct Session {
    window: Window,
    editor: Widget,
    status: Widget,
    state: RefCell<FileState>,
    busy: Cell<bool>,
    dialogs: Dialogs,
    background: Background,
}
impl Session {
    fn changed(&self) -> liteos_ui::Result<()> {
        self.state.borrow_mut().revision += 1;
        self.title()
    }
    fn title(&self) -> liteos_ui::Result<()> {
        let state = self.state.borrow();
        let name = state
            .path
            .as_ref()
            .and_then(|path| path.file_name())
            .map_or_else(|| "无标题".into(), |name| name.to_string_lossy());
        self.window.set_title(&format!(
            "{}{name} - 记事本",
            if state.revision != state.saved {
                "*"
            } else {
                ""
            }
        ))
    }
    fn modified(&self) -> bool {
        let state = self.state.borrow();
        state.revision != state.saved
    }
    fn directory(&self) -> PathBuf {
        self.state
            .borrow()
            .path
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_owned)
            .unwrap_or_else(|| {
                std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from)
            })
    }
    async fn save(&self, choose: bool) -> Result<bool, Box<dyn std::error::Error>> {
        let current = self.state.borrow().path.clone();
        let path = match current {
            Some(path) if !choose => path,
            current => {
                let name = current
                    .as_ref()
                    .and_then(|path| path.file_name())
                    .map_or_else(|| "无标题.txt".into(), |name| name.to_string_lossy());
                let Some(path) = self
                    .dialogs
                    .file_filtered(
                        &self.window,
                        FileDialogMode::Save,
                        &self.directory(),
                        &name,
                        liteos_ui::FileFilter {
                            label: "文本文档 (*.txt)",
                            extensions: &["txt"],
                        },
                    )?
                    .await?
                else {
                    return Ok(false);
                };
                path
            }
        };
        let text = self.editor.text()?;
        let revision = self.state.borrow().revision;
        let target = path.clone();
        self.status.set_text("正在保存…")?;
        // 已开始的写入必须完成；窗口关闭请求在 busy 时保留，UI 不 join 后台线程。
        self.background
            .run(move || std::fs::write(target, text))?
            .await?;
        {
            let mut state = self.state.borrow_mut();
            state.path = Some(path);
            state.saved = revision;
        }
        self.title()?;
        self.status.set_text("已保存")?;
        Ok(true)
    }
    async fn confirm(&self) -> Result<bool, Box<dyn std::error::Error>> {
        while self.modified() {
            match self
                .dialogs
                .message(
                    &self.window,
                    "记事本",
                    "文件内容已修改。是否保存更改？",
                    MessageButtons::YesNoCancel,
                )?
                .await?
            {
                MessageResult::Yes => {
                    if !self.save(false).await? {
                        return Ok(false);
                    }
                }
                MessageResult::No => return Ok(true),
                _ => return Ok(false),
            }
        }
        Ok(true)
    }
    async fn load(&self, path: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
        let source = path.clone();
        self.status.set_text("正在打开…")?;
        let bytes = self
            .background
            .run(move || -> io::Result<Vec<u8>> {
                let file = std::fs::File::open(source)?;
                let mut bytes = Vec::new();
                file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
                if bytes.len() as u64 > MAX_BYTES {
                    return Err(io::Error::other("文件超过 1 MiB"));
                }
                Ok(bytes)
            })?
            .await?;
        self.editor.set_text(&String::from_utf8_lossy(&bytes))?;
        {
            let mut state = self.state.borrow_mut();
            state.path = Some(path);
            state.revision += 1;
            state.saved = state.revision;
        }
        self.title()?;
        self.status.set_text("就绪")?;
        Ok(())
    }
    async fn execute(&self, command: Command) -> Result<(), Box<dyn std::error::Error>> {
        match command {
            Command::New if self.confirm().await? => {
                self.editor.set_text("")?;
                let mut state = self.state.borrow_mut();
                state.path = None;
                state.revision += 1;
                state.saved = state.revision;
                drop(state);
                self.title()?;
                self.status.set_text("就绪")?;
            }
            Command::Open if self.confirm().await? => {
                if let Some(path) = self
                    .dialogs
                    .file_filtered(
                        &self.window,
                        FileDialogMode::Open,
                        &self.directory(),
                        "",
                        liteos_ui::FileFilter {
                            label: "文本文档 (*.txt)",
                            extensions: &["txt"],
                        },
                    )?
                    .await?
                {
                    self.load(path).await?;
                }
            }
            Command::Exit if self.confirm().await? => self.window.close()?,
            Command::Save => {
                self.save(false).await?;
            }
            Command::SaveAs => {
                self.save(true).await?;
            }
            Command::About => {
                self.dialogs
                    .message(
                        &self.window,
                        "关于记事本",
                        "LiteOS 记事本\n纯文本编辑器。",
                        MessageButtons::Ok,
                    )?
                    .await?;
            }
            _ => {}
        }
        Ok(())
    }
    async fn report(&self, error: &dyn std::fmt::Display) {
        let text = error.to_string();
        let _ = self.status.set_text(&text);
        if let Ok(dialog) = self
            .dialogs
            .message(&self.window, "记事本", &text, MessageButtons::Ok)
        {
            let _ = dialog.await;
        }
    }
}
/// 业务任务的忙碌期只串行文件副作用；编辑仍可进行，保存结果按 revision 保留后续修改。
struct Busy(Rc<Session>);
impl Drop for Busy {
    fn drop(&mut self) {
        self.0.busy.set(false);
    }
}
fn child(parent: &Widget, kind: Kind, text: &str, grow: u32) -> liteos_ui::Result<Widget> {
    let mut spec = WidgetSpec::new(kind, text);
    spec.layout.grow = grow;
    parent.child(spec)
}
fn clock_text() -> String {
    let seconds = calendar::zone::to_local(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_secs() as i64),
    );
    let date = calendar::civil_from_days(seconds.div_euclid(86400));
    let minutes = seconds.rem_euclid(86400) / 60;
    format!(
        "{}:{:02} {}-{:02}-{:02}",
        minutes / 60,
        minutes % 60,
        date.year,
        date.month,
        date.day
    )
}
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut app = Application::new()?;
    let window = app.window(WindowOptions::new(
        "无标题 - 记事本",
        "org.liteos.notepad",
        640,
        480,
    ))?;
    let mut spec = WidgetSpec::new(Kind::Column, "");
    spec.layout = Layout {
        grow: 1,
        ..Default::default()
    };
    let root = window.widget(spec)?;
    let menu = child(&root, Kind::MenuBar, "", 0)?;
    menu.set_menus(&[
        Menu {
            title: "文件(F)",
            items: &[
                MenuItem {
                    id: 1,
                    label: "新建(N)",
                    shortcut: "Ctrl+N",
                    state: 0,
                    parent: 0,
                },
                MenuItem {
                    id: 2,
                    label: "打开(O)...",
                    shortcut: "Ctrl+O",
                    state: 0,
                    parent: 0,
                },
                MenuItem {
                    id: 3,
                    label: "保存(S)",
                    shortcut: "Ctrl+S",
                    state: 0,
                    parent: 0,
                },
                MenuItem {
                    id: 4,
                    label: "另存为(A)...",
                    shortcut: "",
                    state: 0,
                    parent: 0,
                },
                MenuItem {
                    id: 5,
                    label: "退出(X)",
                    shortcut: "",
                    state: 4,
                    parent: 0,
                },
            ],
        },
        Menu {
            title: "编辑(E)",
            items: &[
                MenuItem {
                    id: 6,
                    label: "剪切(T)",
                    shortcut: "Ctrl+X",
                    state: 0,
                    parent: 0,
                },
                MenuItem {
                    id: 7,
                    label: "复制(C)",
                    shortcut: "Ctrl+C",
                    state: 0,
                    parent: 0,
                },
                MenuItem {
                    id: 8,
                    label: "粘贴(P)",
                    shortcut: "Ctrl+V",
                    state: 0,
                    parent: 0,
                },
                MenuItem {
                    id: 9,
                    label: "删除(L)",
                    shortcut: "Del",
                    state: 0,
                    parent: 0,
                },
                MenuItem {
                    id: 10,
                    label: "全选(A)",
                    shortcut: "Ctrl+A",
                    state: 4,
                    parent: 0,
                },
                MenuItem {
                    id: 11,
                    label: "时间/日期(D)",
                    shortcut: "F5",
                    state: 0,
                    parent: 0,
                },
            ],
        },
        Menu {
            title: "帮助(H)",
            items: &[MenuItem {
                id: 12,
                label: "关于记事本(A)",
                shortcut: "",
                state: 0,
                parent: 0,
            }],
        },
    ])?;
    let editor = child(&root, Kind::TextEditor, "", 1)?;
    let status = child(&root, Kind::Status, "就绪", 0)?;
    let session = Rc::new(Session {
        window: window.clone(),
        editor,
        status,
        state: RefCell::new(FileState::default()),
        busy: Cell::new(false),
        dialogs: app.dialogs(),
        background: app.background()?,
    });
    let mut task = None;
    if let Some(path) = std::env::args_os().nth(1).map(PathBuf::from) {
        session.busy.set(true);
        let guard = Busy(session.clone());
        task = Some(app.spawn_local(&window, async move {
            if let Err(error) = guard.0.load(path).await {
                guard.0.report(&error).await;
            }
        })?);
    }
    app.run(|app, event| {
        if event.window != window.id() {
            return Ok(ControlFlow::Continue);
        }
        if matches!(event.kind, EventKind::Changed { .. }) && event.node == session.editor.id() {
            session.changed()?;
        }
        let id = match event.kind {
            EventKind::Close => 5,
            EventKind::Activate { value } if event.node == menu.id() => value,
            EventKind::Key {
                pressed: true,
                handled: false,
                code,
                modifiers,
                ..
            } => match (code, modifiers & 2 != 0) {
                (49, true) => 1,
                (24, true) => 2,
                (31, true) => 3,
                (63, false) => 11,
                _ => 0,
            },
            EventKind::ClipboardError { .. } => {
                session.status.set_text("剪贴板传输失败")?;
                0
            }
            _ => 0,
        };
        let edit = match id {
            6 => Some(EditCommand::Cut),
            7 => Some(EditCommand::Copy),
            8 => Some(EditCommand::Paste),
            9 => Some(EditCommand::Delete),
            10 => Some(EditCommand::SelectAll),
            11 => {
                session.editor.edit(EditCommand::Insert(&clock_text()))?;
                None
            }
            _ => None,
        };
        if let Some(edit) = edit {
            session.editor.edit(edit)?;
        }
        let command = match id {
            1 => Some(Command::New),
            2 => Some(Command::Open),
            3 => Some(Command::Save),
            4 => Some(Command::SaveAs),
            5 => Some(Command::Exit),
            12 => Some(Command::About),
            _ => None,
        };
        if let Some(command) = command
            && !session.busy.replace(true)
        {
            let guard = Busy(session.clone());
            task = Some(app.spawn_local(&window, async move {
                if let Err(error) = guard.0.execute(command).await {
                    guard.0.report(&error).await;
                }
            })?);
        }
        Ok(if id == 5 {
            ControlFlow::PreventDefault
        } else {
            ControlFlow::Continue
        })
    })?;
    drop(task);
    Ok(())
}
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("notepad: {error}");
            ExitCode::FAILURE
        }
    }
}
