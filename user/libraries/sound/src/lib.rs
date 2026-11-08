//! 系统提示音：读取 PCM WAV、重采样到设备固定的 48 kHz 立体声，并写入 ALSA 播放子流。
//!
//! 播放在后台线程完成，调用者不被阻塞；设备不存在、没有权限或正忙时静默放弃（提示音不是必需功能）。
extern crate liteos_runtime as _;
use linux_uapi::alsa::{CHANNELS, PERIOD_FRAMES, PlaybackPcm, RATE};
use std::{
    io,
    path::Path,
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    thread::{self, JoinHandle},
};

/// 提示音文件所在目录。
pub const DIRECTORY: &str = "/usr/share/liteos/sounds";
/// 欠载后最多重新开始的次数。
const MAX_RECOVERIES: u32 = 32;
/// 预填的 period 数；与 PCM 环形缓冲的 1024 帧一致。
const PREFILL_PERIODS: usize = 4;

/// 设备固定的采样率（Hz），[`Transport::position`] 与 [`Sound::frames`] 的单位都是这个速率下的帧。
pub const SAMPLE_RATE: u32 = RATE;

/// 一次可中断播放的共享状态：播放线程每个 period 更新位置并检查停止请求，界面线程读取位置、请求停止。
#[derive(Default)]
pub struct Transport {
    stop: AtomicBool,
    position: AtomicUsize,
}

impl Transport {
    /// 请求播放尽快停止（最多延迟一个 period）。
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// 已送入设备的帧数位置。
    pub fn position(&self) -> usize {
        self.position.load(Ordering::Relaxed)
    }
}

