//! 指针语义层：把 `input` 设备层解析出的坐标 / 按钮转换派发为窗口管理动作。
//!
//! - 命中开始菜单（打开时）：按下记录按下态，release 仍在同项内才生效（程序项
//!   经 `supervisor.spawn_one` 把 conf 命令作为 terminal argv[1] 拉起，`终端`
//!   拉普通终端，`关机` 置 shutdown 标志）；点菜单外关闭菜单（落在 Start 按钮
//!   上的点击除外，由其 toggling 关闭）。
//! - 命中任务栏：按下记录按下态，release 仍在同一目标内才生效（Start 切换
//!   开始菜单；窗口按钮按 XP 行为：最小化 → 还原聚焦，焦点 → 最小化，否则
//!   置顶聚焦）。
//! - 命中窗口：任意位置（标题栏按钮除外）按下即 raise + focus；标题栏进入移动
//!   拖动（最大化窗口禁止）；右 / 下边缘与右下角 8px（1× 基准 4px）命中带进入
//!   resize 拖动——拖动中窗口不动、只画 2px 示意框，松开时按示意框减去 chrome
//!   向客户端发 `CONFIGURE`（最小内容 320x192，1× 基准 160x96），客户端
//!   `SET_BUFFER` 后窗口才采用新尺寸。
//! - 标题栏三按钮：press + release 都在同一按钮内才生效（关闭发
//!   `CLOSE_REQUEST`，最大化 Normal ↔ Maximized 切换并向客户端发 `CONFIGURE`
//!   建议新内容尺寸让其重排，最小化进入 Minimized）。

use display_proto::{CloseRequest, Configure, Focus, InputPointer};

use crate::{
    chrome,
    clients::Clients,
    compositor::Damage,
    input::Input,
    scanout::Rect,
    startmenu::{Item, StartMenu},
    supervisor::Supervisor,
    taskbar::{self, Taskbar, Target},
    window::{Region, State, Window, Windows},
};

/// 左键 bit（拖动 / 按钮判定只对左键生效）。
const BUTTON_LEFT: u32 = 1;
/// resize 建议内容尺寸下限（px，1× 基准 160）。
const MIN_CONTENT_WIDTH: i32 = 160 * chrome::SCALE;
/// resize 建议内容尺寸下限（px，1× 基准 96）。
const MIN_CONTENT_HEIGHT: i32 = 96 * chrome::SCALE;
/// 移动拖动 clamp 留屏可点区域（px，1× 基准 32）。
const KEEP_ON_SCREEN: i32 = 32 * chrome::SCALE;

/// 左键拖动状态：移动窗口（本体跟手）或 resize（窗口不动，只更新示意框）。
pub(crate) enum Drag {
    /// 标题栏移动拖动。
    Move {
        surface_id: u32,
        /// 按下点相对窗口外框原点的偏移。
        offset_x: i32,
        offset_y: i32,
    },
    /// resize 拖动：锚定外框左上角，`outline` 为当前示意框。
    Resize {
        surface_id: u32,
        east: bool,
        south: bool,
        outline: Rect,
    },
}

/// 指针语义派发所需的共享状态束（避免函数签名参数过长）。
pub struct PointerShell<'a> {
    pub windows: &'a mut Windows,
    pub clients: &'a Clients,
    pub damage: &'a mut Damage,
    pub taskbar: &'a mut Taskbar,
    pub supervisor: &'a mut Supervisor,
    pub startmenu: &'a mut StartMenu,
    /// 置 `true` 请求关机（事件循环随后进入关机画面并停止响应输入）。
    pub shutdown: &'a mut bool,
    pub screen_width: i32,
    pub screen_height: i32,
}

impl PointerShell<'_> {
    /// work area：全屏减去任务栏高度（最大化窗口的外框）。
    fn work_area(&self) -> Rect {
        Rect::new(0, 0, self.screen_width, self.screen_height - taskbar::HEIGHT)
    }
}

