// Heads up! Before working on this file you should read, at least, RFC 793 and
// the parts of RFC 1122 that discuss TCP, as well as RFC 7323 for some of the TCP options.
// Consult RFC 7414 when implementing a new feature.

use core::fmt::Display;

use core::{fmt, mem};

use crate::time::{Duration, Instant};
use crate::{
    Authentication, Control as TcpControl, IpPacket as IpRepr, Segment as TcpRepr,
    Sequence as TcpSeqNumber, Timestamp as TcpTimestampRepr,
    TimestampGenerator as TcpTimestampGenerator,
};
use crate::{Context, Endpoint as IpEndpoint, ListenEndpoint as IpListenEndpoint, PollAt};
use crate::{reassembly::Assembler, stream_buffer::RingBuffer};
use core::net::IpAddr as IpAddress;
const TCP_HEADER_LEN: usize = 20;

mod ack;
mod connection;
mod options;
mod output;
mod receive;
mod stream;
mod timers;

/// Error returned by [`Socket::listen`]
#[derive(Debug, PartialEq, Eq, Clone, Copy)]

pub enum ListenError {
    InvalidState,
    Unaddressable,
}

impl Display for ListenError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            ListenError::InvalidState => write!(f, "invalid state"),
            ListenError::Unaddressable => write!(f, "unaddressable destination"),
        }
    }
}

/// Error returned by [`Socket::connect`]
#[derive(Debug, PartialEq, Eq, Clone, Copy)]

pub enum ConnectError {
    InvalidState,
    Unaddressable,
}

impl Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            ConnectError::InvalidState => write!(f, "invalid state"),
            ConnectError::Unaddressable => write!(f, "unaddressable destination"),
        }
    }
}

/// Error returned by [`Socket::send`]
#[derive(Debug, PartialEq, Eq, Clone, Copy)]

pub enum SendError {
    InvalidState,
}

impl Display for SendError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            SendError::InvalidState => write!(f, "invalid state"),
        }
    }
}

/// Error returned by [`Socket::recv`]
#[derive(Debug, PartialEq, Eq, Clone, Copy)]

pub enum RecvError {
    InvalidState,
    Finished,
}

impl Display for RecvError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            RecvError::InvalidState => write!(f, "invalid state"),
            RecvError::Finished => write!(f, "operation finished"),
        }
    }
}

/// A TCP socket ring buffer.
pub type SocketBuffer<'a> = RingBuffer<'a, u8>;

/// The state of a TCP socket, according to [RFC 793].
///
/// [RFC 793]: https://tools.ietf.org/html/rfc793
#[derive(Debug, PartialEq, Eq, Clone, Copy)]

pub enum State {
    Closed,
    Listen,
    SynSent,
    SynReceived,
    Established,
    FinWait1,
    FinWait2,
    CloseWait,
    Closing,
    LastAck,
    TimeWait,
}

/// 实际transport终止原因；facade只在首次Closed transition消费，不推测deadline或复制timer。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]

pub enum FailureReason {
    Reset,
    TimedOut,
}

/// 本次实际输出失败的性质；协议计时只区分本地资源拥塞和不可达路径。
/// 路由/邻居失败消耗探测尝试，本地队列拥塞只延后，不据此判定 peer 死亡。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFailure {
    PathUnavailable,
    LocalCongestion,
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            State::Closed => write!(f, "CLOSED"),
            State::Listen => write!(f, "LISTEN"),
            State::SynSent => write!(f, "SYN-SENT"),
            State::SynReceived => write!(f, "SYN-RECEIVED"),
            State::Established => write!(f, "ESTABLISHED"),
            State::FinWait1 => write!(f, "FIN-WAIT-1"),
            State::FinWait2 => write!(f, "FIN-WAIT-2"),
            State::CloseWait => write!(f, "CLOSE-WAIT"),
            State::Closing => write!(f, "CLOSING"),
            State::LastAck => write!(f, "LAST-ACK"),
            State::TimeWait => write!(f, "TIME-WAIT"),
        }
    }
}

/// RFC 6298: (2.1) Until a round-trip time (RTT) measurement has been made for a
/// segment sent between the sender and receiver, the sender SHOULD
/// set RTO <- 1 second,
const RTTE_INITIAL_RTO: u32 = 1000;

// Minimum "safety margin" for the RTO that kicks in when the
// variance gets very low.
const RTTE_MIN_MARGIN: u32 = 5;