/// 已解码并重采样到设备格式的音频。
pub struct Sound {
    frames: Vec<[f32; CHANNELS]>,
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// WAVE_FORMAT_PCM 与 WAVE_FORMAT_ADPCM（Microsoft ADPCM）的格式标签。
const FORMAT_PCM: u16 = 1;
const FORMAT_MS_ADPCM: u16 = 2;
/// MS ADPCM 的步长自适应表（按 4 位编码值索引），与编码器的规范表一致。
const ADPCM_ADAPTATION: [i32; 16] = [
    230, 230, 230, 230, 307, 409, 512, 614, 768, 614, 512, 409, 307, 230, 230, 230,
];

/// 解码 Microsoft ADPCM（`WAVE_FORMAT_ADPCM`）数据为 [-1, 1) 的样本帧。
///
/// # Parameters
///
/// - `format`: 完整的 `fmt ` 块：WAVEFORMATEX 后接 wSamplesPerBlock、wNumCoef 与预测系数对。
/// - `data`: `data` 块，按 nBlockAlign 分块；最后一块可以较短。
/// - `channels`: 1 或 2。
///
/// # Errors
///
/// 扩展字段缺失、系数表为空、块对齐与声道数不符或块头引用了不存在的预测系数。
fn decode_ms_adpcm(
    format: &[u8],
    data: &[u8],
    channels: usize,
) -> io::Result<Vec<[f32; CHANNELS]>> {
    let word = |i: usize| -> io::Result<u16> {
        format
            .get(i..i + 2)
            .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
            .ok_or_else(|| invalid("ADPCM fmt chunk is too short"))
    };
    let block_align = usize::from(word(12)?);
    let samples_per_block = usize::from(word(18)?);
    let coefficients: Vec<(i32, i32)> = (0..usize::from(word(20)?))
        .map(|index| {
            let at = 22 + index * 4;
            Ok((i32::from(word(at)? as i16), i32::from(word(at + 2)? as i16)))
        })
        .collect::<io::Result<_>>()?;
    let header = 7 * channels;
    if coefficients.is_empty() || block_align <= header || samples_per_block < 2 {
        return Err(invalid("malformed Microsoft ADPCM format"));
    }
    let mut frames = Vec::with_capacity(data.len() / block_align * samples_per_block);
    for block in data
        .chunks(block_align)
        .filter(|block| block.len() >= header)
    {
        // 1. 块头按字段交错：各声道的预测器序号（1 字节），再是初始步长、sample1、sample2（各 2 字节）。
        let mut state = [(0i32, 0i32, 0i32, 0i32, 0i32); 2];
        for (channel, slot) in state.iter_mut().enumerate().take(channels) {
            let field = |index: usize| {
                let at = channels + (index * channels + channel) * 2;
                i32::from(i16::from_le_bytes([block[at], block[at + 1]]))
            };
            let (coefficient1, coefficient2) = *coefficients
                .get(usize::from(block[channel]))
                .ok_or_else(|| invalid("ADPCM block uses an unknown predictor"))?;
            *slot = (coefficient1, coefficient2, field(0), field(1), field(2));
        }
        let sample = |value: i32| value as f32 / 32768.0;
        let frame = |values: [i32; 2]| {
            let left = sample(values[0]);
            [
                left,
                if channels == 2 {
                    sample(values[1])
                } else {
                    left
                },
            ]
        };
        // 2. 块的前两个输出样本是头里的 sample2、sample1。
        frames.push(frame([state[0].4, state[1].4]));
        frames.push(frame([state[0].3, state[1].3]));
        // 3. 其余每个 4 位编码值：线性预测加步长乘编码值，饱和到 16 位，再按自适应表更新步长（下限 16）。
        //    立体声的每个字节高 4 位属于左声道、低 4 位属于右声道；单声道先高后低。
        let mut decoded = [0i32; 2];
        let mut produced = 2;
        let mut lane = 0;
        for nibble in block[header..]
            .iter()
            .flat_map(|byte| [byte >> 4, byte & 0x0F])
        {
            if produced >= samples_per_block {
                break;
            }
            let (coefficient1, coefficient2, delta, sample1, sample2) = &mut state[lane];
            let predicted = (*sample1 * *coefficient1 + *sample2 * *coefficient2) / 256;
            let signed = i32::from(nibble as i8) - if nibble & 8 != 0 { 16 } else { 0 };
            let value = (predicted + signed * *delta).clamp(-32768, 32767);
            *sample2 = *sample1;
            *sample1 = value;
            *delta = (ADPCM_ADAPTATION[usize::from(nibble)] * *delta / 256).max(16);
            decoded[lane] = value;
            lane += 1;
            if lane == channels {
                frames.push(frame(decoded));
                produced += 1;
                lane = 0;
            }
        }
    }
    Ok(frames)
}

impl Sound {
    /// 解析 RIFF/WAVE：16 位整数 PCM 或 Microsoft ADPCM（单声道或立体声，任意采样率）。
    ///
    /// # Errors
    ///
    /// 其他编码、块长度越界、ADPCM 格式字段损坏或没有数据。
    pub fn decode(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
            return Err(invalid("not a RIFF/WAVE file"));
        }
        let mut format = None;
        let mut data = None;
        // 压缩格式的真实帧数以 fact 块为准；最后一块按 nBlockAlign 补齐的部分不是声音。
        let mut length = None;
        let mut offset = 12;
        while offset + 8 <= bytes.len() {
            let id = &bytes[offset..offset + 4];
            let chunk =
                u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap()) as usize;
            let body = bytes
                .get(offset + 8..offset + 8 + chunk)
                .ok_or_else(|| invalid("chunk runs past the end of the file"))?;
            match id {
                b"fmt " if body.len() >= 16 => format = Some(body),
                b"data" => data = Some(body),
                b"fact" if body.len() >= 4 => {
                    length = Some(u32::from_le_bytes([body[0], body[1], body[2], body[3]]) as usize)
                }
                _ => {}
            }
            offset += 8 + chunk + (chunk & 1);
        }
        let format = format.ok_or_else(|| invalid("missing fmt chunk"))?;
        let data = data.ok_or_else(|| invalid("missing data chunk"))?;
        let word = |i: usize| u16::from_le_bytes([format[i], format[i + 1]]);
        let (tag, channels, bits) = (word(0), word(2), word(14));
        let rate = u32::from_le_bytes([format[4], format[5], format[6], format[7]]);
        if !(1..=2).contains(&channels) || rate == 0 {
            return Err(invalid("only mono or stereo audio is supported"));
        }
        let source: Vec<[f32; CHANNELS]> = match (tag, bits) {
            (FORMAT_PCM, 16) => {
                let stride = usize::from(channels) * 2;
                data.chunks_exact(stride)
                    .map(|frame| {
                        let sample = |i: usize| {
                            f32::from(i16::from_le_bytes([frame[i], frame[i + 1]])) / 32768.0
                        };
                        let left = sample(0);
                        [left, if channels == 2 { sample(2) } else { left }]
                    })
                    .collect()
            }
            (FORMAT_MS_ADPCM, 4) => {
                let mut frames = decode_ms_adpcm(format, data, usize::from(channels))?;
                if let Some(length) = length {
                    frames.truncate(length);
                }
                frames
            }
            _ => {
                return Err(invalid(
                    "only 16-bit PCM or Microsoft ADPCM audio is supported",
                ));
            }
        };
        if source.is_empty() {
            return Err(invalid("no audio data"));
        }
        Ok(Self {
            frames: resample(&source, rate),
        })
    }

    pub fn load(path: &Path) -> io::Result<Self> {
        Self::decode(&std::fs::read(path)?)
    }

    pub fn frames(&self) -> usize {
        self.frames.len()
    }

    /// 把整段音频压成 `columns` 个振幅峰值（0..=1，左右声道取较大者），用于画波形。
    pub fn peaks(&self, columns: usize) -> Vec<f32> {
        if columns == 0 || self.frames.is_empty() {
            return Vec::new();
        }
        (0..columns)
            .map(|column| {
                let start = column * self.frames.len() / columns;
                let end = ((column + 1) * self.frames.len() / columns).max(start + 1);
                self.frames[start..end.min(self.frames.len())]
                    .iter()
                    .flat_map(|frame| frame.iter())
                    .fold(0.0f32, |peak, sample| peak.max(sample.abs()))
            })
            .collect()
    }

    /// 同步播放到结束。
    ///
    /// 播放中设备欠载（`EPIPE`，宿主音频后端成批消费 period 时可能发生）时重新 prepare 并从稍早处续播，
    /// 最多 [`MAX_RECOVERIES`] 次：声音可能有一点重复或停顿，但不会整段放弃。
    ///
    /// # Errors
    ///
    /// 设备打不开、无权限、被占用、断开，或欠载恢复次数用尽。
    pub fn play_blocking(&self) -> io::Result<()> {
        self.play_from(0, &Transport::default())
    }

    /// 从第 `start` 帧开始同步播放，直到结束或 `transport` 收到停止请求；`transport` 同步位置。
    ///
    /// # Errors
    ///
    /// 同 [`Self::play_blocking`]。
    pub fn play_from(&self, start: usize, transport: &Transport) -> io::Result<()> {
        const EPIPE: i32 = 32;
        if transport.stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        let mut pcm = PlaybackPcm::open()?;
        // 声音写完后再多送一个缓冲的静音，让最后的样本播放完毕才关闭设备。
        let end = self.frames.len().div_ceil(PERIOD_FRAMES) * PERIOD_FRAMES
            + PREFILL_PERIODS * PERIOD_FRAMES;
        let mut position = start / PERIOD_FRAMES * PERIOD_FRAMES;
        transport.position.store(position, Ordering::Relaxed);
        let mut recoveries = 0;
        loop {
            match self.stream(&mut pcm, &mut position, end, transport) {
                Ok(()) => break,
                Err(error)
                    if error.raw_os_error() == Some(EPIPE) && recoveries < MAX_RECOVERIES =>
                {
                    recoveries += 1;
                    // 在途的几个 period 没有播完就丢了；回退这么多重新送。
                    position = position.saturating_sub(PREFILL_PERIODS * PERIOD_FRAMES);
                }
                Err(error) => return Err(error),
            }
        }
        if recoveries > 0 {
            eprintln!("sound: recovered from {recoveries} playback underruns");
        }
        Ok(())
    }

    /// 从 `position` 开始送到 `end`（都以帧计、按 period 对齐）：prepare、预填、启动、逐 period 续写。
    fn stream(
        &self,
        pcm: &mut PlaybackPcm,
        position: &mut usize,
        end: usize,
        transport: &Transport,
    ) -> io::Result<()> {
        if transport.stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        pcm.prepare()?;
        for _ in 0..PREFILL_PERIODS {
            if transport.stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            pcm.write_period(&self.period_at(*position))?;
            *position += PERIOD_FRAMES;
        }
        if transport.stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        pcm.start()?;
        while *position < end && !transport.stop.load(Ordering::Relaxed) {
            pcm.wait_period()?;
            pcm.write_period(&self.period_at(*position))?;
            *position += PERIOD_FRAMES;
            transport.position.store(*position, Ordering::Relaxed);
        }
        Ok(())
    }

    /// 第 `position` 帧起的一个 period；越过末尾的部分是静音。
    fn period_at(&self, position: usize) -> [[f32; CHANNELS]; PERIOD_FRAMES] {
        let mut period = [[0.0; CHANNELS]; PERIOD_FRAMES];
        if let Some(rest) = self.frames.get(position..) {
            let available = rest.len().min(PERIOD_FRAMES);
            period[..available].copy_from_slice(&rest[..available]);
        }
        period
    }
}