impl Input {
    /// 按钮转换（按下）：开始菜单（打开时）> 任务栏 > 窗口命中；窗口任意
    /// 位置（按钮除外）按下即 raise + focus。
    pub(crate) fn press(&mut self, bit: u32, shell: &mut PointerShell) {
        self.buttons |= bit;
        if bit == BUTTON_LEFT && shell.startmenu.is_open() {
            // 菜单是最顶层弹出 UI：命中菜单项记录按下态；菜单矩形内的空白区
            // 点击不生效也不关菜单；点菜单外关闭（Start 按钮除外——它走任务
            // 栏按下态，release 时 toggle 关闭）。
            if shell.startmenu.rect().contains(self.cursor_x, self.cursor_y) {
                if let Some(item) = shell.startmenu.hit_test(self.cursor_x, self.cursor_y) {
                    shell.startmenu.press(item);
                    shell.damage.add(shell.startmenu.rect());
                }
                return;
            }
            if !shell.taskbar.start_rect().contains(self.cursor_x, self.cursor_y) {
                let closed = shell.startmenu.close();
                shell.damage.add(closed);
            }
        }
        // 任务栏是最顶层内部 UI，命中优先于窗口。
        if let Some(target) = shell
            .taskbar
            .hit_test(shell.windows, self.cursor_x, self.cursor_y)
        {
            if bit == BUTTON_LEFT {
                let rect = shell.taskbar.press(target, shell.windows);
                shell.damage.add(rect);
            }
            return;
        }
        let Some((slot, region)) = shell.windows.hit_test(self.cursor_x, self.cursor_y) else {
            return;
        };
        if bit != BUTTON_LEFT {
            if region == Region::Content {
                focus_raise(shell.windows, shell.clients, shell.damage, slot);
                self.forward_pointer(shell.windows, shell.clients, slot);
            }
            return;
        }
        // 标题栏按钮：只记录按下态（不按 raise；release 仍在按钮内才生效）。
        if region.button().is_some() {
            if let Some(window) = shell.windows.get(slot) {
                self.armed = Some((window.surface_id, region));
                shell.damage.add(window.button_rect(region));
            }
            return;
        }
        focus_raise(shell.windows, shell.clients, shell.damage, slot);
        let Some(window) = shell.windows.get(slot) else {
            return;
        };
        if let Some((east, south)) = region.resize_edges() {
            let outline = window.outer_rect();
            self.drag = Some(Drag::Resize {
                surface_id: window.surface_id,
                east,
                south,
                outline,
            });
            shell.damage.add(outline);
        } else if region == Region::TitleBar && !window.geometry_locked() {
            self.drag = Some(Drag::Move {
                surface_id: window.surface_id,
                offset_x: self.cursor_x - window.x,
                offset_y: self.cursor_y - window.y,
            });
        } else if region == Region::Content {
            self.forward_pointer(shell.windows, shell.clients, slot);
        }
    }

    /// 按钮转换（抬起）：结算开始菜单按下态、任务栏按下态、标题栏按钮
    /// 按下态与拖动。
    pub(crate) fn release(&mut self, bit: u32, shell: &mut PointerShell) {
        self.buttons &= !bit;
        if bit == BUTTON_LEFT {
            if shell.startmenu.is_pressed() {
                let confirmed = shell.startmenu.release(self.cursor_x, self.cursor_y);
                let closed = shell.startmenu.close();
                shell.damage.add(closed);
                if let Some(item) = confirmed {
                    menu_action(item, shell);
                }
            } else if shell.taskbar.is_pressed() {
                let (confirmed, rect) =
                    shell.taskbar
                        .release(shell.windows, self.cursor_x, self.cursor_y);
                shell.damage.add(rect);
                if let Some(target) = confirmed {
                    taskbar_action(target, shell);
                }
            } else if let Some((surface_id, region)) = self.armed.take()
                && let Some(slot) = shell.windows.by_surface(surface_id)
            {
                if let Some(window) = shell.windows.get(slot) {
                    shell.damage.add(window.button_rect(region));
                }
                let confirmed = matches!(
                    shell.windows.hit_test(self.cursor_x, self.cursor_y),
                    Some((hit, hit_region)) if hit == slot && hit_region == region
                );
                if confirmed {
                    button_action(slot, region, shell);
                }
            }
            if let Some(Drag::Resize {
                surface_id, outline, ..
            }) = self.drag.take()
            {
                // 松开：擦除示意框并按最终尺寸向客户端建议新内容尺寸。
                shell.damage.add(outline);
                if let Some(slot) = shell.windows.by_surface(surface_id) {
                    send_configure(slot, outline, shell);
                }
            }
        }
        // 让焦点窗口看到按键释放（指针在其内容区内时）。
        if let Some(focused) = shell.windows.focused() {
            self.forward_pointer(shell.windows, shell.clients, focused);
        }
    }

