//! 子流混音数学：把若干 period 的 F32_LE 立体声相加，施加主音量，再限幅成一个硬件 period。
//!
//! 只做算术，不认识子流、设备或锁，因此可以在宿主上逐值测试。内核目标是 softfloat，浮点由编译器内建函数
//! 软件实现；一个 period 只有 512 个样本，每路混入只是一轮加法，成本可以忽略。

use crate::drivers::{PCM_PERIOD_BYTES, PCM_PERIOD_FRAMES};

/// 一个 period 内的样本总数（立体声交错）。
const SAMPLES: usize = PCM_PERIOD_FRAMES * 2;
/// 主音量的最大值；0 静音，100 为单位增益。
pub(super) const VOLUME_MAX: u32 = 100;

/// 把一个 period 的主音量（0..=100）换算成线性增益。
///
/// 使用二次曲线 `(v/100)²`：人耳对响度近似对数，线性映射会让低音量区间的步进过粗。`VOLUME_MAX` 对应
/// 恰好 1.0，所以满音量时混音对未过载的内容完全透明。
///
/// # Parameters
///
/// - `volume`: 0..=100；更大的值按 100 处理。
pub(super) fn volume_gain(volume: u32) -> f32 {
    let linear = volume.min(VOLUME_MAX) as f32 / VOLUME_MAX as f32;
    linear * linear
}

/// 一个 period 的累加器：逐路加入子流数据，最后一次性输出。
pub(super) struct Accumulator {
    sum: [f32; SAMPLES],
}

impl Accumulator {
    /// 全零累加器。
    pub(super) const fn new() -> Self {
        Self {
            sum: [0.0; SAMPLES],
        }
    }

    /// 把一路 period 加入累加器。
    ///
    /// # Parameters
    ///
    /// - `period`: 256 帧、立体声、F32_LE 交错的原始字节。
    ///
    /// 不是有限数的样本（NaN 与无穷）按静音处理：这些字节来自用户态，一个恶意或出错的客户端不能让
    /// 其他子流的声音随之失真。
    pub(super) fn add(&mut self, period: &[u8; PCM_PERIOD_BYTES]) {
        for (sum, bytes) in self.sum.iter_mut().zip(period.as_chunks::<4>().0) {
            let sample = f32::from_le_bytes(*bytes);
            if sample.is_finite() {
                *sum += sample;
            }
        }
    }

    /// 施加每声道增益并限幅到 [-1, 1]，写出最终的硬件 period。
    ///
    /// # Parameters
    ///
    /// - `gain`: 左、右声道的线性增益。
    /// - `output`: 接收 F32_LE 字节。
    ///
    /// 限幅是无状态的硬削波：峰值不超过 1.0 的混音完全透明；超过时才会削波。没有前瞻限幅器，这是有意的
    /// 简化——它需要跨 period 的状态与 128 帧延迟线，等有真实的过载场景再引入。
    pub(super) fn finish(&self, gain: [f32; 2], output: &mut [u8; PCM_PERIOD_BYTES]) {
        for (index, (sum, bytes)) in self
            .sum
            .iter()
            .zip(output.as_chunks_mut::<4>().0.iter_mut())
            .enumerate()
        {
            let scaled = (*sum * gain[index % 2]).clamp(-1.0, 1.0);
            *bytes = scaled.to_le_bytes();
        }
    }
}
