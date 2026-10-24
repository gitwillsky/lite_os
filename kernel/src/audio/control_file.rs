//! `/dev/snd/controlC0` 字符设备：声卡的 ALSA 控制接口（Master、逐流音量与事件）。

use alloc::sync::Arc;
use spin::Mutex;
use syscall_abi::errno;

use super::codec::{PCM_INFO_BYTES, PcmInfo, encode_pcm_info, read_u32};
use super::control::{
    CARD_INFO_BYTES, CTL_PROTOCOL_VERSION, CardText, ControlError, ControlOpen, ELEM_INFO_BYTES,
    ELEM_LIST_BYTES, ELEM_VALUE_BYTES, EVENT_BYTES, Element, SNDRV_CTL_IOCTL_CARD_INFO,
    SNDRV_CTL_IOCTL_ELEM_INFO, SNDRV_CTL_IOCTL_ELEM_LIST, SNDRV_CTL_IOCTL_ELEM_READ,
    SNDRV_CTL_IOCTL_ELEM_WRITE, SNDRV_CTL_IOCTL_PCM_INFO, SNDRV_CTL_IOCTL_PCM_NEXT_DEVICE,
    SNDRV_CTL_IOCTL_PCM_PREFER_SUBDEVICE, SNDRV_CTL_IOCTL_PVERSION,
    SNDRV_CTL_IOCTL_SUBSCRIBE_EVENTS, describe_element, element_by_id, encode_card_info,
    encode_event, list_elements, read_element, write_element,
};
use super::{CARD, CARD_ID, Card, PCM_NAME, SUBSTREAMS};
use crate::{
    fs::{
        FileSystemError,
        {
            DeviceNumber,
            chardev::{
                CharacterDriver, DeviceError, DeviceFile, DeviceWaitSources, IoctlCall,
                OpenRequest, UserMemory, UserOutput,
            },
        },
    },
    ipc::{Pipe, PipeDirection},
    namespace::PidIdentity,
};

pub(super) const CONTROL_NUMBER: DeviceNumber = DeviceNumber::new(116, 0);
pub(super) const CONTROL_PATH: &[u8] = b"snd/controlC0";

pub(super) struct ControlDriver;

impl CharacterDriver for ControlDriver {
    fn open(&self, _request: &OpenRequest<'_>) -> Result<Arc<dyn DeviceFile>, FileSystemError> {
        let card = CARD.get().cloned().ok_or(FileSystemError::IoError)?;
        let opener = (card.caller)().ok_or(FileSystemError::IoError)?;
        let file = Arc::try_new(ControlFile {
            card: card.clone(),
            opener,
            state: Mutex::new(ControlOpen::new()),
        })
        .map_err(|_| FileSystemError::OutOfMemory)?;
        let mut controls = card.controls.lock();
        controls.retain(|entry| {
            entry
                .upgrade()
                .is_some_and(|file| file.state.lock().active())
        });
        controls
            .try_reserve(1)
            .map_err(|_| FileSystemError::OutOfMemory)?;
        controls.push(Arc::downgrade(&file));
        Ok(file)
    }
}

/// 每次 control open 捕获实际 TID，偏好与订阅属于该 OFD，不能按 fd 或数字 PID 缓存。
pub(super) struct ControlFile {
    card: Arc<Card>,
    opener: Arc<PidIdentity>,
    // OWNER: 只串行化本 OFD 的订阅、FIFO 和偏好；不在此锁内执行 user-copy、引擎或 readiness 回调。
    state: Mutex<ControlOpen>,
}

impl Card {
    /// 按 Linux ctl_files 顺序，选择同一 opener TID 的第一个非 -1 偏好。
    pub(super) fn preferred_subdevice(&self) -> i32 {
        let Some(caller) = (self.caller)() else {
            return -1;
        };
        let controls = self.controls.lock();
        for entry in controls.iter() {
            if let Some(file) = entry.upgrade()
                && Arc::ptr_eq(&file.opener, &caller)
            {
                let preferred = file.state.lock().preferred();
                if preferred != -1 {
                    return preferred;
                }
            }
        }
        -1
    }