/// K, according to RFC 6298
const RTTE_K: u32 = 4;

// 固定Linux TCP_RTO_MIN=HZ/5；初次SYN仍使用RFC6298的1秒，不混同后续样本下限。
const RTTE_MIN_RTO: u32 = 200;

// RFC 6298 (2.5) A maximum value MAY be placed on RTO provided it is at least 60
// seconds
const RTTE_MAX_RTO: u32 = 120_000;

#[derive(Debug, Clone, Copy)]

struct RttEstimator {
    /// true if we have made at least one rtt measurement.
    have_measurement: bool,
    // Using u32 instead of Duration to save space (Duration is i64)
    /// Smoothed RTT
    srtt: u32,
    /// RTT variance.
    rttvar: u32,
    /// Retransmission Time-Out
    rto: u32,
    timestamp: Option<(Instant, TcpSeqNumber)>,
    max_seq_sent: Option<TcpSeqNumber>,
    rto_count: u8,
}

impl Default for RttEstimator {
    fn default() -> Self {
        Self {
            have_measurement: false,
            srtt: 0,   // ignored, will be overwritten on first measurement.
            rttvar: 0, // ignored, will be overwritten on first measurement.
            rto: RTTE_INITIAL_RTO,
            timestamp: None,
            max_seq_sent: None,
            rto_count: 0,
        }
    }
}

impl RttEstimator {
    /// 完成握手时只seed首个data RTO，不伪造或重写真实RTT样本。
    fn initialize_route(&mut self, rtt: u32) {
        if rtt > self.srtt {
            self.rto = rtt
                .saturating_add(rtt.saturating_mul(2).max(RTTE_MIN_RTO))
                .clamp(RTTE_MIN_RTO, RTTE_MAX_RTO);
        } else if !self.have_measurement {
            self.rto = 3000;
        }
    }
    fn retransmission_timeout(&self) -> Duration {
        Duration::from_millis(self.rto as _)
    }

    fn sample(&mut self, new_rtt: u32) {
        if self.have_measurement {
            // RFC 6298 (2.3) When a subsequent RTT measurement R' is made, a host MUST set (...)
            let diff = (self.srtt as i32 - new_rtt as i32).unsigned_abs();
            self.rttvar = (self.rttvar * 3 + diff).div_ceil(4);
            self.srtt = (self.srtt * 7 + new_rtt).div_ceil(8);
        } else {
            // RFC 6298 (2.2) When the first RTT measurement R is made, the host MUST set (...)
            self.have_measurement = true;
            self.srtt = new_rtt;
            self.rttvar = new_rtt / 2;
        }

        // RFC 6298 (2.2), (2.3)
        let margin = RTTE_MIN_MARGIN.max(self.rttvar * RTTE_K);
        self.rto = (self.srtt + margin).clamp(RTTE_MIN_RTO, RTTE_MAX_RTO);

        self.rto_count = 0;

        net_trace!(
            "rtte: sample={:?} srtt={:?} rttvar={:?} rto={:?}",
            new_rtt,
            self.srtt,
            self.rttvar,
            self.rto
        );
    }

    fn on_send(&mut self, timestamp: Instant, seq: TcpSeqNumber) {
        if self
            .max_seq_sent
            .map(|max_seq_sent| seq > max_seq_sent)
            .unwrap_or(true)
        {
            self.max_seq_sent = Some(seq);
            if self.timestamp.is_none() {
                self.timestamp = Some((timestamp, seq));
                net_trace!("rtte: sampling at seq={:?}", seq);
            }
        }
    }

    fn on_ack(&mut self, timestamp: Instant, seq: TcpSeqNumber) {
        if let Some((sent_timestamp, sent_seq)) = self.timestamp
            && seq >= sent_seq
        {
            self.sample((timestamp - sent_timestamp).total_millis() as u32);
            self.timestamp = None;
        }
    }

