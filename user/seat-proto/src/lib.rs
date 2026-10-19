//! Seat 切换协议：文本控制台与桌面 compositor 共用显示器和键盘，同一时刻只有一个 session 持有。
//!
//! 本 crate 只有两件事：线上帧编解码，以及无 IO 的 [`Seat`] 状态机。`seatd` 负责 socket、超时和
//! 终止对端，这里的状态机只把事件翻译成 [`Action`]，因此切换规则可以在 host 上完整单测。
//!
//! 持有显示器的含义由 session 自己实现：`Acquire` 时打开 DRM 并 `set_master`、打开 evdev；
//! `Release` 时关闭二者并回复 [`ClientFrame::Released`]。内核没有 VT 层，关闭 DRM fd 即交还显示。

/// seatd 监听的 unix socket 路径；`/run` 是易失 tmpfs，重启后自然清除旧 socket。
pub const SOCKET_PATH: &str = "/run/seat.sock";

/// 一个可持有显示器与键盘的 session 种类；每种至多一个已注册 client。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// 显示器上的文本控制台。
    Text,
    /// 图形桌面（compositor）。
    Desktop,
}

impl Role {
    fn index(self) -> usize {
        match self {
            Role::Text => 0,
            Role::Desktop => 1,
        }
    }

    /// Ctrl+Alt+F`n` 对应的 session：F1 文本、F2 桌面；其他功能键没有绑定。
    ///
    /// 参数是 evdev keycode（`KEY_F1` = 59、`KEY_F2` = 60）。快捷键由持有座位的 session 自己识别
    /// （compositor 独占 grab 键盘，seatd 看不到事件），识别后向 seatd 发 [`ClientFrame::Switch`]。
    pub fn from_function_key(code: u16) -> Option<Role> {
        match code {
            59 => Some(Role::Text),
            60 => Some(Role::Desktop),
            _ => None,
        }
    }

    /// `liteos.target=` 与 `seatctl` 参数使用的名字。
    pub fn parse(name: &str) -> Option<Role> {
        match name {
            "text" => Some(Role::Text),
            "desktop" => Some(Role::Desktop),
            _ => None,
        }
    }
}

/// session → seatd。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClientFrame {
    /// 连接后的第一帧；声明自己是哪种 session。
    Register(Role),
    /// 已关闭 DRM 与 evdev，显示器和键盘可交给下一个 session。
    Released,
    /// 请求切换到 `Role`（已注册 session 的快捷键，或未注册的 `seatctl`）。
    Switch(Role),
}

/// seatd → session。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServerFrame {
    /// 请关闭 DRM 与 evdev 并回复 `Released`；session 自身状态必须保留。
    Release,
    /// 现在持有显示器与键盘：打开 DRM、`set_master`、全量重绘、打开 evdev。
    Acquire,
    /// `Switch` 的结果。
    Switched(SwitchResult),
}

/// `Switch` 请求的结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwitchResult {
    /// 已开始（或立即完成）切换。
    Started,
    /// 目标已是 active。
    AlreadyActive,
    /// 目标 session 尚未注册，无法切换；不会让显示器落入无人持有的状态。
    Unavailable,
}

const REGISTER: u8 = 1;
const RELEASED: u8 = 2;
const SWITCH: u8 = 3;
const RELEASE: u8 = 16;
const ACQUIRE: u8 = 17;
const SWITCHED: u8 = 18;

fn role_byte(role: Role) -> u8 {
    role.index() as u8 + 1
}

fn byte_role(byte: u8) -> Option<Role> {
    match byte {
        1 => Some(Role::Text),
        2 => Some(Role::Desktop),
        _ => None,
    }
}

fn result_byte(result: SwitchResult) -> u8 {
    match result {
        SwitchResult::Started => 0,
        SwitchResult::AlreadyActive => 1,
        SwitchResult::Unavailable => 2,
    }
}

fn byte_result(byte: u8) -> Option<SwitchResult> {
    match byte {
        0 => Some(SwitchResult::Started),
        1 => Some(SwitchResult::AlreadyActive),
        2 => Some(SwitchResult::Unavailable),
        _ => None,
    }
}

/// 帧固定为 2 字节（`[opcode, argument]`），stream socket 上按 2 字节切分即可，无需长度前缀。
pub const FRAME_BYTES: usize = 2;

impl ClientFrame {
    /// 编码为固定长度帧。
    pub fn encode(self) -> [u8; FRAME_BYTES] {
        match self {
            ClientFrame::Register(role) => [REGISTER, role_byte(role)],
            ClientFrame::Released => [RELEASED, 0],
            ClientFrame::Switch(role) => [SWITCH, role_byte(role)],
        }
    }

