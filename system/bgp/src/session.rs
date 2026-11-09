//! 一条当前 TCP 连接的 BGP 状态与单调截止；不在状态转换中执行 I/O 或分配。
use super::{Config, InvalidConfig, Negotiated, Notice, negotiation::negotiate};
use packet_wire::bgp::{Frame, Kind, Message};

const SECOND: u64 = 1_000_000_000;
// RFC 4271 的大初始 HoldTimer：尚未收到 OPEN，不能使用还未协商的 hold time。
const OPEN_HOLD_SECONDS: u16 = 240;

/// RFC 4271 的六个会话状态；Idle 也可持重连退避截止，没有第二份 enabled/state 副本。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Idle,
    Connect,
    Active,
    OpenSent,
    OpenConfirm,
    Established,
}

/// 对当前连接的事件；传输 owner 必须先过滤已退休连接/连接尝试的旧就绪消息。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Input<'a> {
    Start,
    Stop,
    Connected,
    ConnectFailed,
    TransportLost,
    Frame(Frame<'a>),
    /// 已完整送入 TCP 的消息类型；部分发送/排队不算发送完成，不能提前延后 KEEPALIVE。
    Sent(Kind),
    /// 任意一次截止唤醒；按当前截止复核，旧 timer 消息不会直接操作已退休的期限。
    Timer,
    ProtocolError(Notice<'a>),
    /// Peer owner 已选出另一条连接；输家不得自己重连并与赢家反复碰撞。
    CollisionLost,
}

/// 需要传输/路由 actor 按顺序执行的副作用；没有回调、RPC 或第二个会话状态 owner。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[expect(
    clippy::large_enum_variant,
    reason = "UPDATE 只携固定数量的不可变借用视图；两槽事件窗口有界，装箱会为每条控制消息增加可失败的堆分配"
)]
pub enum Effect<'a> {
    Connect,
    /// 一个完整终止义务：通知是 best-effort，失败仍须关闭 TCP；withdraw_routes 为真时还须完成来源撤回，不能短路清理。
    Close {
        notice: Option<Notice<'a>>,
        withdraw_routes: bool,
        /// 关闭前捕获本方向的发送授权，不能在协商退休后再查询或无条件发送扩展通知。
        send_extended: bool,
    },
    SendOpen,
    SendKeepalive,
    Established,
    Update(super::ReceivedUpdate<'a>),
    Refresh {
        afi: u16,
        safi: u8,
    },
}
/// 一个事件最多产生“取消旧连接、尝试新连接”两项命令；终止义务不拆开，调用者必须在下一事件前消费。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Effects<'a> {
    entries: [Option<Effect<'a>>; 2],
    used: usize,
}
impl<'a> Effects<'a> {
    fn new() -> Self {
        Self {
            entries: [None; 2],
            used: 0,
        }
    }
    fn push(&mut self, effect: Effect<'a>) {
        self.entries[self.used] = Some(effect);
        self.used += 1;
    }
}
impl<'a> IntoIterator for Effects<'a> {
    type Item = Effect<'a>;
    type IntoIter = core::iter::Flatten<core::array::IntoIter<Option<Effect<'a>>, 2>>;
    fn into_iter(self) -> Self::IntoIter {
        self.entries.into_iter().flatten()
    }
}

