//! `/dev/snd/controlC0` 字符设备：声卡的 ALSA 控制接口（主音量与静音开关）。

use alloc::sync::Arc;
use syscall_abi::errno;

use super::codec::{PCM_INFO_BYTES, PcmInfo, encode_pcm_info, read_u32};
use super::control::{
    CARD_INFO_BYTES, CTL_PROTOCOL_VERSION, CardText, ControlError, ELEM_INFO_BYTES,
    ELEM_LIST_BYTES, ELEM_VALUE_BYTES, SNDRV_CTL_IOCTL_CARD_INFO, SNDRV_CTL_IOCTL_ELEM_INFO,
    SNDRV_CTL_IOCTL_ELEM_LIST, SNDRV_CTL_IOCTL_ELEM_READ, SNDRV_CTL_IOCTL_ELEM_WRITE,
    SNDRV_CTL_IOCTL_PCM_INFO, SNDRV_CTL_IOCTL_PCM_NEXT_DEVICE,
    SNDRV_CTL_IOCTL_PCM_PREFER_SUBDEVICE, SNDRV_CTL_IOCTL_PVERSION,
    SNDRV_CTL_IOCTL_SUBSCRIBE_EVENTS, describe_element, encode_card_info, list_elements,
    read_element, write_element,
};
use super::{CARD, CARD_ID, Card, PCM_NAME, SUBSTREAMS};
use crate::{
    fs::{
        FileSystemError,
        {
            DeviceNumber,
            chardev::{
                CharacterDriver, DeviceError, DeviceFile, IoctlCall, OpenRequest, UserMemory,
            },
        },
    },
    ipc::Pipe,
};

pub(super) const CONTROL_NUMBER: DeviceNumber = DeviceNumber::new(116, 0);
pub(super) const CONTROL_PATH: &[u8] = b"snd/controlC0";

pub(super) struct ControlDriver;

impl CharacterDriver for ControlDriver {
    fn open(&self, _request: &OpenRequest<'_>) -> Result<Arc<dyn DeviceFile>, FileSystemError> {
        let card = CARD.get().cloned().ok_or(FileSystemError::IoError)?;
        Arc::try_new(ControlFile { card })
            .map(|file| file as Arc<dyn DeviceFile>)
            .map_err(|_| FileSystemError::IoError)
    }
}

/// 一个打开的控制设备；没有私有状态，音量属于声卡本身。
struct ControlFile {
    card: Arc<Card>,
}

fn control_errno(error: ControlError) -> isize {
    match error {
        ControlError::NoSuchElement => errno::ENOENT,
        ControlError::Invalid => errno::EINVAL,
    }
}

/// 读出用户态结构，交给 `handler` 处理，再把结果写回。`argument` 为 0 是 `EFAULT`。
fn exchange<const N: usize>(
    user: &dyn UserMemory,
    argument: usize,
    handler: impl FnOnce(&mut [u8; N]) -> Result<(), isize>,
) -> Result<(), isize> {
    if argument == 0 {
        return Err(errno::EFAULT);
    }
    let mut bytes = [0u8; N];
    user.read(argument, &mut bytes).map_err(|_| errno::EFAULT)?;
    handler(&mut bytes)?;
    user.write(argument, &bytes).map_err(|_| errno::EFAULT)
}