    /// 解码；未知 opcode 或参数返回 `None`，调用方应断开该 client。
    pub fn decode(bytes: [u8; FRAME_BYTES]) -> Option<ClientFrame> {
        match bytes[0] {
            REGISTER => byte_role(bytes[1]).map(ClientFrame::Register),
            RELEASED if bytes[1] == 0 => Some(ClientFrame::Released),
            SWITCH => byte_role(bytes[1]).map(ClientFrame::Switch),
            _ => None,
        }
    }
}

impl ServerFrame {
    /// 编码为固定长度帧。
    pub fn encode(self) -> [u8; FRAME_BYTES] {
        match self {
            ServerFrame::Release => [RELEASE, 0],
            ServerFrame::Acquire => [ACQUIRE, 0],
            ServerFrame::Switched(result) => [SWITCHED, result_byte(result)],
        }
    }

    /// 解码；未知 opcode 或参数返回 `None`。
    pub fn decode(bytes: [u8; FRAME_BYTES]) -> Option<ServerFrame> {
        match bytes[0] {
            RELEASE if bytes[1] == 0 => Some(ServerFrame::Release),
            ACQUIRE if bytes[1] == 0 => Some(ServerFrame::Acquire),
            SWITCHED => byte_result(bytes[1]).map(ServerFrame::Switched),
            _ => None,
        }
    }
}

/// seatd 分配的连接标识；同一 Role 重新注册会得到新的 `ClientId`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClientId(pub u64);

/// 状态机要求 seatd 执行的副作用。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// 向 client 发送一帧。
    Send(ClientId, ServerFrame),
    /// 对端在 `Release` 后没有回复：关闭连接并终止其进程，随后以 `Disconnected` 反馈。
    Evict(ClientId),
}

/// 状态机输入事件。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// client 完成注册。
    Registered(ClientId, Role),
    /// client 回复 `Released`。
    Released(ClientId),
    /// `seatctl` 或快捷键请求切换。
    Switch(Role),
    /// client 连接关闭（含被驱逐）。
    Disconnected(ClientId),
    /// 启动宽限期结束：初始 session 一直没有注册时回退到可用的 session，避免黑屏。
    StartupTimeout,
    /// `Release` 发出后超过期限仍无回复。
    ReleaseTimeout,
}

/// Seat 仲裁状态。唯一 owner：`seatd` 进程；任何 session 不得自行判断自己是否 active。
pub struct Seat {
    initial: Role,
    clients: [Option<ClientId>; 2],
    active: Option<Role>,
    /// 已向 `active` 发出 `Release`、等待其回复时的切换目标。缺少该字段，收到 `Released` 时就
    /// 不知道该把显示器交给谁。
    pending: Option<Role>,
}

impl Seat {
    /// `initial` 是启动后第一个获得显示器的 session（来自 `liteos.target`）。
    pub fn new(initial: Role) -> Self {
        Self {
            initial,
            clients: [None, None],
            active: None,
            pending: None,
        }
    }

    /// 当前持有（或正在释放）显示器的 session。
    pub fn active(&self) -> Option<Role> {
        self.active
    }

    /// 是否有等待 `Released` 的切换。
    pub fn release_pending(&self) -> bool {
        self.pending.is_some()
    }

    fn client(&self, role: Role) -> Option<ClientId> {
        self.clients[role.index()]
    }

    fn role_of(&self, id: ClientId) -> Option<Role> {
        [Role::Text, Role::Desktop]
            .into_iter()
            .find(|role| self.client(*role) == Some(id))
    }

    /// 把显示器交给 `role`（必须已注册）。
    fn activate(&mut self, role: Role, actions: &mut Vec<Action>) {
        let id = self
            .client(role)
            .expect("activate requires a registered client");
        self.active = Some(role);
        self.pending = None;
        actions.push(Action::Send(id, ServerFrame::Acquire));
    }

    /// 无人持有时的回退：优先 Text（始终可用于诊断），其次另一种；都没有则保持空。
    fn fall_back(&mut self, actions: &mut Vec<Action>) {
        self.active = None;
        self.pending = None;
        for role in [Role::Text, Role::Desktop] {
            if self.client(role).is_some() {
                self.activate(role, actions);
                return;
            }
        }
    }