    fn notify_control(&self, element: Element) {
        {
            let controls = self.controls.lock();
            for entry in controls.iter() {
                if let Some(file) = entry.upgrade() {
                    file.state.lock().notify(element);
                }
            }
        }
        // 回调可以重入 control poll，必须先释放 registry 与所有 OFD state 锁。
        self.control_notification_write.signal_readiness();
    }
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
                // 只有 space=0 才是计数查询；需要输出 ID 时 NULL pids 必须返回 EFAULT。
                if used != 0 {
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
                let element = element_by_id(bytes).map_err(control_errno)?;
                let values = self.card.engine.lock().control_values(element);
                read_element(bytes, values).map_err(control_errno)
            })
            .map(|()| 0),
            SNDRV_CTL_IOCTL_ELEM_WRITE => {
                let mut changed_element = None;
                // Linux 的 put callback 以 bool 请求通知，但 ELEM_WRITE ioctl 成功统一返回 0。
                let result = exchange::<ELEM_VALUE_BYTES>(user, argument, |bytes| {
                    let (element, values) = write_element(bytes).map_err(control_errno)?;
                    if self.card.engine.lock().set_control(element, values) {
                        changed_element = Some(element);
                    }
                    read_element(bytes, values).map_err(control_errno)
                });
                // 与 Linux 一样，状态提交后的 copyout 失败不会回滚音量，通知也必须保留。
                if let Some(element) = changed_element {
                    self.card.notify_control(element);
                }
                result.map(|()| 0)
            }
            SNDRV_CTL_IOCTL_SUBSCRIBE_EVENTS => {
                let mut bytes = [0; 4];
                user.read(argument, &mut bytes).map_err(|_| errno::EFAULT)?;
                let subscribe = i32::from_ne_bytes(bytes);
                if subscribe < 0 {
                    let enabled = i32::from(self.state.lock().subscribed());
                    user.write(argument, &enabled.to_ne_bytes())
                        .map_err(|_| errno::EFAULT)?;
                } else {
                    self.state.lock().subscribe(subscribe != 0);
                }
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
                let mut bytes = [0; 4];
                user.read(argument, &mut bytes).map_err(|_| errno::EFAULT)?;
                self.state.lock().prefer(i32::from_ne_bytes(bytes));
                Ok(0)
            }
            _ => Err(errno::ENOTTY),
        }
    }
}

impl DeviceFile for ControlFile {
    fn supports_write(&self) -> bool {
        false
    }

    fn release(&self) {
        self.state.lock().close();
    }

    fn read(&self, output: &mut dyn UserOutput, nonblocking: bool) -> Result<(), DeviceError> {
        let mut delivered = false;
        loop {
            if !self.state.lock().subscribed() {
                return Err(DeviceError::Errno(errno::EBADFD));
            }
            if output.remaining() < EVENT_BYTES {
                return if delivered {
                    Ok(())
                } else {
                    Err(DeviceError::Errno(errno::EINVAL))
                };
            }
            if !self.state.lock().readable() {
                if delivered {
                    return Ok(());
                }
                crate::fs::chardev::wait_ready(self, 0x001, nonblocking)?;
                continue;
            }
            // user-copy 可能 fault/sleep，不能跨它持有 OFD 或 Card 锁。
            output
                .reserve(EVENT_BYTES)
                .map_err(|_| DeviceError::Errno(errno::EFAULT))?;
            let event = self.state.lock().pop();
            if let Some(element) = event {
                output
                    .write(&encode_event(element))
                    .map_err(|_| DeviceError::Errno(errno::EFAULT))?;
                delivered = true;
            } else if delivered {
                return Ok(());
            } else {
                crate::fs::chardev::wait_ready(self, 0x001, nonblocking)?;
            }
        }
    }

    fn poll(&self, events: i16) -> i16 {
        if self.state.lock().readable() {
            events & (0x001 | 0x040)
        } else {
            0
        }
    }

    fn wait_sources(&self, _events: i16) -> DeviceWaitSources {
        DeviceWaitSources::pipe(self.card.control_notification_read.pipe(), 0x001)
    }

    fn readiness_generation(&self) -> u64 {
        self.card
            .control_notification_read
            .pipe()
            .readiness_generation(PipeDirection::Read)
    }

    fn prepare_wait(&self, events: i16) -> Option<Arc<Pipe>> {
        if self.poll(events) != 0 {
            return None;
        }
        // 队列为空时旧 token 已失去语义；不排空会让阻塞 read 在 kernel 内反复立即醒来，
        // 占住 CPU，甚至阻止同 CPU 的 writer 从 timer sleep 返回。排空后必须重查真实队列。
        self.card.control_notification_read.drain_readiness();
        (self.poll(events) == 0).then(|| self.card.control_notification_read.pipe())
    }

    fn ioctl(&self, call: &IoctlCall<'_>) -> Result<isize, DeviceError> {
        self.dispatch(call).map_err(DeviceError::Errno)
    }
}
