//! Linux LP64 ALSA Master controls；布局固定到项目 Linux 7.1 asound.h，不镜像内核 mixer 状态。
use super::ioctl_bytes;
use std::{
    fs::{File, OpenOptions},
    io,
    os::fd::AsFd,
};
const INFO: usize = 0xc110_5511;
const READ: usize = 0xc4c8_5512;
const WRITE: usize = 0xc4c8_5513;
const MIXER: u32 = 2;
const VOLUME: &str = "Master Playback Volume";
const SWITCH: &str = "Master Playback Switch";

/// 当前内核 Master 的百分比与静音快照；不持有第二份可修改的 mixer 状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MasterVolume {
    pub left: u8,
    pub right: u8,
    pub muted: bool,
}
/// 独占一个标准 ALSA control fd；设备权限由既有文件系统策略决定。
pub struct MasterControl {
    file: File,
}
struct Element {
    id: [u8; 64],
    minimum: i64,
    maximum: i64,
}
impl MasterControl {
    /// 打开 card0 的标准 control 节点；权限、设备不存在等错误直接返回，不降级为伪控件。
    pub fn open() -> io::Result<Self> {
        Ok(Self {
            file: OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/snd/controlC0")?,
        })
    }
    fn element(&self, name: &str, kind: u32) -> io::Result<Element> {
        let mut bytes = [0u8; 272];
        bytes[4..8].copy_from_slice(&MIXER.to_ne_bytes());
        bytes[16..16 + name.len()].copy_from_slice(name.as_bytes());
        ioctl_bytes(self.file.as_fd(), INFO, &mut bytes)?;
        let word = |at| u32::from_ne_bytes(bytes[at..at + 4].try_into().unwrap());
        if word(64) != kind || word(72) != 2 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Master control is not stereo integer/boolean",
            ));
        }
        let minimum = i64::from_ne_bytes(bytes[80..88].try_into().unwrap());
        let maximum = i64::from_ne_bytes(bytes[88..96].try_into().unwrap());
        // BOOLEAN 的范围由类型定义；INTEGER 必须有可安全换算的有界范围。
        let (minimum, maximum) = if kind == 1 {
            (0, 1)
        } else {
            (minimum, maximum)
        };
        if minimum < 0 || maximum <= minimum || maximum > i64::from(i32::MAX) {
            return Err(io::ErrorKind::InvalidData.into());
        }
        Ok(Element {
            id: bytes[..64].try_into().unwrap(),
            minimum,
            maximum,
        })
    }
    fn read(&self, element: &Element) -> io::Result<[i64; 2]> {
        let mut bytes = [0u8; 1224];
        bytes[..64].copy_from_slice(&element.id);
        ioctl_bytes(self.file.as_fd(), READ, &mut bytes)?;
        let values = [
            i64::from_ne_bytes(bytes[72..80].try_into().unwrap()),
            i64::from_ne_bytes(bytes[80..88].try_into().unwrap()),
        ];
        if values
            .iter()
            .any(|value| *value < element.minimum || *value > element.maximum)
        {
            return Err(io::ErrorKind::InvalidData.into());
        }
        Ok(values)
    }
    /// 读取实际左右音量与开关；返回百分比快照，ioctl/布局错误不发布虚假值。
    pub fn get(&self) -> io::Result<MasterVolume> {
        let volume = self.element(VOLUME, 2)?;
        let switch = self.element(SWITCH, 1)?;
        let values = self.read(&volume)?;
        let switches = self.read(&switch)?;
        let percent = |value| {
            (((value - volume.minimum) * 100 + (volume.maximum - volume.minimum) / 2)
                / (volume.maximum - volume.minimum)) as u8
        };
        Ok(MasterVolume {
            left: percent(values[0]),
            right: percent(values[1]),
            muted: switches == [0, 0],
        })
    }
    fn write(&self, element: &Element, values: [i64; 2]) -> io::Result<()> {
        let mut bytes = [0u8; 1224];
        bytes[..64].copy_from_slice(&element.id);
        for (index, value) in values.iter().enumerate() {
            bytes[72 + index * 8..80 + index * 8].copy_from_slice(&value.to_ne_bytes());
        }
        ioctl_bytes(self.file.as_fd(), WRITE, &mut bytes)
    }
    /// 将左右音量设为同一百分比并切换静音；返回后须 get 读取实际量化值。
    ///
    /// `percent` 必须为 0..=100。两个标准 control 分别写入；后一个失败时前一个可能已生效，错误不代表事务回滚。
    pub fn set(&self, percent: u8, muted: bool) -> io::Result<()> {
        self.set_stereo(percent, percent, muted)
    }

    /// 分别设置左右声道的百分比与静音。
    ///
    /// # Errors
    ///
    /// 任一百分比超过 100 返回 `InvalidInput`；ioctl 失败原样返回。
    pub fn set_stereo(&self, left: u8, right: u8, muted: bool) -> io::Result<()> {
        if left > 100 || right > 100 {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let volume = self.element(VOLUME, 2)?;
        let switch = self.element(SWITCH, 1)?;
        let value = |percent: u8| {
            volume.minimum + (volume.maximum - volume.minimum) * i64::from(percent) / 100
        };
        self.write(&volume, [value(left), value(right)])?;
        self.write(&switch, [i64::from(!muted); 2])
    }
}