    fn on_retransmit(&mut self, backoff: bool) {
        if self.timestamp.is_some() {
            net_trace!("rtte: abort sampling due to retransmit");
        }
        self.timestamp = None;

        // RFC 6298 (5.5) The host MUST set RTO <- RTO * 2 ("back off the timer").  The
        // maximum value discussed in (2.5) above may be used to provide
        // an upper bound to this doubling operation.
        if backoff {
            self.rto = (self.rto * 2).min(RTTE_MAX_RTO);
        }
        net_trace!("rtte: doubling rto to {:?}", self.rto);

        // RFC 6298: a TCP implementation MAY clear SRTT and RTTVAR after
        // backing off the timer multiple times as it is likely that the current
        // SRTT and RTTVAR are bogus in this situation.  Once SRTT and RTTVAR
        // are cleared, they should be initialized with the next RTT sample
        // taken per (2.2) rather than using (2.3).
        self.rto_count += 1;
        if self.rto_count >= 3 {
            self.rto_count = 0;
            self.have_measurement = false;
            net_trace!("rtte: too many retransmissions, clearing srtt, rttvar.");
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]

enum Timer {
    Idle,
    Retransmit {
        expires_at: Instant,
    },
    FastRetransmit,
    ZeroWindowProbe {
        expires_at: Option<Instant>,
        delay: Duration,
        // clock已安排本次probe；成功/本地拥塞消费intent，无route由下轮时钟继续退避。
        pending: bool,
    },
    Close {
        expires_at: Instant,
    },
}

const ACK_DELAY_DEFAULT: Duration = Duration::from_millis(10);
// 固定Linux TCP_TIMEWAIT_LEN=60秒；不是应用SO_RCVTIMEO/SO_SNDTIMEO。
const CLOSE_DELAY: Duration = Duration::from_millis(60_000);

impl Timer {
    fn new() -> Timer {
        Timer::Idle
    }

    fn should_retransmit(&self, timestamp: Instant) -> bool {
        match *self {
            Timer::Retransmit { expires_at } if timestamp >= expires_at => true,
            Timer::FastRetransmit => true,
            _ => false,
        }
    }

    fn should_close(&self, timestamp: Instant) -> bool {
        matches!(*self, Timer::Close { expires_at } if timestamp >= expires_at)
    }

    fn should_zero_window_probe(&self, timestamp: Instant) -> bool {
        match *self {
            Timer::ZeroWindowProbe { pending: true, .. } => true,
            Timer::ZeroWindowProbe {
                expires_at: Some(expires_at),
                ..
            } if timestamp >= expires_at => true,
            _ => false,
        }
    }

    fn poll_at(&self) -> PollAt {
        match *self {
            Timer::Idle => PollAt::Ingress,
            Timer::ZeroWindowProbe { pending: true, .. } => PollAt::Now,
            Timer::ZeroWindowProbe { expires_at, .. } => {
                expires_at.map_or(PollAt::Now, PollAt::Time)
            }
            Timer::Retransmit { expires_at, .. } => PollAt::Time(expires_at),
            Timer::FastRetransmit => PollAt::Now,
            Timer::Close { expires_at } => PollAt::Time(expires_at),
        }
    }

    fn set_for_idle(&mut self) {
        *self = Timer::Idle;
    }

    fn set_for_retransmit(&mut self, timestamp: Instant, delay: Duration) {
        match *self {
            Timer::Idle
            | Timer::FastRetransmit
            | Timer::Retransmit { .. }
            | Timer::ZeroWindowProbe { .. } => {
                *self = Timer::Retransmit {
                    expires_at: timestamp + delay,
                }
            }
            Timer::Close { .. } => (),
        }
    }

    fn set_for_fast_retransmit(&mut self) {
        *self = Timer::FastRetransmit
    }

    fn set_for_close(&mut self, timestamp: Instant) {
        *self = Timer::Close {
            expires_at: timestamp + CLOSE_DELAY,
        }
    }

    fn set_for_zero_window_probe(&mut self, timestamp: Option<Instant>, delay: Duration) {
        *self = Timer::ZeroWindowProbe {
            expires_at: timestamp.map(|timestamp| timestamp + delay),
            delay,
            pending: false,
        };
    }

    fn probe_deadline(
        timestamp: Instant,
        delay: Duration,
        user_deadline: Option<Instant>,
    ) -> Instant {
        let resource_floor = timestamp + Duration::from_millis(2);
        user_deadline.map_or(timestamp + delay, |deadline| {
            (timestamp + delay).min(deadline.max(resource_floor))
        })
    }

    fn schedule_zero_window_probe(&mut self, timestamp: Instant, user_deadline: Option<Instant>) {
        if let Timer::ZeroWindowProbe {
            mut delay, pending, ..
        } = *self
        {
            // 前一轮无route未消费intent等价native负route错误，不能冻结退避与deadline。
            if pending {
                delay = (delay * 2).min(Duration::from_millis(RTTE_MAX_RTO as _));
            }
            let next = (delay * 2).min(Duration::from_millis(RTTE_MAX_RTO as _));
            *self = Timer::ZeroWindowProbe {
                expires_at: Some(Self::probe_deadline(timestamp, next, user_deadline)),
                delay,
                pending: true,
            };
        }
    }

    fn finish_zero_window_probe(
        &mut self,
        timestamp: Instant,
        delivered: bool,
        user_deadline: Option<Instant>,
    ) {
        if let Timer::ZeroWindowProbe {
            mut delay,
            expires_at,
            ..
        } = *self
        {
            let expires_at = if delivered {
                delay = (delay * 2).min(Duration::from_millis(RTTE_MAX_RTO as _));
                expires_at
            } else {
                Some(Self::probe_deadline(
                    timestamp,
                    Duration::from_millis(500),
                    user_deadline,
                ))
            };
            *self = Timer::ZeroWindowProbe {
                expires_at,
                delay,
                pending: false,
            };
        }
    }

    fn is_idle(&self) -> bool {
        matches!(self, Timer::Idle)
    }

    fn is_zero_window_probe(&self) -> bool {
        matches!(self, Timer::ZeroWindowProbe { .. })
    }

    fn is_retransmit(&self) -> bool {
        matches!(self, Timer::Retransmit { .. } | Timer::FastRetransmit)
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum AckDelayTimer {
    Idle,
    Waiting(Instant),
    Immediate,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]

struct Tuple {
    local: IpEndpoint,
    remote: IpEndpoint,
}

impl Display for Tuple {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.local, self.remote)
    }
}

/// A Transmission Control Protocol socket.
///
/// A TCP socket may passively listen for connections or actively connect to another endpoint.
/// Note that, for listening sockets, there is no "backlog"; to be able to simultaneously
/// accept several connections, as many sockets must be allocated, or any new connection
/// attempts will be reset.
#[derive(Debug)]
pub struct Socket<'a> {
    state: State,
    failure_reason: Option<FailureReason>,
    /// 原生SYNCNT/user timeout policy；pending_since是真实未完成sequence的起点，idle无此期限。
    syn_retries: u8,
    user_timeout: Option<Duration>,
    pending_since: Option<Instant>,
    /// 实际RTO事件次数，不用RTT estimator的Karn样本退避计数冒充native重试预算。
    retransmits: u8,
    /// 累计成功提交的重传段，与控制退避的连续超时次数分别维护。
    total_retransmissions: u32,
    /// PROBE0实际计数与第一探测时刻；窗口0应答只清count，不重置USER_TIMEOUT起点。
    zero_window_probes: u8,
    zero_window_since: Option<Instant>,
    /// 最后application owner退出的真实permission及FIN_WAIT2起点；SHUT_WR仍存活时不计期。
    orphaned: bool,
    fin_wait2_since: Option<Instant>,
    /// TCP_LINGER2：-1立即abort，0用native60秒，正值最多120秒。
    linger2: i32,
    timer: Timer,
    rtte: RttEstimator,
    assembler: Assembler,
    /// Advisory sender SACK history belongs to this connection; only cumulative ACK releases TX bytes.
    sack: crate::loss::Sack,
    rx_buffer: SocketBuffer<'a>,
    rx_fin_received: bool,
    tx_buffer: SocketBuffer<'a>,
    /// Interval after which, if no inbound packets are received, the connection is aborted.
    timeout: Option<Duration>,
    /// Interval at which keep-alive packets will be sent.
    keep_alive: Option<Duration>,
    /// 原生KEEPINTVL/KEEPCNT及实际已成功probe次数；只有入队成功后计数。
    keep_alive_interval: Duration,
    keep_alive_count: u8,
    keep_alive_probes: u8,
    /// 实际最后接收/正常发送活动与独立keepalive deadline，不借重传timer或app timeout。
    keep_alive_at: Option<Instant>,
    activity_at: Option<Instant>,
    /// The time-to-live (IPv4) or hop limit (IPv6) value used in outgoing packets.
    hop_limit: Option<u8>,
    bound_interface: Option<u32>,
    /// Linux user_mss policy; changing it does not rewrite an existing handshake's advertisement.
    user_mss: Option<u16>,
    /// Route/user MSS captured by the actual active open or received SYN, not by accept().
    advertised_mss: Option<usize>,
    /// 实际路由对发送方的MTU上限，独立于对端可接收的广告MSS。
    route_send_mss: usize,
    /// 本次握手冻结的发送user cap；ESTABLISHED setter配置下一次open，PMTU刷新仍保留旧cap。
    sender_mss_limit: usize,
    /// 握手冻结的native窗口clamp；控制实际SYN及后续RX窗口，零等价未配置。
    receive_window: usize,
    /// 握手冻结的RTAX_RTT，只在完成握手时初始化首个data RTO。
    route_initial_rtt: u32,
    /// Native sender mss_cache, retained across disconnect until the next route/handshake update.
    send_mss: usize,
    /// Address passed to listen(). Listen address is set when listen() is called and
    /// used every time the socket is reset back to the LISTEN state.
    listen_endpoint: IpListenEndpoint,
    /// Current 4-tuple (local and remote endpoints).
    tuple: Option<Tuple>,
    /// The sequence number corresponding to the beginning of the transmit buffer.
    /// I.e. an ACK(local_seq_no+n) packet removes n bytes from the transmit buffer.
    local_seq_no: TcpSeqNumber,
    /// The sequence number corresponding to the beginning of the receive buffer.
    /// I.e. userspace reading n bytes adds n to remote_seq_no.
    remote_seq_no: TcpSeqNumber,
    /// 实际 SND.NXT：仅成功提交 SYN、数据或 FIN 时前进，重传不回退。
    /// 与 queued bytes 分开；否则攻击者可以确认从未发送的数据。
    send_seq_no: TcpSeqNumber,
    /// 下一次发送/重传的 cursor；RTO 回退至 SND.UNA，不是 ACK 准入上界。
    remote_last_seq: TcpSeqNumber,
    /// The last acknowledgement number sent.
    /// I.e. in an idle socket, remote_seq_no+rx_buffer.len().
    remote_last_ack: Option<TcpSeqNumber>,
    /// The last window length sent.
    remote_last_win: u16,
    /// The sending window scaling factor advertised to remotes which support RFC 1323.
    /// It is zero if the window <= 64KiB and/or the remote does not support it.
    remote_win_shift: u8,
    /// The remote window size, relative to local_seq_no
    /// I.e. we're allowed to send octets until local_seq_no+remote_win_len
    remote_win_len: usize,
    /// The receive window scaling factor for remotes which support RFC 1323, None if unsupported.
    remote_win_scale: Option<u8>,
    /// Whether or not the remote supports selective ACK as described in RFC 2018.
    remote_has_sack: bool,
    /// Per-operation output format from the caller's selected key; no secret/key table is mirrored here.
    authentication: Authentication,
    /// The maximum number of data octets that the remote side may receive.
    remote_mss: usize,
    /// The timestamp of the last packet received.
    remote_last_ts: Option<Instant>,
    /// The sequence number of the last packet received, used for sACK
    local_rx_last_seq: Option<TcpSeqNumber>,
    /// The ACK number of the last packet received.
    local_rx_last_ack: Option<TcpSeqNumber>,
    /// The number of packets received directly after
    /// each other which have the same ACK number.
    local_rx_dup_acks: u8,

    /// Duration for Delayed ACK. If None no ACKs will be delayed.
    ack_delay: Option<Duration>,
    /// Delayed ack timer. If set, packets containing exclusively
    /// ACK or window updates (ie, no data) won't be sent until expiry.
    ack_delay_timer: AckDelayTimer,

    /// Used for rate-limiting: No more challenge ACKs will be sent until this instant.
    challenge_ack_timer: Instant,

    /// Nagle's Algorithm enabled.
    nagle: bool,

    /// The congestion control algorithm.
    congestion_controller: crate::congestion::Reno,

    /// tsval generator - if some, tcp timestamp is enabled
    tsval_generator: Option<TcpTimestampGenerator>,

    /// 0 if not seen or timestamp not enabled
    last_remote_tsval: u32,
}

const DEFAULT_MSS: usize = 536;

impl<'a> fmt::Write for Socket<'a> {
    fn write_str(&mut self, slice: &str) -> fmt::Result {
        let slice = slice.as_bytes();
        if self.send_slice(slice) == Ok(slice.len()) {
            Ok(())
        } else {
            Err(fmt::Error)
        }
    }
}