    /// 处理一个事件，返回 seatd 必须执行的动作（按顺序）。
    pub fn handle(&mut self, event: Event) -> (Vec<Action>, Option<SwitchResult>) {
        let mut actions = Vec::new();
        let mut result = None;
        match event {
            Event::Registered(id, role) => {
                let replaced = self.clients[role.index()].replace(id);
                if self.active == Some(role) {
                    // 持有者崩溃后重连：新实例必须重新 Acquire，否则显示器无人绘制。
                    self.pending = None;
                    self.activate(role, &mut actions);
                } else if self.active.is_none() && role == self.initial {
                    self.activate(role, &mut actions);
                }
                let _ = replaced;
            }
            Event::Switch(to) => {
                result = Some(if self.active == Some(to) && self.pending.is_none() {
                    SwitchResult::AlreadyActive
                } else if self.client(to).is_none() {
                    SwitchResult::Unavailable
                } else if let Some(active) = self.active {
                    if self.pending.is_some() || active == to {
                        // 正在释放：只更新目标，不重复发送 Release。
                        self.pending = (active != to).then_some(to);
                    } else {
                        let holder = self.client(active).expect("active role is registered");
                        self.pending = Some(to);
                        actions.push(Action::Send(holder, ServerFrame::Release));
                    }
                    SwitchResult::Started
                } else {
                    self.activate(to, &mut actions);
                    SwitchResult::Started
                });
            }
            Event::Released(id) => {
                if self.role_of(id) == self.active && self.active.is_some() {
                    match self.pending.take() {
                        Some(target) if self.client(target).is_some() => {
                            self.activate(target, &mut actions)
                        }
                        // 目标在等待期间断开：回退，而不是让显示器无人持有。
                        _ => self.fall_back(&mut actions),
                    }
                }
            }
            Event::Disconnected(id) => {
                if let Some(role) = self.role_of(id) {
                    self.clients[role.index()] = None;
                    if self.active == Some(role) {
                        self.fall_back(&mut actions);
                    } else if self.pending == Some(role) {
                        // 目标消失；Released 到来时 fall_back 会处理，这里只清除过期目标。
                        self.pending = None;
                    }
                }
            }
            Event::StartupTimeout => {
                if self.active.is_none() {
                    self.fall_back(&mut actions);
                }
            }
            Event::ReleaseTimeout => {
                let holder = self
                    .active
                    .filter(|_| self.pending.is_some())
                    .and_then(|active| self.client(active));
                actions.extend(holder.map(Action::Evict));
            }
        }
        (actions, result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: ClientId = ClientId(1);
    const DESKTOP: ClientId = ClientId(2);

    fn run(seat: &mut Seat, event: Event) -> Vec<Action> {
        seat.handle(event).0
    }

    fn both_registered(initial: Role) -> Seat {
        let mut seat = Seat::new(initial);
        run(&mut seat, Event::Registered(TEXT, Role::Text));
        run(&mut seat, Event::Registered(DESKTOP, Role::Desktop));
        seat
    }

    #[test]
    fn function_keys_bind_only_f1_and_f2() {
        assert_eq!(Role::from_function_key(59), Some(Role::Text));
        assert_eq!(Role::from_function_key(60), Some(Role::Desktop));
        assert_eq!(Role::from_function_key(61), None);
        assert_eq!(Role::from_function_key(30), None);
    }

    #[test]
    fn frames_roundtrip_and_reject_garbage() {
        for frame in [
            ClientFrame::Register(Role::Text),
            ClientFrame::Register(Role::Desktop),
            ClientFrame::Released,
            ClientFrame::Switch(Role::Desktop),
        ] {
            assert_eq!(ClientFrame::decode(frame.encode()), Some(frame));
        }
        for frame in [
            ServerFrame::Release,
            ServerFrame::Acquire,
            ServerFrame::Switched(SwitchResult::Unavailable),
        ] {
            assert_eq!(ServerFrame::decode(frame.encode()), Some(frame));
        }
        assert_eq!(ClientFrame::decode([9, 1]), None);
        assert_eq!(ClientFrame::decode([REGISTER, 7]), None);
        assert_eq!(ServerFrame::decode([RELEASE, 1]), None);
    }

    #[test]
    fn initial_role_acquires_on_register_and_the_other_waits() {
        let mut seat = Seat::new(Role::Text);
        assert_eq!(
            run(&mut seat, Event::Registered(DESKTOP, Role::Desktop)),
            vec![]
        );
        assert_eq!(seat.active(), None);
        assert_eq!(
            run(&mut seat, Event::Registered(TEXT, Role::Text)),
            vec![Action::Send(TEXT, ServerFrame::Acquire)]
        );
        assert_eq!(seat.active(), Some(Role::Text));
    }

    #[test]
    fn switch_releases_the_holder_then_acquires_the_target() {
        let mut seat = both_registered(Role::Text);
        let (actions, result) = seat.handle(Event::Switch(Role::Desktop));
        assert_eq!(result, Some(SwitchResult::Started));
        assert_eq!(actions, vec![Action::Send(TEXT, ServerFrame::Release)]);
        assert_eq!(
            seat.active(),
            Some(Role::Text),
            "holder keeps the seat until Released"
        );
        assert_eq!(
            run(&mut seat, Event::Released(TEXT)),
            vec![Action::Send(DESKTOP, ServerFrame::Acquire)]
        );
        assert_eq!(seat.active(), Some(Role::Desktop));
        assert!(!seat.release_pending());
    }

    #[test]
    fn switch_to_unregistered_or_active_role_changes_nothing() {
        let mut seat = Seat::new(Role::Text);
        run(&mut seat, Event::Registered(TEXT, Role::Text));
        assert_eq!(
            seat.handle(Event::Switch(Role::Desktop)),
            (vec![], Some(SwitchResult::Unavailable))
        );
        assert_eq!(
            seat.handle(Event::Switch(Role::Text)),
            (vec![], Some(SwitchResult::AlreadyActive))
        );
        assert_eq!(seat.active(), Some(Role::Text));
    }

    #[test]
    fn repeated_switch_while_releasing_does_not_resend_release() {
        let mut seat = both_registered(Role::Text);
        seat.handle(Event::Switch(Role::Desktop));
        let (actions, result) = seat.handle(Event::Switch(Role::Desktop));
        assert_eq!((actions, result), (vec![], Some(SwitchResult::Started)));
    }

    #[test]
    fn switching_back_while_releasing_cancels_to_the_holder() {
        let mut seat = both_registered(Role::Text);
        seat.handle(Event::Switch(Role::Desktop));
        seat.handle(Event::Switch(Role::Text));
        // 持有者已被要求 Release，回复后只能重新 Acquire 自己。
        assert_eq!(
            run(&mut seat, Event::Released(TEXT)),
            vec![Action::Send(TEXT, ServerFrame::Acquire)]
        );
    }

    #[test]
    fn active_client_crash_falls_back_to_text() {
        let mut seat = both_registered(Role::Desktop);
        assert_eq!(seat.active(), Some(Role::Desktop));
        assert_eq!(
            run(&mut seat, Event::Disconnected(DESKTOP)),
            vec![Action::Send(TEXT, ServerFrame::Acquire)]
        );
        assert_eq!(seat.active(), Some(Role::Text));
    }

    #[test]
    fn restarted_holder_reacquires_the_seat() {
        let mut seat = both_registered(Role::Text);
        run(&mut seat, Event::Disconnected(TEXT));
        // Desktop 没有注册为 active，但 fall_back 把座位给了它。
        assert_eq!(seat.active(), Some(Role::Desktop));
        let respawned = ClientId(3);
        run(&mut seat, Event::Registered(respawned, Role::Desktop));
        assert_eq!(seat.active(), Some(Role::Desktop));
    }

    #[test]
    fn target_disconnect_during_release_falls_back_instead_of_blackout() {
        let mut seat = both_registered(Role::Desktop);
        seat.handle(Event::Switch(Role::Text));
        run(&mut seat, Event::Disconnected(TEXT));
        assert_eq!(
            run(&mut seat, Event::Released(DESKTOP)),
            vec![Action::Send(DESKTOP, ServerFrame::Acquire)]
        );
        assert_eq!(seat.active(), Some(Role::Desktop));
    }

    #[test]
    fn release_timeout_evicts_only_the_unresponsive_holder() {
        let mut seat = both_registered(Role::Desktop);
        assert_eq!(run(&mut seat, Event::ReleaseTimeout), vec![]);
        seat.handle(Event::Switch(Role::Text));
        assert_eq!(
            run(&mut seat, Event::ReleaseTimeout),
            vec![Action::Evict(DESKTOP)]
        );
        // 被驱逐后连接关闭，座位回退到 Text。
        assert_eq!(
            run(&mut seat, Event::Disconnected(DESKTOP)),
            vec![Action::Send(TEXT, ServerFrame::Acquire)]
        );
    }

    #[test]
    fn startup_timeout_activates_text_when_the_initial_role_never_registers() {
        let mut seat = Seat::new(Role::Desktop);
        run(&mut seat, Event::Registered(TEXT, Role::Text));
        assert_eq!(seat.active(), None);
        assert_eq!(
            run(&mut seat, Event::StartupTimeout),
            vec![Action::Send(TEXT, ServerFrame::Acquire)]
        );
        // 已有 active 时超时无副作用。
        assert_eq!(run(&mut seat, Event::StartupTimeout), vec![]);
    }

    #[test]
    fn stale_released_from_a_non_holder_is_ignored() {
        let mut seat = both_registered(Role::Text);
        assert_eq!(run(&mut seat, Event::Released(DESKTOP)), vec![]);
        assert_eq!(seat.active(), Some(Role::Text));
    }
}
