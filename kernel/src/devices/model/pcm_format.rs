//! PCM 样本布局与设备能力；不含 ALSA ioctl 或具体总线编码。

/// 可无损传递的 little-endian PCM 存储格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum PcmFormat {
    S16,
    S24Packed,
    S24,
    S32,
    Float,
}

impl PcmFormat {
    pub(crate) const ALL: [Self; 5] = [
        Self::S16,
        Self::S24Packed,
        Self::S24,
        Self::S32,
        Self::Float,
    ];

    /// 一个样本占用的字节数；S24 在 32-bit 容器内低位对齐。
    pub(crate) const fn bytes(self) -> usize {
        match self {
            Self::S16 => 2,
            Self::S24Packed => 3,
            Self::S24 | Self::S32 | Self::Float => 4,
        }
    }

    pub(crate) const fn bits(self) -> u32 {
        match self {
            Self::S16 => 16,
            Self::S24Packed | Self::S24 => 24,
            Self::S32 | Self::Float => 32,
        }
    }
}

/// PCM 已配置的物理输出格式；period 始终按帧计数，不假定每帧 8 字节。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PcmConfiguration {
    pub(crate) rate: u32,
    pub(crate) format: PcmFormat,
    pub(crate) period_frames: usize,
}

impl PcmConfiguration {
    /// 桌面共享混音使用的正规形。
    pub(crate) const SHARED: Self = Self {
        rate: 48_000,
        format: PcmFormat::Float,
        period_frames: 256,
    };

    pub(crate) const fn frame_bytes(self) -> usize {
        self.format.bytes() * 2
    }

    pub(crate) const fn period_bytes(self) -> usize {
        self.period_frames * self.frame_bytes()
    }
}

/// 可枚举的 PCM 采样率集合；位序只属于通用设备 seam，adapter 必须显式转换总线编码。
pub(crate) const PCM_RATES: [u32; 14] = [
    5_512, 8_000, 11_025, 16_000, 22_050, 32_000, 44_100, 48_000, 64_000, 88_200, 96_000, 176_400,
    192_000, 384_000,
];

/// 设备实际公布的 PCM 能力；未知/未实现的格式不能作为支持位返回。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PcmCapabilities {
    /// 每个 PcmFormat 的采样率集合；USB alternate settings 不能被误报为格式 × rate 的笛卡尔积。
    pub(crate) rates: [u32; 5],
}

impl PcmCapabilities {
    pub(crate) const EMPTY: Self = Self { rates: [0; 5] };
    pub(crate) const SHARED: Self = Self {
        rates: [0, 0, 0, 0, 1 << 7],
    };

    pub(crate) fn supports(self, configuration: PcmConfiguration) -> bool {
        PCM_RATES.iter().enumerate().any(|(index, rate)| {
            *rate == configuration.rate
                && self.rates[configuration.format as usize] & (1 << index) != 0
        })
    }
}

/// 384 kHz 的 5ms stereo S32 period 上限；DMA 与热路径 scratch 在配置前预留，不逐周期分配。
pub(crate) const MAX_PCM_PERIOD_BYTES: usize = 1_920 * 8;
