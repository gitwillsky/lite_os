//! evdev → 语义输入事件：指针位置（绝对与相对设备共用一个指针）、按钮与按键。

use std::{fs, io, path::Path};

use linux_uapi::input::{AbsoluteAxis, AbsoluteRange, InputDevice, InputEvent};

const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_REL: u16 = 2;
const EV_ABS: u16 = 3;
const SYN_REPORT: u16 = 0;
const AXIS_X: u16 = 0;
const AXIS_Y: u16 = 1;
const REL_HWHEEL: u16 = 6;
const REL_WHEEL: u16 = 8;
const REL_WHEEL_HI_RES: u16 = 11;
const REL_HWHEEL_HI_RES: u16 = 12;
/// 滚轮一个档位的 hi-res 单位数（Linux `REL_WHEEL_HI_RES` 约定）。
const WHEEL_NOTCH: i32 = 120;
const BUTTON_FIRST: u16 = 0x110;
const BUTTON_LAST: u16 = 0x117;

/// 一个已解码、与具体设备无关的输入事件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    PointerMoved {
        x: i32,
        y: i32,
    },
    /// 鼠标按钮，`code` 是 Linux `BTN_*`。
    Button {
        code: u16,
        pressed: bool,
    },
    /// 键盘键，`code` 是 Linux `KEY_*`；自动重复由客户端自己负责，这里不产生。
    Key {
        code: u16,
        pressed: bool,
    },
    /// 滚轮档位：`vertical > 0` 向下滚，`horizontal > 0` 向右滚（Wayland 轴方向，与 evdev 的垂直方向相反）。
    Wheel {
        vertical: i32,
        horizontal: i32,
    },
}

/// 屏幕内的唯一指针位置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pointer {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

impl Pointer {
    /// 初始在屏幕中央。
    pub fn new(width: i32, height: i32) -> Self {
        Self {
            x: width / 2,
            y: height / 2,
            width,
            height,
        }
    }

    pub fn position(&self) -> (i32, i32) {
        (self.x, self.y)
    }

    fn clamp(&mut self) {
        self.x = self.x.clamp(0, self.width - 1);
        self.y = self.y.clamp(0, self.height - 1);
    }

    fn scale(value: i32, range: AbsoluteRange, extent: i32) -> i32 {
        let span = i64::from(range.maximum) - i64::from(range.minimum);
        if span <= 0 {
            return 0;
        }
        let offset = (i64::from(value) - i64::from(range.minimum)).clamp(0, span);
        (offset * i64::from(extent - 1) / span) as i32
    }
}

/// 一个设备的解码状态：绝对设备带量程，按 `SYN_REPORT` 成批提交。
pub struct Decoder {
    ranges: Option<[AbsoluteRange; 2]>,
    absolute: [Option<i32>; 2],
    relative: [i32; 2],
    /// 本帧的滚轮输入 `[垂直, 水平]`：低分辨率档位与 hi-res 单位分开记，同一设备往往两种都报，
    /// 提交时以 hi-res 为准，否则一次滚动会被算两遍。
    wheel_low: [i32; 2],
    wheel_high: [i32; 2],
    /// 尚不足一个档位的 hi-res 余量；高精度滚轮逐步累计到 120 才产生一个档位。
    wheel_remainder: [i32; 2],
}

impl Decoder {
    pub fn new(ranges: Option<[AbsoluteRange; 2]>) -> Self {
        Self {
            ranges,
            absolute: [None; 2],
            relative: [0; 2],
            wheel_low: [0; 2],
            wheel_high: [0; 2],
            wheel_remainder: [0; 2],
        }
    }

