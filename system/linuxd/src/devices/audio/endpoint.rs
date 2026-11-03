//! ALSA 卡的两个 PCM 端点与共享端点的子流数；纯 UAPI 事实，宿主测试与 linuxd 共用。

/// 共享端点的子流数量，即 `snd_pcm_info.subdevices_count`；与 audio-core 每个输出的渲染流数一致。
pub(crate) const SUBSTREAMS: usize = 8;

/// 一张卡的两个 PCM 端点。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PcmEndpoint {
    Shared,
    Direct,
}

impl PcmEndpoint {
    pub(crate) const fn name(self) -> &'static [u8] {
        match self {
            Self::Shared => b"LiteOS Playback",
            Self::Direct => b"LiteOS Native (Exclusive)",
        }
    }
    pub(crate) const fn device(self) -> u32 {
        match self {
            Self::Shared => 0,
            Self::Direct => 1,
        }
    }
}
