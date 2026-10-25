//! 可信本机登录、锁定与用户会话监督。认证策略不进入 compositor 或控件库。
mod account;
mod paint;

use classic_ui::{
    Display, Edit, EventKind, Key, KeyboardInteractivity, Layer, LayerSettings, ShellLease,
    WindowId,
};
use linux_uapi::process::{SessionChild, SessionCommand, SessionIo, SessionUser};
use std::{
    io::{self, Read, Write},
    os::unix::net::UnixStream,
    path::Path,
    process::{Command, ExitCode},
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Welcome,
    Credentials,
    Loading,
    Desktop,
    Security,
    Locked,
    Unlock,
    Message,
    Shutdown,
    Password,
}
struct UserSession {
    child: SessionChild,
    user: SessionUser,
    // 父进程持有 capability 租约；注销先终止进程组，再关闭租约，不留下仍运行的旧外壳。
    _lease: ShellLease,
    control: UnixStream,
    ready: bool,
    started: Instant,
    login_time: String,
}
struct Logon {
    page: Page,
    username: Edit,
    password: Edit,
    new_password: Edit,
    confirmation: Edit,
    focus: usize,
    pointer: (f64, f64),
    armed: Option<usize>,
    options: bool,
    session: Option<UserSession>,
    message: String,
    return_page: Page,
    retry: Instant,
    shutdown: usize,
}
impl Logon {
    fn new() -> Self {
        Self {
            page: Page::Welcome,
            username: Edit::new("root"),
            password: Edit::default(),
            new_password: Edit::default(),
            confirmation: Edit::default(),
            focus: 0,
            pointer: (0., 0.),
            armed: None,
            options: false,
            session: None,
            message: String::new(),
            return_page: Page::Welcome,
            retry: Instant::now(),
            shutdown: 0,
        }
    }
    fn secure(&mut self, display: &mut Display, window: WindowId, page: Page) -> io::Result<()> {
        self.password.clear_secret();
        self.new_password.clear_secret();
        self.confirmation.clear_secret();
        if page == Page::Unlock
            && let Some(session) = &self.session
        {
            self.username = Edit::new(&session.user.name);
        }
        if page == Page::Shutdown && self.session.is_none() && self.shutdown == 0 {
            self.shutdown = 1;
        }
        self.page = page;
        println!("logon: page {page:?}");
        self.armed = None;
        self.focus = if matches!(page, Page::Credentials | Page::Unlock) {
            1
        } else {
            0
        };
        display.configure_layer(
            window,
            LayerSettings {
                layer: if page == Page::Desktop {
                    Layer::Background
                } else {
                    Layer::Overlay
                },
                keyboard: if page == Page::Desktop {
                    KeyboardInteractivity::None
                } else {
                    KeyboardInteractivity::Exclusive
                },
                exclusive_zone: -1,
                ..Default::default()
            },
        )
    }
    /// 目标账户（锁定时是会话用户，否则是默认用户名）没有口令时直接登录或解除锁定。
    ///
    /// # Returns
    ///
    /// 是否已经处理（调用者不应再显示登录对话框）。
    fn try_without_password(
        &mut self,
        display: &mut Display,
        window: WindowId,
    ) -> io::Result<bool> {
        if let Some(session) = &self.session {
            self.username = Edit::new(&session.user.name);
        }
        if !account::passwordless(self.username.text()) {
            return Ok(false);
        }
        println!("logon: account has no password, skipping the logon dialog");
        self.password.clear_secret();
        self.login(display, window)?;
        Ok(true)
    }
    fn fail(&mut self, message: &str) {
        self.return_page = if self.session.is_some() {
            Page::Unlock
        } else {
            Page::Credentials
        };
        self.password.clear_secret();
        self.message = message.into();
        self.page = Page::Message;
        self.focus = 0;
        self.retry = Instant::now() + Duration::from_secs(1);
    }
    fn login(&mut self, display: &mut Display, window: WindowId) -> io::Result<()> {
        if Instant::now() < self.retry {
            return Ok(());
        }
        let name = self.username.text();
        let authenticated =
            account::authenticate(name, self.password.text(), account::Purpose::Login);
        self.password.clear_secret();
        let user = match authenticated {
            Ok(Some(user)) => user,
            Ok(None) => {
                println!("logon: authentication rejected");
                self.fail("用户名或密码不正确，或账户已锁定、过期。请重新输入。");
                return Ok(());
            }
            Err(error) => {
                eprintln!("logon: authentication service: {error}");
                self.fail("无法读取本机账户。请在恢复控制台检查 /etc/shadow。");
                return Ok(());
            }
        };
        if let Some(session) = &self.session {
            // 不能用另一个普通账户接管既有桌面；同一内核 UID 或本机 UID 0 管理员才可解锁。
            if user.identity.uid != session.user.identity.uid && user.identity.uid != 0 {
                println!("logon: authentication rejected");
                self.fail("此账户不能解除当前会话锁定。");
                return Ok(());
            }
            self.secure(display, window, Page::Desktop)?;
            println!("logon: unlocked");
            return Ok(());
        }
        let result = (|| {
            account::runtime(&user)?;
            let path = Path::new("/run/liteos/desktop-listener");
            let (socket, lease) = display
                .delegate_shell(
                    path,
                    &format!("{}-{}", user.identity.uid, std::process::id()),
                )?
                .split();
            let (control, child_control) = UnixStream::pair()?;
            let child = SessionChild::spawn(
                SessionCommand::new(
                    "/bin/desktop",
                    Vec::new(),
                    SessionIo::Wayland {
                        socket: socket.into(),
                        control: child_control.into(),
                    },
                )
                .user(user.clone()),
            )?;
            Ok::<_, io::Error>(UserSession {
                child,
                user,
                _lease: lease,
                control,
                ready: false,
                started: Instant::now(),
                login_time: Command::new("/bin/date")
                    .arg("+%Y-%m-%d %H:%M:%S UTC")
                    .env_remove("WAYLAND_SOCKET")
                    .output()
                    .ok()
                    .filter(|output| output.status.success())
                    .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
                    .unwrap_or_default(),
            })
        })();
        match result {
            Ok(session) => {
                println!("logon: authenticated uid={}", session.user.identity.uid);
                self.session = Some(session);
                self.secure(display, window, Page::Loading)?;
            }
            Err(error) => {
                eprintln!("logon: session launch: {error}");
                self.fail("无法启动桌面会话。请检查主目录与系统文件。");
            }
        }
        Ok(())
    }
    fn change_password(&mut self, display: &mut Display, window: WindowId) -> io::Result<()> {
        let Some(session) = &self.session else {
            return Err(io::ErrorKind::PermissionDenied.into());
        };
        let valid = !self.new_password.text().is_empty()
            && self.new_password.text() == self.confirmation.text()
            && account::authenticate(
                &session.user.name,
                self.password.text(),
                account::Purpose::PasswordChange,
            )?
            .is_some();
        if !valid {
            self.password.clear_secret();
            self.new_password.clear_secret();
            self.confirmation.clear_secret();
            self.return_page = Page::Password;
            self.message = "原密码不正确、新密码不一致，或尚未达到最短改密间隔。".into();
            self.page = Page::Message;
            return Ok(());
        }
        // 账户文件锁定、随机 salt 与原子替换都由固定 BusyBox passwd 实现；不在 logon 复制写入算法。
        let mut child = Command::new("/bin/passwd")
            .args(["-a", "sha512", "--", &session.user.name])
            .env_remove("WAYLAND_SOCKET")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        let result = (|| {
            let mut input = child
                .stdin
                .take()
                .ok_or_else(|| io::Error::other("passwd stdin missing"))?;
            input.write_all(self.new_password.text().as_bytes())?;
            input.write_all(b"\n")?;
            input.write_all(self.new_password.text().as_bytes())?;
            input.write_all(b"\n")?;
            drop(input);
            Ok::<_, io::Error>(())
        })();
        self.password.clear_secret();
        self.new_password.clear_secret();
        self.confirmation.clear_secret();
        let status = child.wait()?;
        self.return_page = Page::Security;
        self.message = if result.is_ok() && status.success() {
            println!("logon: password changed");
            "密码已更改。"
        } else {
            println!("logon: password change failed");
            "无法更改密码，请在恢复控制台检查账户文件。"
        }
        .into();
        self.page = Page::Message;
        self.secure(display, window, Page::Message)
    }