    /// 无按钮转换的光标移动：拖动窗口 / 更新 resize 示意框、悬停转发或拖动中
    /// 转发。
    pub(crate) fn motion(&mut self, shell: &mut PointerShell) {
        match self.drag.take() {
            Some(Drag::Move {
                surface_id,
                offset_x,
                offset_y,
            }) => {
                let Some(slot) = shell.windows.by_surface(surface_id) else {
                    return;
                };
                let Some(window) = shell.windows.get_mut(slot) else {
                    return;
                };
                if !window.geometry_locked() {
                    let old = window.outer_rect();
                    // clamp：至少保留 KEEP_ON_SCREEN 可点区域在屏内，标题栏不推出上沿。
                    let layout = window.layout();
                    let new_x = (self.cursor_x - offset_x)
                        .clamp(KEEP_ON_SCREEN - layout.outer_width, shell.screen_width - KEEP_ON_SCREEN);
                    let new_y =
                        (self.cursor_y - offset_y).clamp(0, shell.screen_height - KEEP_ON_SCREEN);
                    if new_x != window.x || new_y != window.y {
                        window.x = new_x;
                        window.y = new_y;
                        shell.damage.add(old);
                        shell.damage.add(window.outer_rect());
                    }
                }
                self.drag = Some(Drag::Move {
                    surface_id,
                    offset_x,
                    offset_y,
                });
            }
            Some(Drag::Resize {
                surface_id,
                east,
                south,
                outline,
            }) => {
                let Some(slot) = shell.windows.by_surface(surface_id) else {
                    // 缩放期间 client 断连：取消拖动并擦除示意框。
                    shell.damage.add(outline);
                    return;
                };
                let Some(window) = shell.windows.get(slot) else {
                    shell.damage.add(outline);
                    return;
                };
                // 窗口本体不动：示意框锚定外框左上角，仅按方向更新右 / 下缘，
                // 下限保证减去 chrome 后内容不小于 320x192（1× 基准 160x96）。
                let anchor = window.outer_rect();
                let mut next = Rect::new(anchor.x1, anchor.y1, outline.x2, outline.y2);
                if east {
                    next.x2 = self.cursor_x.clamp(
                        anchor.x1 + MIN_CONTENT_WIDTH + 2 * chrome::BORDER,
                        shell.screen_width,
                    );
                }
                if south {
                    next.y2 = self.cursor_y.clamp(
                        anchor.y1 + MIN_CONTENT_HEIGHT + chrome::TITLE_HEIGHT + chrome::BORDER,
                        shell.screen_height,
                    );
                }
                if next != outline {
                    shell.damage.add(outline);
                    shell.damage.add(next);
                }
                self.drag = Some(Drag::Resize {
                    surface_id,
                    east,
                    south,
                    outline: next,
                });
            }
            None => {
                if self.buttons == 0 {
                    // 菜单打开时优先做菜单悬停；指针在菜单矩形内时不再向窗口
                    // 内容区转发悬停。
                    if shell.startmenu.is_open() {
                        let hover = shell.startmenu.hit_test(self.cursor_x, self.cursor_y);
                        shell.damage.add(shell.startmenu.set_hover(hover));
                        if shell.startmenu.rect().contains(self.cursor_x, self.cursor_y) {
                            return;
                        }
                    }
                    if let Some((slot, Region::Content)) =
                        shell.windows.hit_test(self.cursor_x, self.cursor_y)
                    {
                        self.forward_pointer(shell.windows, shell.clients, slot);
                    }
                } else if let Some(focused) = shell.windows.focused() {
                    self.forward_pointer(shell.windows, shell.clients, focused);
                }
            }
        }
    }

    /// 指针在窗口内容区内时转发 `INPUT_POINTER`（内容相对坐标 + buttons）。
    fn forward_pointer(&self, windows: &Windows, clients: &Clients, slot: usize) {
        let Some(window) = windows.get(slot) else {
            return;
        };
        let content = window.content_rect();
        let x = self.cursor_x - content.x1;
        let y = self.cursor_y - content.y1;
        if !(0..content.width()).contains(&x) || !(0..content.height()).contains(&y) {
            return;
        }
        let message = InputPointer {
            surface_id: window.surface_id,
            x: x as u32,
            y: y as u32,
            buttons: self.buttons,
            wheel: 0,
        };
        let mut buffer = [0u8; 32];
        if let Some(length) = message.encode(&mut buffer) {
            clients.send(window.client, &buffer[..length]);
        }
    }