/// 线性插值重采样到设备的 48 kHz；采样率已匹配时原样返回。
fn resample(source: &[[f32; CHANNELS]], rate: u32) -> Vec<[f32; CHANNELS]> {
    if rate == RATE {
        return source.to_vec();
    }
    let length = (source.len() as u64 * u64::from(RATE) / u64::from(rate)) as usize;
    (0..length)
        .map(|index| {
            let position = index as f64 * f64::from(rate) / f64::from(RATE);
            let base = position as usize;
            let fraction = (position - base as f64) as f32;
            let a = source[base.min(source.len() - 1)];
            let b = source[(base + 1).min(source.len() - 1)];
            [
                a[0] + (b[0] - a[0]) * fraction,
                a[1] + (b[1] - a[1]) * fraction,
            ]
        })
        .collect()
}

/// Windows 2000 默认声音方案里有声音的系统事件（HIVEDEF.INF 的 AppEvents 与 ieuinit.inf 的 Explorer\Navigating）。
///
/// 事件到声音文件的对应只在这里定义；控制面板“声音”页与各播放点共用，不在调用处写文件名。
/// 默认方案里没有声音的事件（最大化、最小化、打开/关闭程序、菜单等）不在此列。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// 默认声音：MessageBeep(MB_OK)、无图标的消息框。
    Default,
    /// 严重停止信息：MB_ICONHAND 消息框。
    CriticalStop,
    /// 提示信息：MB_ICONEXCLAMATION 消息框。
    Exclamation,
    /// 警告信息：MB_ICONASTERISK 消息框。
    Asterisk,
    /// 疑问信息：MB_ICONQUESTION 消息框。
    Question,
    /// 启动 Windows：登录完成。
    Logon,
    /// 退出 Windows：注销。
    Logoff,
    EmptyRecycleBin,
    /// 资源管理器“启动导航”：打开文件夹、前进后退与地址栏跳转。
    Navigating,
}

