//! TCP 运行时事实的原生布局，Linux ABI 的字段和状态编号由其消费者转换。

use crate::{Reader, Status, Writer, native_abi};

/// TCP 连接阶段，序号属于原生控制消息。
#[derive(Clone, Copy, Default)]
#[repr(u8)]
pub enum TcpPhase {
    #[default]
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

impl TcpPhase {
    const VALUES: [Self; 11] = [
        Self::Closed,
        Self::Listen,
        Self::SynSent,
        Self::SynReceived,
        Self::Established,
        Self::FinWait1,
        Self::FinWait2,
        Self::CloseWait,
        Self::Closing,
        Self::LastAck,
        Self::TimeWait,
    ];
}

/// 一次查询所得的 TCP 发送统计。
#[derive(Default)]
pub struct TcpInformation {
    pub phase: TcpPhase,
    pub timeout_retries: u8,
    pub sack: bool,
    pub timestamps: bool,
    /// 对端及本地窗口移位，无协商时缺席。
    pub window_scale: Option<[u8; 2]>,
    pub rto_us: u32,
    pub rtt_us: u32,
    pub rtt_variance_us: u32,
    pub send_mss: u32,
    pub advertised_mss: u32,
    pub congestion_window: u32,
    /// 以段为单位；u32::MAX 表示尚无限制。
    pub slow_start_threshold: u32,
    pub retransmitted_segments: u32,
    pub accept_ready: u32,
    pub accept_limit: u32,
}

impl TcpInformation {
    pub(crate) const ORDINAL: u32 = 25;
    pub(crate) const VERSION: u32 = 1;
    pub(crate) const WIRE_BYTES: usize = 6 + 10 * 4;

    pub(crate) fn write(&self, out: &mut Writer) {
        out.u8(self.phase as u8)
            .u8(self.timeout_retries)
            .bool(self.sack)
            .bool(self.timestamps);
        let [send, receive] = self.window_scale.unwrap_or([u8::MAX; 2]);
        out.u8(send).u8(receive);
        for field in [
            self.rto_us,
            self.rtt_us,
            self.rtt_variance_us,
            self.send_mss,
            self.advertised_mss,
            self.congestion_window,
            self.slow_start_threshold,
            self.retransmitted_segments,
            self.accept_ready,
            self.accept_limit,
        ] {
            out.u32(field);
        }
    }

    pub(crate) fn read(input: &mut Reader<'_>) -> Result<Self, Status> {
        let phase = *TcpPhase::VALUES
            .get(usize::from(input.u8()?))
            .ok_or(native_abi::ERR_INVALID_ARGS)?;
        let timeout_retries = input.u8()?;
        let sack = input.bool()?;
        let timestamps = input.bool()?;
        let window_scale = match [input.u8()?, input.u8()?] {
            [u8::MAX, u8::MAX] => None,
            scales @ [0..=14, 0..=14] => Some(scales),
            _ => return Err(native_abi::ERR_INVALID_ARGS),
        };
        Ok(Self {
            phase,
            timeout_retries,
            sack,
            timestamps,
            window_scale,
            rto_us: input.u32()?,
            rtt_us: input.u32()?,
            rtt_variance_us: input.u32()?,
            send_mss: input.u32()?,
            advertised_mss: input.u32()?,
            congestion_window: input.u32()?,
            slow_start_threshold: input.u32()?,
            retransmitted_segments: input.u32()?,
            accept_ready: input.u32()?,
            accept_limit: input.u32()?,
        })
    }
}
