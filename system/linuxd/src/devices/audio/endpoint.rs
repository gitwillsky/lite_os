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

/// 按 Linux control 偏好选择子流：-1 选择首个空闲槽，其余值只接受精确编号；独占端点要求整卡空闲且不指定编号，
/// 两种端点不能同时占用一张卡。
///
/// # Parameters
///
/// - `occupied`: 各子流是否已被 OFD 占用。
/// - `current`: 已占用子流所属的端点（无占用时无意义）。
///
/// # Returns
///
/// 选中的子流编号；不能接纳时为 `None`。
pub(crate) fn choose_substream(
    occupied: [bool; SUBSTREAMS],
    current: PcmEndpoint,
    endpoint: PcmEndpoint,
    preferred: i32,
) -> Option<usize> {
    let any = occupied.contains(&true);
    if (endpoint == PcmEndpoint::Direct && (any || preferred > 0)) || (any && current != endpoint) {
        return None;
    }
    (0..SUBSTREAMS)
        .find(|&index| !occupied[index] && (preferred == -1 || preferred == index as i32))
}

/// `endpoint` 上仍可打开的子流数（`subdevices_avail`）：另一端点占用时为零，独占端点只在整卡空闲时为一。
pub(crate) fn endpoint_available(
    occupied: [bool; SUBSTREAMS],
    current: PcmEndpoint,
    endpoint: PcmEndpoint,
) -> usize {
    let free = occupied.iter().filter(|busy| !**busy).count();
    if free != SUBSTREAMS && current != endpoint {
        0
    } else if endpoint == PcmEndpoint::Direct {
        usize::from(free == SUBSTREAMS)
    } else {
        free
    }
}