    /// 当前 resize 示意框（合成时叠加绘制）；非 resize 拖动时为 `None`。
    pub fn resize_outline(&self) -> Option<Rect> {
        match &self.drag {
            Some(Drag::Resize { outline, .. }) => Some(*outline),
            _ => None,
        }
    }

    /// 当前按住的标题栏按钮（surface id + 区域），合成时画按下态。
    pub fn armed_button(&self) -> Option<(u32, Region)> {
        self.armed
    }

    /// 拖动 / 按下态引用的窗口已销毁（client 断连）时取消对应状态；
    /// resize 取消时 damage 示意框区域以擦除。
    pub fn validate_drag(&mut self, windows: &Windows, damage: &mut Damage) {
        // 先取出判定结果再改状态，避免对 self.drag 的同时借用。
        enum Stale {
            Move,
            Resize(Rect),
        }
        let stale = match &self.drag {
            Some(Drag::Move { surface_id, .. })
                if windows.by_surface(*surface_id).is_none() =>
            {
                Some(Stale::Move)
            }
            Some(Drag::Resize {
                surface_id, outline, ..
            }) if windows.by_surface(*surface_id).is_none() => Some(Stale::Resize(*outline)),
            _ => None,
        };
        match stale {
            Some(Stale::Move) => self.drag = None,
            Some(Stale::Resize(outline)) => {
                damage.add(outline);
                self.drag = None;
            }
            None => {}
        }
        if let Some((surface_id, _)) = self.armed
            && windows.by_surface(surface_id).is_none()
        {
            self.armed = None;
        }
    }
}

/// 任务栏按钮确认点击后的动作（XP 行为）。
fn taskbar_action(target: Target, shell: &mut PointerShell) {
    match target {
        Target::Start => {
            let rect = shell.startmenu.toggle();
            shell.damage.add(rect);
        }
        Target::Window(surface_id) => {
            let Some(slot) = shell.windows.by_surface(surface_id) else {
                return;
            };
            let Some(window) = shell.windows.get(slot) else {
                return;
            };
            let state = window.state();
            if state == State::Minimized {
                let outer = window.outer_rect();
                if let Some(window) = shell.windows.get_mut(slot) {
                    window.unminimize();
                }
                focus_raise(shell.windows, shell.clients, shell.damage, slot);
                shell.damage.add(outer);
                damage_window_button(shell, surface_id);
            } else if shell.windows.focused() == Some(slot) {
                minimize_window(slot, shell.windows, shell.clients, shell.damage);
                damage_window_button(shell, surface_id);
            } else {
                focus_raise(shell.windows, shell.clients, shell.damage, slot);
            }
        }
    }
}

/// 开始菜单项确认点击后的动作：程序项把 conf 命令作为 terminal argv[1]
/// 拉起，`终端` 拉普通终端，`关机` 置 shutdown 标志（事件循环随后进入
/// 关机画面并停止响应输入）。
fn menu_action(item: Item, shell: &mut PointerShell) {
    match item {
        Item::Shutdown => *shell.shutdown = true,
        other => {
            let command = shell.startmenu.command(other);
            shell.supervisor.spawn_one(command);
        }
    }
}

/// 标题栏按钮确认点击后的动作。
fn button_action(slot: usize, region: Region, shell: &mut PointerShell) {
    match region {
        Region::CloseButton => {
            let Some(window) = shell.windows.get(slot) else {
                return;
            };
            let message = CloseRequest {
                surface_id: window.surface_id,
            };
            let mut buffer = [0u8; 16];
            if let Some(length) = message.encode(&mut buffer) {
                shell.clients.send(window.client, &buffer[..length]);
            }
        }
        Region::MaximizeButton => {
            let Some(window) = shell.windows.get(slot) else {
                return;
            };
            if !window.decorated || window.state() == State::Minimized {
                return;
            }
            let old = window.outer_rect();
            let work_area = shell.work_area();
            let mut configure = None;
            if let Some(window) = shell.windows.get_mut(slot) {
                configure = window.toggle_maximize(work_area);
            }
            if let Some(window) = shell.windows.get(slot) {
                shell.damage.add(old);
                shell.damage.add(window.outer_rect());
                // 进入最大化建议 work area 内容尺寸，还原建议 saved geometry
                // 内容尺寸；客户端 SET_BUFFER 后内容重排填满 / 外框自适应。
                if let Some((width, height)) = configure {
                    send_configure_size(window, width, height, shell.clients);
                }
            }
        }
        Region::MinimizeButton => {
            let surface_id = shell.windows.get(slot).map(|window| window.surface_id);
            minimize_window(slot, shell.windows, shell.clients, shell.damage);
            if let Some(surface_id) = surface_id {
                damage_window_button(shell, surface_id);
            }
        }
        _ => {}
    }
}