impl ControlFile {
    fn dispatch(&self, call: &IoctlCall<'_>) -> Result<isize, isize> {
        let (user, argument) = (call.user, call.argument);
        match call.request {
            SNDRV_CTL_IOCTL_PVERSION => {
                if argument == 0 {
                    return Err(errno::EFAULT);
                }
                user.write(argument, &CTL_PROTOCOL_VERSION.to_ne_bytes())
                    .map_err(|_| errno::EFAULT)?;
                Ok(0)
            }
            SNDRV_CTL_IOCTL_CARD_INFO => {
                if argument == 0 {
                    return Err(errno::EFAULT);
                }
                let info = encode_card_info(&CardText {
                    id: CARD_ID,
                    name: b"LiteOS Sound",
                    long_name: b"LiteOS VirtIO Sound Card",
                    mixer_name: b"LiteOS Mixer",
                });
                debug_assert_eq!(info.len(), CARD_INFO_BYTES);
                user.write(argument, &info).map_err(|_| errno::EFAULT)?;
                Ok(0)
            }
            SNDRV_CTL_IOCTL_ELEM_LIST => {
                if argument == 0 {
                    return Err(errno::EFAULT);
                }
                let mut list = [0u8; ELEM_LIST_BYTES];
                user.read(argument, &mut list).map_err(|_| errno::EFAULT)?;
                let pointer = super::codec::read_u64(&list, 16).ok_or(errno::EFAULT)? as usize;
                let (ids, used) = list_elements(&mut list).map_err(control_errno)?;
                // 调用方给了 id 数组指针时才写 id；指针为 0 表示它只想要总数（`count`）。
                if pointer != 0 && used != 0 {
                    user.write(pointer, &ids[..used * super::control::ELEM_ID_BYTES])
                        .map_err(|_| errno::EFAULT)?;
                }
                user.write(argument, &list).map_err(|_| errno::EFAULT)?;
                Ok(0)
            }
            SNDRV_CTL_IOCTL_ELEM_INFO => exchange::<ELEM_INFO_BYTES>(user, argument, |bytes| {
                describe_element(bytes).map_err(control_errno)
            })
            .map(|()| 0),
            SNDRV_CTL_IOCTL_ELEM_READ => exchange::<ELEM_VALUE_BYTES>(user, argument, |bytes| {
                let master = self.card.engine.lock().master();
                read_element(bytes, master).map_err(control_errno)
            })
            .map(|()| 0),
            SNDRV_CTL_IOCTL_ELEM_WRITE => {
                // Linux 约定：值发生变化返回 1，没有变化返回 0。
                let mut changed = false;
                exchange::<ELEM_VALUE_BYTES>(user, argument, |bytes| {
                    let mut engine = self.card.engine.lock();
                    let (master, did_change) =
                        write_element(bytes, engine.master()).map_err(control_errno)?;
                    engine.set_master(master);
                    changed = did_change;
                    Ok(())
                })?;
                Ok(isize::from(changed))
            }
            SNDRV_CTL_IOCTL_SUBSCRIBE_EVENTS => {
                // 事件投递尚未实现：接受订阅请求，使 `alsa-lib` 能打开混音器；控制值被别的进程改变时本进程不会
                // 收到通知，需要重新读取。读出的状态恒为 0（未订阅），如实反映没有事件队列。
                if argument == 0 {
                    return Err(errno::EFAULT);
                }
                user.write(argument, &0i32.to_ne_bytes())
                    .map_err(|_| errno::EFAULT)?;
                Ok(0)
            }
            SNDRV_CTL_IOCTL_PCM_NEXT_DEVICE => {
                // 输入是上一个设备号（-1 表示从头开始）；只有设备 0，之后返回 -1 表示没有更多。
                if argument == 0 {
                    return Err(errno::EFAULT);
                }
                let mut bytes = [0u8; 4];
                user.read(argument, &mut bytes).map_err(|_| errno::EFAULT)?;
                let next: i32 = if i32::from_ne_bytes(bytes) < 0 { 0 } else { -1 };
                user.write(argument, &next.to_ne_bytes())
                    .map_err(|_| errno::EFAULT)?;
                Ok(0)
            }
            SNDRV_CTL_IOCTL_PCM_INFO => exchange::<PCM_INFO_BYTES>(user, argument, |bytes| {
                // 请求里 `device`@0、`subdevice`@4、`stream`@8：只有设备 0 的 playback，子流 0..SUBSTREAMS。
                let device = read_u32(bytes, 0).ok_or(errno::EINVAL)?;
                let subdevice = read_u32(bytes, 4).ok_or(errno::EINVAL)?;
                let stream = read_u32(bytes, 8).ok_or(errno::EINVAL)?;
                if device != 0 || stream != 0 || subdevice as usize >= SUBSTREAMS {
                    return Err(errno::ENOENT);
                }
                let available = self.card.engine.lock().available();
                *bytes = encode_pcm_info(&PcmInfo {
                    subdevice,
                    subdevices_count: SUBSTREAMS as u32,
                    subdevices_available: available as u32,
                    card_id: CARD_ID,
                    name: PCM_NAME,
                });
                Ok(())
            })
            .map(|()| 0),
            SNDRV_CTL_IOCTL_PCM_PREFER_SUBDEVICE => {
                // 偏好子设备只是提示；open 总是占用第一个空闲子流，所以只校验参数合法即可。
                if argument == 0 {
                    return Err(errno::EFAULT);
                }
                let mut bytes = [0u8; 4];
                user.read(argument, &mut bytes).map_err(|_| errno::EFAULT)?;
                Ok(0)
            }
            _ => Err(errno::ENOTTY),
        }
    }
}

impl DeviceFile for ControlFile {
    fn poll(&self, _events: i16) -> i16 {
        // 没有事件队列，控制设备永远不就绪。
        0
    }

    fn prepare_wait(&self, _events: i16) -> Option<Arc<Pipe>> {
        None
    }

    fn ioctl(&self, call: &IoctlCall<'_>) -> Result<isize, DeviceError> {
        self.dispatch(call).map_err(DeviceError::Errno)
    }
}