    /// 喂入一条原始事件，产生的语义事件追加到 `out`，指针位置更新进 `pointer`。
    pub fn feed(
        &mut self,
        kind: u16,
        code: u16,
        value: i32,
        pointer: &mut Pointer,
        out: &mut Vec<Event>,
    ) {
        match (kind, code) {
            (EV_ABS, AXIS_X | AXIS_Y) if self.ranges.is_some() => {
                self.absolute[usize::from(code)] = Some(value)
            }
            (EV_REL, AXIS_X | AXIS_Y) => self.relative[usize::from(code)] += value,
            (EV_REL, REL_WHEEL) => self.wheel_low[0] -= value,
            (EV_REL, REL_HWHEEL) => self.wheel_low[1] += value,
            (EV_REL, REL_WHEEL_HI_RES) => self.wheel_high[0] -= value,
            (EV_REL, REL_HWHEEL_HI_RES) => self.wheel_high[1] += value,
            (EV_KEY, BUTTON_FIRST..=BUTTON_LAST) if value != 2 => out.push(Event::Button {
                code,
                pressed: value != 0,
            }),
            (EV_KEY, _) if value != 2 => out.push(Event::Key {
                code,
                pressed: value != 0,
            }),
            (EV_SYN, SYN_REPORT) => self.commit(pointer, out),
            _ => {}
        }
    }

    fn commit(&mut self, pointer: &mut Pointer, out: &mut Vec<Event>) {
        let before = pointer.position();
        if let Some(ranges) = self.ranges {
            if let Some(value) = self.absolute[0].take() {
                pointer.x = Pointer::scale(value, ranges[0], pointer.width);
            }
            if let Some(value) = self.absolute[1].take() {
                pointer.y = Pointer::scale(value, ranges[1], pointer.height);
            }
        }
        let mut notches = [0; 2];
        for axis in 0..2 {
            let low = std::mem::take(&mut self.wheel_low[axis]);
            let high = std::mem::take(&mut self.wheel_high[axis]);
            let units = if high != 0 { high } else { low * WHEEL_NOTCH };
            self.wheel_remainder[axis] += units;
            notches[axis] = self.wheel_remainder[axis] / WHEEL_NOTCH;
            self.wheel_remainder[axis] %= WHEEL_NOTCH;
        }
        if notches != [0; 2] {
            out.push(Event::Wheel {
                vertical: notches[0],
                horizontal: notches[1],
            });
        }
        pointer.x += std::mem::take(&mut self.relative[0]);
        pointer.y += std::mem::take(&mut self.relative[1]);
        pointer.clamp();
        if pointer.position() != before {
            out.push(Event::PointerMoved {
                x: pointer.x,
                y: pointer.y,
            });
        }
    }
}

/// 全部输入设备与共享的指针。
pub struct Input {
    devices: Vec<(InputDevice, Decoder)>,
    pointer: Pointer,
}

impl Input {
    /// 打开 `directory` 下所有 `event*` 设备。
    ///
    /// # Errors
    ///
    /// 目录不可读或设备无法打开。没有任何设备不是错误（无头运行）。
    pub fn open(directory: &Path, width: i32, height: i32) -> io::Result<Self> {
        let mut paths: Vec<_> = fs::read_dir(directory)?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("event"))
            })
            .collect();
        paths.sort();
        let mut devices = Vec::new();
        for path in paths {
            let device = InputDevice::open(&path)?;
            // 能报告 ABS_X/ABS_Y 量程的是绝对指针设备（触摸板/平板）。
            let ranges = match (
                device.absolute_range(AbsoluteAxis::X),
                device.absolute_range(AbsoluteAxis::Y),
            ) {
                (Ok(x), Ok(y)) if x.maximum > x.minimum && y.maximum > y.minimum => Some([x, y]),
                _ => None,
            };
            devices.push((device, Decoder::new(ranges)));
        }
        Ok(Self {
            devices,
            pointer: Pointer::new(width, height),
        })
    }

    pub fn pointer(&self) -> Pointer {
        self.pointer
    }

    pub fn devices(&self) -> impl Iterator<Item = &InputDevice> {
        self.devices.iter().map(|(device, _)| device)
    }

    /// 读空第 `index` 个设备当前可读的事件。
    ///
    /// # Errors
    ///
    /// 读取失败（非 `WouldBlock`）。
    pub fn drain(&mut self, index: usize, out: &mut Vec<Event>) -> io::Result<()> {
        let (device, decoder) = &mut self.devices[index];
        let mut buffer = [InputEvent::EMPTY; 64];
        loop {
            let count = match device.read_events(&mut buffer) {
                Ok(count) => count,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) => return Err(error),
            };
            for event in &buffer[..count] {
                decoder.feed(
                    event.kind(),
                    event.code(),
                    event.value(),
                    &mut self.pointer,
                    out,
                );
            }
            if count < buffer.len() {
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests;