/// 当前连接唯一的协议状态与期限；所有时间为单调纳秒，调用者不得倒退时钟或并发投递事件。
pub struct Session {
    config: Config,
    state: State,
    negotiated: Option<Negotiated>,
    retry_at: Option<u64>,
    hold_at: Option<u64>,
    keepalive_at: Option<u64>,
}
fn at(now: u64, seconds: u16) -> u64 {
    // 只在单调纳秒表示最终耗尽时饱和；不会溢出到已经到期的过去并形成忙循环。
    now.saturating_add(u64::from(seconds) * SECOND)
}
impl Session {
    /// 捕获合法参数，初始停止在 Idle，没有常驻 timer 或隐式主动连接。
    /// # Errors
    /// 非法本地参数；不构造部分会话或将本地错误归咎于对端。
    pub fn new(config: Config) -> Result<Self, InvalidConfig> {
        config.validate()?;
        Ok(Self {
            config,
            state: State::Idle,
            negotiated: None,
            retry_at: None,
            hold_at: None,
            keepalive_at: None,
        })
    }
    /// 唯一当前状态。
    pub fn state(&self) -> State {
        self.state
    }
    /// 借用参数的值投影，供发送本连接 OPEN；调用者不得另存可变会话政策。
    pub fn config(&self) -> Config {
        self.config
    }
    /// OPEN 确认阶段起可读的冻结协商；关闭时立即退休，不能沿用到重连。
    pub fn negotiated(&self) -> Option<Negotiated> {
        self.negotiated
    }
    /// 下一次实际待办截止；None 表示无定时工作，传输 actor 应无限等待 I/O。
    pub fn next_deadline(&self) -> Option<u64> {
        [self.retry_at, self.hold_at, self.keepalive_at]
            .into_iter()
            .flatten()
            .min()
    }
    fn start(&mut self, now: u64, effects: &mut Effects<'_>) {
        self.state = if self.config.passive {
            State::Active
        } else {
            State::Connect
        };
        self.retry_at = (!self.config.passive).then(|| at(now, self.config.retry_seconds));
        if !self.config.passive {
            effects.push(Effect::Connect);
        }
    }
    fn hold(&mut self, now: u64) {
        self.hold_at = self
            .negotiated
            .and_then(|peer| (peer.hold_seconds != 0).then(|| at(now, peer.hold_seconds)));
    }
    fn sent(&mut self, now: u64, kind: Kind) {
        if kind == Kind::Keepalive && matches!(self.state, State::OpenConfirm | State::Established)
            || kind == Kind::Update && self.state == State::Established
        {
            self.keepalive_at = self
                .negotiated
                .and_then(|peer| (peer.hold_seconds != 0).then(|| at(now, peer.hold_seconds / 3)));
        }
    }
    fn close<'a>(
        &mut self,
        now: u64,
        notice: Option<Notice<'a>>,
        restart: bool,
        effects: &mut Effects<'a>,
    ) {
        let established = self.state == State::Established;
        effects.push(Effect::Close {
            notice,
            withdraw_routes: established,
            send_extended: self.negotiated.is_some_and(|peer| peer.send_extended),
        });
        self.state = State::Idle;
        self.negotiated = None;
        self.hold_at = None;
        self.keepalive_at = None;
        self.retry_at = restart.then(|| at(now, self.config.retry_seconds));
    }
    fn unexpected<'a>(&mut self, now: u64, kind: Kind, effects: &mut Effects<'a>) {
        let subcode = match self.state {
            State::OpenSent => 1,
            State::OpenConfirm => 2,
            State::Established => 3,
            _ => 0,
        };
        let data: &[u8] = match kind {
            Kind::Open => &[1],
            Kind::Update => &[2],
            Kind::Notification => &[3],
            Kind::Keepalive => &[4],
            Kind::RouteRefresh => &[5],
        };
        self.close(
            now,
            Some(Notice {
                code: 5,
                subcode,
                data,
            }),
            true,
            effects,
        );
    }
    /// 消费当前连接的一个事件并返回有序副作用；本方法不进行分配、I/O 或通知其他 owner。
    /// # Parameters
    /// now 为单调纳秒；副作用执行失败必须反馈 TransportLost/ProtocolError，不能忽略撤回或继续使用已关闭的协商。
    pub fn on<'a>(&mut self, now: u64, input: Input<'a>) -> Effects<'a> {
        let mut effects = Effects::new();
        match input {
            Input::Start if self.state == State::Idle => self.start(now, &mut effects),
            Input::Stop => {
                if self.state != State::Idle {
                    let notice = matches!(
                        self.state,
                        State::OpenSent | State::OpenConfirm | State::Established
                    )
                    .then(|| Notice::simple(6, 2));
                    self.close(now, notice, false, &mut effects);
                } else {
                    self.retry_at = None;
                }
            }
            Input::Connected => {
                if matches!(self.state, State::Connect | State::Active) {
                    self.state = State::OpenSent;
                    self.retry_at = None;
                    self.hold_at = Some(at(now, OPEN_HOLD_SECONDS));
                    effects.push(Effect::SendOpen);
                } else if self.state == State::Idle {
                    effects.push(Effect::Close {
                        notice: None,
                        withdraw_routes: false,
                        send_extended: false,
                    });
                }
            }
            Input::ConnectFailed if self.state == State::Connect => {
                self.state = State::Active;
                self.retry_at = Some(at(now, self.config.retry_seconds));
                effects.push(Effect::Close {
                    notice: None,
                    withdraw_routes: false,
                    send_extended: false,
                });
            }
            Input::TransportLost if self.state != State::Idle => {
                self.close(now, None, true, &mut effects)
            }
            Input::ProtocolError(notice)
                if matches!(
                    self.state,
                    State::OpenSent | State::OpenConfirm | State::Established
                ) =>
            {
                self.close(now, Some(notice), true, &mut effects)
            }
            Input::CollisionLost
                if self.state != State::Established && self.state != State::Idle =>
            {
                let notice = matches!(self.state, State::OpenSent | State::OpenConfirm)
                    .then(|| Notice::simple(6, 7));
                self.close(now, notice, false, &mut effects)
            }
            Input::Frame(frame)
                if matches!(
                    self.state,
                    State::OpenSent | State::OpenConfirm | State::Established
                ) =>
            {
                if frame.header().length() > packet_wire::bgp::STANDARD_LIMIT
                    && !self.negotiated.is_some_and(|peer| peer.receive_extended)
                {
                    self.close(
                        now,
                        Some(Notice {
                            code: 1,
                            subcode: 2,
                            data: frame.length_bytes(),
                        }),
                        true,
                        &mut effects,
                    );
                    return effects;
                }
                match frame.message() {
                    Ok(Message::Notification { .. }) => self.close(now, None, true, &mut effects),
                    Ok(Message::Open(open)) if self.state == State::OpenSent => {
                        match negotiate(self.config, open) {
                            Ok(peer) => {
                                self.negotiated = Some(peer);
                                self.state = State::OpenConfirm;
                                self.hold(now);
                                effects.push(Effect::SendKeepalive);
                            }
                            Err(notice) => self.close(now, Some(notice), true, &mut effects),
                        }
                    }
                    Ok(Message::Keepalive) if self.state == State::OpenConfirm => {
                        self.state = State::Established;
                        self.hold(now);
                        effects.push(Effect::Established);
                    }
                    Ok(Message::Keepalive) if self.state == State::Established => self.hold(now),
                    Ok(Message::Update(update)) if self.state == State::Established => {
                        match super::received::capture(
                            self.config,
                            self.negotiated.expect("Established owns negotiated OPEN"),
                            update,
                            frame.bytes(),
                        ) {
                            Ok(update) => {
                                self.hold(now);
                                effects.push(Effect::Update(update));
                            }
                            Err(notice) => self.close(now, Some(notice), true, &mut effects),
                        }
                    }
                    Ok(Message::RouteRefresh { afi, safi, .. })
                        if self.state == State::Established =>
                    {
                        if self.negotiated.is_some_and(|peer| peer.receive_refresh) {
                            if safi == 1
                                && (afi == 1 && self.config.ipv4_unicast
                                    || afi == 2 && self.config.ipv6_unicast)
                            {
                                effects.push(Effect::Refresh { afi, safi });
                            }
                        } else {
                            self.close(
                                now,
                                Some(Notice {
                                    code: 1,
                                    subcode: 3,
                                    data: &[5],
                                }),
                                true,
                                &mut effects,
                            );
                        }
                    }
                    Err(_) => {
                        let notice = if frame.header().kind() == Kind::Open {
                            Notice::simple(2, 0)
                        } else {
                            Notice::simple(3, 1)
                        };
                        self.close(now, Some(notice), true, &mut effects);
                    }
                    _ => self.unexpected(now, frame.header().kind(), &mut effects),
                }
            }
            Input::Sent(kind) => self.sent(now, kind),
            Input::Timer => {
                if self.hold_at.is_some_and(|deadline| deadline <= now) {
                    self.close(now, Some(Notice::simple(4, 0)), true, &mut effects);
                } else if self.retry_at.is_some_and(|deadline| deadline <= now) {
                    if self.state == State::Connect {
                        effects.push(Effect::Close {
                            notice: None,
                            withdraw_routes: false,
                            send_extended: false,
                        });
                    }
                    self.start(now, &mut effects);
                } else if self.keepalive_at.is_some_and(|deadline| deadline <= now) {
                    self.keepalive_at = None;
                    effects.push(Effect::SendKeepalive);
                }
            }
            _ => {}
        }
        effects
    }
}