/// 最小化窗口：记 damage 旧外框；焦点窗口被最小化时焦点回落栈顶可见窗口。
pub fn minimize_window(
    slot: usize,
    windows: &mut Windows,
    clients: &Clients,
    damage: &mut Damage,
) {
    if let Some(window) = windows.get(slot) {
        damage.add(window.outer_rect());
    }
    if let Some(window) = windows.get_mut(slot) {
        window.minimize();
    }
    if windows.focused() == Some(slot) {
        let top = windows.top_visible();
        set_focus(windows, clients, damage, top);
    }
}

/// resize 松开：按示意框减去 chrome 计算建议内容尺寸（下限 320x192，1× 基准
/// 160x96）并向该 client 发 `CONFIGURE`；实际尺寸以客户端 `SET_BUFFER` 为准。
fn send_configure(slot: usize, outline: Rect, shell: &PointerShell) {
    let Some(window) = shell.windows.get(slot) else {
        return;
    };
    if !window.decorated {
        return;
    }
    let width = (outline.width() - 2 * chrome::BORDER).max(MIN_CONTENT_WIDTH);
    let height =
        (outline.height() - chrome::TITLE_HEIGHT - chrome::BORDER).max(MIN_CONTENT_HEIGHT);
    send_configure_size(window, width as u32, height as u32, shell.clients);
}

/// 编码并发送 `CONFIGURE{surface_id, width, height}` 给窗口的 owning client。
fn send_configure_size(window: &Window, width: u32, height: u32, clients: &Clients) {
    let message = Configure {
        surface_id: window.surface_id,
        width,
        height,
    };
    let mut buffer = [0u8; 24];
    if let Some(length) = message.encode(&mut buffer) {
        clients.send(window.client, &buffer[..length]);
    }
}

/// damage 某个窗口的任务栏按钮（按下态 / 文字 / 最小化置灰变化）。
fn damage_window_button(shell: &mut PointerShell, surface_id: u32) {
    if let Some(rect) = shell.taskbar.window_button_rect(shell.windows, surface_id) {
        shell.damage.add(rect);
    }
}

/// raise 窗口并把键盘焦点切过去（附带 `FOCUS` 消息与标题栏重画）。
pub fn focus_raise(
    windows: &mut Windows,
    clients: &Clients,
    damage: &mut Damage,
    slot: usize,
) {
    windows.raise(slot);
    set_focus(windows, clients, damage, Some(slot));
}

/// 切换键盘焦点：旧焦点发 `FOCUS{0}`、新焦点发 `FOCUS{1}`，两侧标题栏
/// 记入 damage（焦点色变化）。
pub fn set_focus(
    windows: &mut Windows,
    clients: &Clients,
    damage: &mut Damage,
    slot: Option<usize>,
) {
    if windows.focused() == slot {
        return;
    }
    if let Some(old) = windows.focused()
        && let Some(window) = windows.get(old)
    {
        send_focus(clients, window, 0);
        damage.add(title_bar_strip(window));
    }
    windows.set_focus(slot);
    if let Some(new) = slot
        && let Some(window) = windows.get(new)
    {
        send_focus(clients, window, 1);
        damage.add(title_bar_strip(window));
    }
}

fn send_focus(clients: &Clients, window: &Window, focused: u32) {
    let message = Focus {
        surface_id: window.surface_id,
        focused,
    };
    let mut buffer = [0u8; 16];
    if let Some(length) = message.encode(&mut buffer) {
        clients.send(window.client, &buffer[..length]);
    }
}

fn title_bar_strip(window: &Window) -> Rect {
    let outer = window.outer_rect();
    Rect::new(outer.x1, outer.y1, outer.x2, outer.y1 + crate::chrome::TITLE_HEIGHT)
}
