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

impl Sound {
    /// 解析 RIFF/WAVE 的 16 位整数 PCM（单声道或立体声，任意采样率）。
    ///
    /// # Errors
    ///
    /// 不是 PCM16 的 WAV、块长度越界或没有数据。
    pub fn decode(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
            return Err(invalid("not a RIFF/WAVE file"));
        }
        let mut format = None;
        let mut data = None;
        let mut offset = 12;
        while offset + 8 <= bytes.len() {
            let id = &bytes[offset..offset + 4];
            let length =
                u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap()) as usize;
            let body = bytes
                .get(offset + 8..offset + 8 + length)
                .ok_or_else(|| invalid("chunk runs past the end of the file"))?;
            match id {
                b"fmt " if body.len() >= 16 => {
                    let word = |i: usize| u16::from_le_bytes(body[i..i + 2].try_into().unwrap());
                    let rate = u32::from_le_bytes(body[4..8].try_into().unwrap());
                    format = Some((word(0), word(2), rate, word(14)));
                }
                b"data" => data = Some(body),
                _ => {}
            }
            offset += 8 + length + (length & 1);
        }
        let (tag, channels, rate, bits) = format.ok_or_else(|| invalid("missing fmt chunk"))?;
        let data = data.ok_or_else(|| invalid("missing data chunk"))?;
        if tag != 1 || bits != 16 || !(1..=2).contains(&channels) || rate == 0 {
            return Err(invalid("only 16-bit PCM mono or stereo is supported"));
        }
        let stride = usize::from(channels) * 2;
        let source: Vec<[f32; CHANNELS]> = data
            .chunks_exact(stride)
            .map(|frame| {
                let sample =
                    |i: usize| f32::from(i16::from_le_bytes([frame[i], frame[i + 1]])) / 32768.0;
                let left = sample(0);
                [left, if channels == 2 { sample(2) } else { left }]
            })
            .collect();
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