    fn logout(&mut self, display: &mut Display, window: WindowId) -> io::Result<()> {
        // 先覆盖桌面，再回收进程；SessionChild 无论 leader 是否已退出都清理整个拥有的组。
        self.secure(display, window, Page::Welcome)?;
        if let Some(mut session) = self.session.take() {
            session.child.terminate()?;
        }
        println!("logon: logged out");
        Ok(())
    }
    fn supervise(&mut self, display: &mut Display, window: WindowId) -> io::Result<bool> {
        use linux_uapi::unix::{PollEvents, PollFd, poll};
        use std::os::fd::AsFd;
        let Some(session) = &mut self.session else {
            return Ok(false);
        };
        if let Some(status) = session.child.try_wait()? {
            println!("logon: desktop exited {status}");
            self.logout(display, window)?;
            return Ok(true);
        }
        let mut fd = [PollFd::new(session.control.as_fd(), PollEvents::READ)];
        if poll(&mut fd, Some(Duration::ZERO))? != 0 {
            let mut byte = [0];
            if session.control.read(&mut byte)? == 0 {
                self.logout(display, window)?;
                return Ok(true);
            }
            match byte[0] {
                1 if !session.ready => {
                    session.ready = true;
                    self.secure(display, window, Page::Desktop)?;
                    println!("logon: desktop ready");
                }
                2 if session.ready && self.page == Page::Desktop => {
                    self.secure(display, window, Page::Locked)?;
                    println!("logon: locked");
                }
                4 if session.ready && self.page == Page::Desktop => {
                    self.return_page = Page::Desktop;
                    self.secure(display, window, Page::Shutdown)?
                }
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid desktop session request",
                    ));
                }
            }
            return Ok(true);
        }
        if !session.ready && session.started.elapsed() > Duration::from_secs(30) {
            self.logout(display, window)?;
            self.fail("桌面启动超时。");
            return Ok(true);
        }
        Ok(false)
    }
    fn activate(
        &mut self,
        index: usize,
        display: &mut Display,
        window: WindowId,
    ) -> io::Result<()> {
        match self.page {
            Page::Welcome => {
                self.return_page = Page::Welcome;
                self.message = "按 Ctrl-Alt-Delete 开始登录。要启用口令登录，请在终端运行 passwd root 设置密码。".into();
                self.page = Page::Message;
            }
            Page::Credentials | Page::Unlock => match index {
                0 | 1 => self.focus = index,
                2 => self.login(display, window)?,
                3 => self.secure(
                    display,
                    window,
                    if self.session.is_some() {
                        Page::Locked
                    } else {
                        Page::Welcome
                    },
                )?,
                4 => {
                    self.return_page = Page::Credentials;
                    self.secure(display, window, Page::Shutdown)?;
                }
                5 => self.options = !self.options,
                _ => {}
            },
            Page::Security => match index {
                0 => {
                    self.secure(display, window, Page::Locked)?;
                    println!("logon: locked");
                }
                1 => self.logout(display, window)?,
                2 => {
                    self.return_page = Page::Security;
                    self.secure(display, window, Page::Shutdown)?;
                }
                3 => self.secure(display, window, Page::Password)?,
                4 => {
                    if let Some(session) = &mut self.session {
                        session.control.write_all(&[5])?;
                    }
                    self.secure(display, window, Page::Desktop)?;
                }
                5 => self.secure(display, window, Page::Desktop)?,
                _ => {}
            },
            Page::Password => match index {
                0..=2 => self.focus = index,
                3 => self.change_password(display, window)?,
                4 => self.secure(display, window, Page::Security)?,
                _ => {}
            },
            Page::Message => self.secure(display, window, self.return_page)?,
            Page::Shutdown => match index {
                0 if self.session.is_none() => {}
                0..=2 => self.shutdown = index,
                3 => match self.shutdown {
                    0 => self.logout(display, window)?,
                    1 => power(false)?,
                    _ => power(true)?,
                },
                4 => self.secure(display, window, self.return_page)?,
                _ => {}
            },
            _ => {}
        }
        Ok(())
    }
    fn key(&mut self, key: Key, display: &mut Display, window: WindowId) -> io::Result<()> {
        // 确认/取消不重复触发；否则耗时认证后按住 Enter 会连续越过消息框与安全页。
        if !key.pressed || (key.repeat && matches!(key.code, 1 | 15 | 28)) {
            return Ok(());
        }
        if key.code == 111 && key.modifiers.control && key.modifiers.alt && !key.repeat {
            let page = match self.page {
                Page::Welcome => Page::Credentials,
                Page::Locked => Page::Unlock,
                Page::Desktop => Page::Security,
                _ => return Ok(()),
            };
            println!("logon: secure attention {page:?}");
            // 没有设置口令的账户不需要对话框：直接登录或解除锁定。
            if matches!(page, Page::Credentials | Page::Unlock)
                && self.try_without_password(display, window)?
            {
                return Ok(());
            }
            self.secure(display, window, page)?;
            return Ok(());
        }
        match key.code {
            15 => {
                let count = match self.page {
                    Page::Credentials => 6,
                    Page::Unlock => 4,
                    Page::Security => 6,
                    Page::Password => 5,
                    Page::Shutdown => 5,
                    _ => 1,
                };
                self.focus = (self.focus + if key.modifiers.shift { count - 1 } else { 1 }) % count;
            }
            28 => self.activate(
                match self.page {
                    Page::Credentials | Page::Unlock => 2,
                    Page::Password => 3,
                    Page::Shutdown => 3,
                    _ => self.focus,
                },
                display,
                window,
            )?,
            1 => self.activate(
                match self.page {
                    Page::Credentials | Page::Unlock => 3,
                    Page::Security => 5,
                    Page::Password => 4,
                    Page::Shutdown => 4,
                    _ => 0,
                },
                display,
                window,
            )?,
            57 if matches!(self.page, Page::Security | Page::Shutdown) => {
                self.activate(self.focus, display, window)?
            }
            _ if self.page == Page::Password => {
                let edit = match self.focus {
                    0 => Some(&mut self.password),
                    1 => Some(&mut self.new_password),
                    2 => Some(&mut self.confirmation),
                    _ => None,
                };
                if let Some(edit) = edit
                    && edit.text().len() < 1024
                {
                    edit.key(key);
                }
            }
            _ if matches!(self.page, Page::Credentials | Page::Unlock) => {
                if self.focus == 0 {
                    self.username.key(key);
                }
                if self.focus == 1 && self.password.text().len() < 1024 {
                    self.password.key(key);
                }
            }
            _ => {}
        }
        Ok(())
    }
}
fn power(reboot: bool) -> io::Result<()> {
    let status = Command::new("/bin/shutdown")
        .args([if reboot { "-r" } else { "-P" }, "now"])
        .env_remove("WAYLAND_SOCKET")
        .status()?;
    if !status.success() {
        return Err(io::Error::other("shutdown failed"));
    }
    Ok(())
}
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut display = Display::connect()?;
    let window = display.create_layer(
        "org.liteos.logon",
        LayerSettings {
            layer: Layer::Overlay,
            keyboard: KeyboardInteractivity::Exclusive,
            exclusive_zone: -1,
            ..Default::default()
        },
    )?;
    let mut state = Logon::new();
    let mut art = paint::Art::load()?;
    let mut dirty = true;
    println!("logon: welcome");
    // 开机即登录：默认账户没有口令时不显示"按 Ctrl+Alt+Del"和登录对话框，直接进入桌面。
    if state.try_without_password(&mut display, window)? {
        dirty = true;
    }
    loop {
        dirty |= state.supervise(&mut display, window)?;
        if let Some(event) = display.next_event_timeout(Duration::from_millis(100))? {
            if event.window != window {
                continue;
            }
            match event.kind {
                EventKind::Close => return Err("trusted logon surface closed".into()),
                EventKind::Key(key) => {
                    state.key(key, &mut display, window)?;
                    dirty = true;
                }
                EventKind::Configure(_) | EventKind::Focus(_) => dirty = true,
                EventKind::PointerEnter { x, y } | EventKind::PointerMove { x, y } => {
                    state.pointer = (x, y)
                }
                EventKind::PointerLeave => {
                    state.armed = None;
                    dirty = true;
                }
                EventKind::Button {
                    code: 0x110,
                    pressed,
                    ..
                } => {
                    if let Some(c) = display.configuration(window) {
                        let hit = paint::areas(&state, c).iter().position(|r| {
                            state.pointer.0 >= f64::from(r.x)
                                && state.pointer.0 < f64::from(r.right())
                                && state.pointer.1 >= f64::from(r.y)
                                && state.pointer.1 < f64::from(r.bottom())
                        });
                        if pressed {
                            state.armed = hit;
                            if let Some(index) = hit {
                                state.focus = index;
                            }
                        } else if let Some(index) =
                            state.armed.take().filter(|index| Some(*index) == hit)
                        {
                            state.activate(index, &mut display, window)?;
                        }
                        dirty = true;
                    }
                }
                _ => {}
            }
        }
        if dirty && display.ready_to_draw(window) {
            let configuration = display
                .configuration(window)
                .ok_or("missing logon configure")?;
            let pixels = paint::frame(&state, configuration, &mut art)?;
            display.present(window, &pixels)?;
            dirty = false;
        }
    }
}
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("logon: error: {e}");
            ExitCode::FAILURE
        }
    }
}