impl Event {
    /// 控制面板列出的全部事件，按原版“声音”页的顺序。
    pub const ALL: [Self; 9] = [
        Self::Default,
        Self::CriticalStop,
        Self::Exclamation,
        Self::Asterisk,
        Self::Question,
        Self::Logon,
        Self::Logoff,
        Self::EmptyRecycleBin,
        Self::Navigating,
    ];

    /// 事件名（HIVEDEF.INF/ieuinit.inf 的 [Strings]，产品名换成 LiteOS）。
    pub const fn label(self) -> &'static str {
        match self {
            Self::Default => "默认声音",
            Self::CriticalStop => "严重停止信息",
            Self::Exclamation => "提示信息",
            Self::Asterisk => "警告信息",
            Self::Question => "疑问信息",
            Self::Logon => "启动 LiteOS",
            Self::Logoff => "退出 LiteOS",
            Self::EmptyRecycleBin => "清空回收站",
            Self::Navigating => "启动导航",
        }
    }

    /// 该事件在 [`DIRECTORY`] 下的声音文件。
    pub const fn file(self) -> &'static str {
        match self {
            Self::Default | Self::EmptyRecycleBin => "ding.wav",
            Self::CriticalStop | Self::Exclamation | Self::Asterisk | Self::Question => "chord.wav",
            Self::Logon => "logon.wav",
            Self::Logoff => "logoff.wav",
            Self::Navigating => "start.wav",
        }
    }

    /// 异步播放该事件的声音，见 [`play`]。
    pub fn play(self) -> JoinHandle<()> {
        play(self.file())
    }
}

/// 启动独立进程 `/bin/sndplay` 播放 `name`（`DIRECTORY` 下的 WAV 文件名）；返回的句柄可 `join` 以等待播放结束。
///
/// 播放放在独立进程里：用户态是 `panic=abort`，解码或设备出错若发生在调用者进程内会让整个桌面退出。
/// 进程无法启动或播放失败只记一行日志，不向调用者传播；后台线程只负责回收子进程。
pub fn play(name: &str) -> JoinHandle<()> {
    let spawned = Command::new("/bin/sndplay")
        .arg(name)
        .env_remove("WAYLAND_SOCKET")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn();
    thread::spawn(move || match spawned {
        Ok(mut child) => {
            let _ = child.wait();
        }
        Err(error) => eprintln!("sound: cannot start /bin/sndplay: {error}"),
    })
}

#[cfg(test)]
mod tests {
    use super::Sound;

    /// ISO 导入的原版“启动导航”声音：22050 Hz 立体声 Microsoft ADPCM。
    const START: &[u8] = include_bytes!("../../../../assets/classic/sounds/start.wav");

    #[test]
    fn microsoft_adpcm_decodes_to_the_length_in_the_fact_chunk_and_is_audible() {
        let sound = Sound::decode(START).expect("original start.wav decodes");
        // fact 块记录的源帧数，按 22050 → 48000 重采样后的帧数。
        let fact = START
            .windows(4)
            .position(|window| window == b"fact")
            .map(|at| u32::from_le_bytes(START[at + 8..at + 12].try_into().unwrap()))
            .expect("ADPCM file carries a fact chunk");
        let expected = u64::from(fact) * u64::from(super::SAMPLE_RATE) / 22050;
        assert!(
            (sound.frames() as u64).abs_diff(expected) <= 2,
            "{} frames, fact says {expected}",
            sound.frames()
        );
        let peak = sound.peaks(1)[0];
        assert!(
            (0.05..1.0).contains(&peak),
            "decoded click must be audible and not clipped noise, peak {peak}"
        );
    }
}
