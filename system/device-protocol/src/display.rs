//! 显示与 virgl 3D：scanout/damage/cursor/virgl 命令走请求 channel，完成经活动 Event 通知后由使用者
//! `poll_update` 拉取。
//!
//! 缓冲（GEM、cursor、virgl backing 与命令流）以 VMO 句柄随请求传递；驱动宿主按 VMO koid 缓存对应的
//! [`DeviceBacking`]，同一缓冲反复提交时复用同一份固定记录，不重复向 IOMMU 申请设备地址。

use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

use device_model::{
    Activity, CursorCommand, DeviceBacking, DisplayDevice, DisplayError, DisplayMode, DisplayRect,
    DisplayUpdate, GraphicsDevice, VirglBox, VirglCapsetInfo, VirglCommand, VirglTransferDirection,
};
use native_runtime::native_abi::{self, Status, rights};
use native_runtime::sync::Mutex;
use native_runtime::{Channel, Event, Handle, Vmo};

use crate::directory::{self, Announcement, Class};
use crate::{ActivityBinding, EventHub, Reader, Writer, activity_signal, call, serve, take_handle};

const MODE: u32 = 1;
const SUBMIT_SCANOUT: u32 = 2;
const SUBMIT_DAMAGE: u32 = 3;
const RELEASE_BUFFER: u32 = 4;
const DISABLE_SCANOUT: u32 = 5;
const POLL_UPDATE: u32 = 6;
const SUBMIT_CURSOR_UPLOAD: u32 = 7;
const SUBMIT_CURSOR: u32 = 8;
const MOVE_CURSOR: u32 = 9;
const COPY_CAPSET: u32 = 10;
const SUBMIT_VIRGL: u32 = 11;
const SUBMIT_VIRGL_SCANOUT: u32 = 12;

const ERROR_WOULD_BLOCK: Status = native_abi::ERR_SHOULD_WAIT;
const ERROR_INVALID_RECTANGLE: Status = native_abi::ERR_OUT_OF_RANGE;

fn encode_error(error: DisplayError) -> Status {
    match error {
        DisplayError::WouldBlock => ERROR_WOULD_BLOCK,
        DisplayError::InvalidRectangle => ERROR_INVALID_RECTANGLE,
        DisplayError::Device => native_abi::ERR_IO,
    }
}

fn decode_error(status: Status) -> DisplayError {
    match status {
        ERROR_WOULD_BLOCK => DisplayError::WouldBlock,
        ERROR_INVALID_RECTANGLE => DisplayError::InvalidRectangle,
        _ => DisplayError::Device,
    }
}

fn put_mode(writer: &mut Writer, mode: DisplayMode) {
    writer.u32(mode.width).u32(mode.height).u32(mode.pitch);
}

fn get_mode(reader: &mut Reader<'_>) -> Result<DisplayMode, Status> {
    Ok(DisplayMode {
        width: reader.u32()?,
        height: reader.u32()?,
        pitch: reader.u32()?,
    })
}

fn put_rect(writer: &mut Writer, rect: DisplayRect) {
    writer.u32(rect.x).u32(rect.y).u32(rect.width).u32(rect.height);
}

fn get_rect(reader: &mut Reader<'_>) -> Result<DisplayRect, Status> {
    Ok(DisplayRect {
        x: reader.u32()?,
        y: reader.u32()?,
        width: reader.u32()?,
        height: reader.u32()?,
    })
}

const UPDATE_OPERATION: u8 = 1;
const UPDATE_MODE: u8 = 2;
const UPDATE_READY: u8 = 3;
const UPDATE_RENDER: u8 = 4;
const UPDATE_CURSOR: u8 = 5;

const VIRGL_CONTEXT_CREATE: u8 = 1;
const VIRGL_CONTEXT_DESTROY: u8 = 2;
const VIRGL_RESOURCE_CREATE: u8 = 3;
const VIRGL_ATTACH_BACKING: u8 = 4;
const VIRGL_CONTEXT_ATTACH: u8 = 5;
const VIRGL_CONTEXT_DETACH: u8 = 6;
const VIRGL_SUBMIT: u8 = 7;
const VIRGL_TRANSFER: u8 = 8;
const VIRGL_SET_SCANOUT: u8 = 9;
const VIRGL_FLUSH: u8 = 10;
const VIRGL_RESOURCE_UNREF: u8 = 11;

/// 驱动宿主侧的缓冲缓存：VMO koid → 仍被驱动持有的 [`DeviceBacking`]。
struct BackingCache {
    // OWNER: 只保存弱引用；驱动释放最后一个强引用后该项失效并在下次插入时清理。缺失时同一 GEM 每帧
    // 重新固定一次，设备地址空间随帧数增长。
    entries: Mutex<BTreeMap<u64, Weak<DeviceBacking>>>,
}

impl BackingCache {
    fn resolve(&self, handle: Handle) -> Result<Arc<DeviceBacking>, Status> {
        let koid = handle.basic()?.koid;
        let mut entries = self.entries.lock();
        if let Some(backing) = entries.get(&koid).and_then(Weak::upgrade) {
            return Ok(backing);
        }
        let backing = Arc::try_new(DeviceBacking::from_vmo(Vmo::from_handle(handle))?)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        entries.retain(|_, entry| entry.strong_count() != 0);
        entries.insert(koid, Arc::downgrade(&backing));
        Ok(backing)
    }
}

fn backing_handle(backing: &DeviceBacking) -> Result<Handle, DisplayError> {
    backing
        .vmo()
        .duplicate(rights::SAME)
        .map(Vmo::into_handle)
        .map_err(|_| DisplayError::Device)
}

/// 驱动宿主：发布显示/图形设备。
///
/// # Errors
///
/// 对象、线程创建或目录写入失败。
pub fn publish(directory: &Channel, device: Arc<dyn GraphicsDevice>) -> Result<(), Status> {
    let (client, server) = Channel::create()?;
    let event = Event::create()?;
    let peer_event = event.duplicate(rights::SAME)?;
    let mut description = directory::description();
    match device.platform_identity() {
        Some((base, irq)) => {
            description.bool(true).u64(base).u32(irq);
        }
        None => {
            description.bool(false);
        }
    }
    description.u32(device.first_dynamic_resource_id());
    match device.virgl_capset_info() {
        Some(info) => {
            description.bool(true).u32(info.id).u32(info.version).u64(info.size as u64);
        }
        None => {
            description.bool(false);
        }
    }
    description.bool(device.supports_virgl_context_init());
    device.bind_activity(activity_signal(Arc::try_new(event).map_err(|_| native_abi::ERR_NO_MEMORY)?)?);
    let cache = Arc::try_new(BackingCache {
        entries: Mutex::new(BTreeMap::new()),
    })
    .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    serve(
        server,
        2,
        b"display-server",
        Arc::try_new(move |ordinal: u32, request: &mut Reader<'_>, mut handles: Vec<Handle>, reply: &mut Writer| {
            handle(&*device, &cache, ordinal, request, &mut handles, reply)?;
            Ok(Vec::new())
        })
        .map_err(|_| native_abi::ERR_NO_MEMORY)?,
    )?;
    directory::announce(
        directory,
        Class::Display,
        &description,
        alloc::vec![client.into_handle(), peer_event.into_handle()],
    )
}

fn handle(
    device: &dyn GraphicsDevice,
    cache: &BackingCache,
    ordinal: u32,
    request: &mut Reader<'_>,
    handles: &mut [Handle],
    reply: &mut Writer,
) -> Result<(), Status> {
    let mut backing = |index: usize| cache.resolve(take_handle(handles, index)?);
    match ordinal {
        MODE => put_mode(reply, device.mode()),
        SUBMIT_SCANOUT => {
            let identity = request.u64()?;
            let mode = get_mode(request)?;
            let fence = device.submit_scanout(identity, mode, backing(0)?).map_err(encode_error)?;
            reply.u64(fence);
        }
        SUBMIT_DAMAGE => {
            let identity = request.u64()?;
            let mode = get_mode(request)?;
            let count = request.u32()? as usize;
            let mut rectangles = Vec::new();
            rectangles.try_reserve_exact(count).map_err(|_| native_abi::ERR_NO_MEMORY)?;
            for _ in 0..count {
                rectangles.push(get_rect(request)?);
            }
            let fence = device
                .submit_damage(identity, mode, backing(0)?, &rectangles)
                .map_err(encode_error)?;
            reply.u64(fence);
        }
        RELEASE_BUFFER => match device.release_buffer(request.u64()?).map_err(encode_error)? {
            Some(fence) => {
                reply.bool(true).u64(fence);
            }
            None => {
                reply.bool(false);
            }
        },
        DISABLE_SCANOUT => {
            reply.u64(device.disable_scanout().map_err(encode_error)?);
        }
        POLL_UPDATE => match device.poll_update().map_err(encode_error)? {
            None => {
                reply.u8(0);
            }
            Some(DisplayUpdate::OperationCompleted(fence)) => {
                reply.u8(UPDATE_OPERATION).u64(fence);
            }
            Some(DisplayUpdate::ModeChanged(mode)) => {
                reply.u8(UPDATE_MODE);
                put_mode(reply, mode);
            }
            Some(DisplayUpdate::AdapterReady) => {
                reply.u8(UPDATE_READY);
            }
            Some(DisplayUpdate::RenderCompleted(fence)) => {
                reply.u8(UPDATE_RENDER).u64(fence);
            }
            Some(DisplayUpdate::CursorCompleted(fence)) => {
                reply.u8(UPDATE_CURSOR).u64(fence);
            }
        },
        SUBMIT_CURSOR_UPLOAD => {
            let identity = request.u64()?;
            reply.u64(device.submit_cursor_upload(identity, backing(0)?).map_err(encode_error)?);
        }
        SUBMIT_CURSOR => {
            let x = request.u32()?;
            let y = request.u32()?;
            let visible = request.bool()?;
            let command = if request.bool()? {
                CursorCommand::Update {
                    x,
                    y,
                    visible,
                    hot_x: request.u32()?,
                    hot_y: request.u32()?,
                }
            } else {
                CursorCommand::Move { x, y, visible }
            };
            reply.u64(device.submit_cursor(command).map_err(encode_error)?);
        }
        MOVE_CURSOR => {
            let x = request.u32()?;
            let y = request.u32()?;
            device.move_cursor(x, y).map_err(encode_error)?;
        }
        COPY_CAPSET => {
            let version = request.u32()?;
            let capacity = request.usize()?.min(native_abi::CHANNEL_MAX_BYTES / 2);
            let mut output = alloc::vec![0u8; capacity];
            let length = device.copy_virgl_capset(version, &mut output).map_err(encode_error)?;
            reply.bytes(&output[..length]);
        }
        SUBMIT_VIRGL => {
            let fence = submit_virgl(device, request, &mut backing)?;
            reply.u64(fence);
        }
        SUBMIT_VIRGL_SCANOUT => {
            let mode = get_mode(request)?;
            let resource_id = request.u32()?;
            reply.u64(device.submit_virgl_scanout(mode, resource_id).map_err(encode_error)?);
        }
        _ => return Err(native_abi::ERR_NOT_SUPPORTED),
    }
    Ok(())
}

fn get_box(reader: &mut Reader<'_>) -> Result<VirglBox, Status> {
    Ok(VirglBox {
        x: reader.u32()?,
        y: reader.u32()?,
        z: reader.u32()?,
        width: reader.u32()?,
        height: reader.u32()?,
        depth: reader.u32()?,
    })
}

fn submit_virgl(
    device: &dyn GraphicsDevice,
    request: &mut Reader<'_>,
    backing: &mut dyn FnMut(usize) -> Result<Arc<DeviceBacking>, Status>,
) -> Result<u64, Status> {
    let command = match request.u8()? {
        VIRGL_CONTEXT_CREATE => VirglCommand::ContextCreate {
            context_id: request.u32()?,
            context_init: request.u32()?,
            name: request.bytes()?,
        },
        VIRGL_CONTEXT_DESTROY => VirglCommand::ContextDestroy {
            context_id: request.u32()?,
        },
        VIRGL_RESOURCE_CREATE => VirglCommand::ResourceCreate3d {
            resource_id: request.u32()?,
            target: request.u32()?,
            format: request.u32()?,
            bind: request.u32()?,
            width: request.u32()?,
            height: request.u32()?,
            depth: request.u32()?,
            array_size: request.u32()?,
            last_level: request.u32()?,
            samples: request.u32()?,
            flags: request.u32()?,
        },
        VIRGL_ATTACH_BACKING => VirglCommand::ResourceAttachBacking {
            resource_id: request.u32()?,
            backing: backing(0)?,
        },
        VIRGL_CONTEXT_ATTACH => VirglCommand::ContextAttachResource {
            context_id: request.u32()?,
            resource_id: request.u32()?,
        },
        VIRGL_CONTEXT_DETACH => VirglCommand::ContextDetachResource {
            context_id: request.u32()?,
            resource_id: request.u32()?,
        },
        VIRGL_SUBMIT => VirglCommand::Submit3d {
            context_id: request.u32()?,
            length: request.usize()?,
            commands: backing(0)?,
        },
        VIRGL_TRANSFER => VirglCommand::Transfer3d {
            direction: if request.bool()? {
                VirglTransferDirection::FromHost
            } else {
                VirglTransferDirection::ToHost
            },
            context_id: request.u32()?,
            offset: request.u64()?,
            resource_id: request.u32()?,
            level: request.u32()?,
            region: get_box(request)?,
            stride: request.u32()?,
            layer_stride: request.u32()?,
        },
        VIRGL_SET_SCANOUT => VirglCommand::SetScanout {
            mode: get_mode(request)?,
            resource_id: request.u32()?,
        },
        VIRGL_FLUSH => VirglCommand::Flush {
            resource_id: request.u32()?,
            rectangle: get_rect(request)?,
        },
        VIRGL_RESOURCE_UNREF => VirglCommand::ResourceUnref {
            resource_id: request.u32()?,
        },
        _ => return Err(native_abi::ERR_INVALID_ARGS),
    };
    device.submit_virgl(command).map_err(encode_error)
}

/// 使用者端的远端显示/图形设备。
pub struct RemoteDisplay {
    channel: Channel,
    activity: ActivityBinding,
    platform_identity: Option<(u64, u32)>,
    first_dynamic_resource_id: u32,
    capset: Option<VirglCapsetInfo>,
    context_init: bool,
}

/// 连接目录中的显示设备。
///
/// # Errors
///
/// 描述或句柄不完整，或事件登记失败。
pub fn connect(mut announcement: Announcement, hub: &Arc<EventHub>) -> Result<Arc<RemoteDisplay>, Status> {
    let channel = Channel::from_handle(take_handle(&mut announcement.handles, 0)?);
    let activity = hub.activity(Event::from_handle(take_handle(&mut announcement.handles, 1)?))?;
    let mut reader = announcement.reader();
    let platform_identity = if reader.bool()? {
        Some((reader.u64()?, reader.u32()?))
    } else {
        None
    };
    let first_dynamic_resource_id = reader.u32()?;
    let capset = if reader.bool()? {
        Some(VirglCapsetInfo {
            id: reader.u32()?,
            version: reader.u32()?,
            size: reader.usize()?,
        })
    } else {
        None
    };
    let context_init = reader.bool()?;
    Arc::try_new(RemoteDisplay {
        channel,
        activity,
        platform_identity,
        first_dynamic_resource_id,
        capset,
        context_init,
    })
    .map_err(|_| native_abi::ERR_NO_MEMORY)
}

impl RemoteDisplay {
    fn fence(&self, request: &Writer, handles: Vec<Handle>) -> Result<u64, DisplayError> {
        let reply = call(&self.channel, request, handles).map_err(decode_error)?;
        reply.reader().u64().map_err(|_| DisplayError::Device)
    }
}

impl DisplayDevice for RemoteDisplay {
    fn bind_activity(&self, activity: Activity) {
        self.activity.bind(activity);
    }

    fn mode(&self) -> DisplayMode {
        call(&self.channel, &Writer::request(MODE), Vec::new())
            .and_then(|reply| get_mode(&mut reply.reader()))
            .unwrap_or(DisplayMode {
                width: 0,
                height: 0,
                pitch: 0,
            })
    }

    fn submit_scanout(&self, identity: u64, mode: DisplayMode, backing: Arc<DeviceBacking>) -> Result<u64, DisplayError> {
        let mut request = Writer::request(SUBMIT_SCANOUT);
        request.u64(identity);
        put_mode(&mut request, mode);
        self.fence(&request, alloc::vec![backing_handle(&backing)?])
    }

    fn submit_damage(
        &self,
        identity: u64,
        mode: DisplayMode,
        backing: Arc<DeviceBacking>,
        rectangles: &[DisplayRect],
    ) -> Result<u64, DisplayError> {
        let mut request = Writer::request(SUBMIT_DAMAGE);
        request.u64(identity);
        put_mode(&mut request, mode);
        request.u32(rectangles.len() as u32);
        for rect in rectangles {
            put_rect(&mut request, *rect);
        }
        self.fence(&request, alloc::vec![backing_handle(&backing)?])
    }

    fn release_buffer(&self, identity: u64) -> Result<Option<u64>, DisplayError> {
        let mut request = Writer::request(RELEASE_BUFFER);
        request.u64(identity);
        let reply = call(&self.channel, &request, Vec::new()).map_err(decode_error)?;
        let mut reader = reply.reader();
        if reader.bool().map_err(|_| DisplayError::Device)? {
            Ok(Some(reader.u64().map_err(|_| DisplayError::Device)?))
        } else {
            Ok(None)
        }
    }

    fn disable_scanout(&self) -> Result<u64, DisplayError> {
        self.fence(&Writer::request(DISABLE_SCANOUT), Vec::new())
    }

    fn poll_update(&self) -> Result<Option<DisplayUpdate>, DisplayError> {
        let reply = call(&self.channel, &Writer::request(POLL_UPDATE), Vec::new()).map_err(decode_error)?;
        let mut reader = reply.reader();
        let decode = |reader: &mut Reader<'_>| -> Result<Option<DisplayUpdate>, Status> {
            Ok(match reader.u8()? {
                0 => None,
                UPDATE_OPERATION => Some(DisplayUpdate::OperationCompleted(reader.u64()?)),
                UPDATE_MODE => Some(DisplayUpdate::ModeChanged(get_mode(reader)?)),
                UPDATE_READY => Some(DisplayUpdate::AdapterReady),
                UPDATE_RENDER => Some(DisplayUpdate::RenderCompleted(reader.u64()?)),
                UPDATE_CURSOR => Some(DisplayUpdate::CursorCompleted(reader.u64()?)),
                _ => return Err(native_abi::ERR_INVALID_ARGS),
            })
        };
        decode(&mut reader).map_err(|_| DisplayError::Device)
    }
}

fn put_virgl(request: &mut Writer, command: &VirglCommand<'_>, handles: &mut Vec<Handle>) -> Result<(), DisplayError> {
    match command {
        VirglCommand::ContextCreate {
            context_id,
            context_init,
            name,
        } => {
            request.u8(VIRGL_CONTEXT_CREATE).u32(*context_id).u32(*context_init).bytes(name);
        }
        VirglCommand::ContextDestroy { context_id } => {
            request.u8(VIRGL_CONTEXT_DESTROY).u32(*context_id);
        }
        VirglCommand::ResourceCreate3d {
            resource_id,
            target,
            format,
            bind,
            width,
            height,
            depth,
            array_size,
            last_level,
            samples,
            flags,
        } => {
            request.u8(VIRGL_RESOURCE_CREATE);
            for value in [
                *resource_id,
                *target,
                *format,
                *bind,
                *width,
                *height,
                *depth,
                *array_size,
                *last_level,
                *samples,
                *flags,
            ] {
                request.u32(value);
            }
        }
        VirglCommand::ResourceAttachBacking { resource_id, backing } => {
            request.u8(VIRGL_ATTACH_BACKING).u32(*resource_id);
            handles.push(backing_handle(backing)?);
        }
        VirglCommand::ContextAttachResource {
            context_id,
            resource_id,
        } => {
            request.u8(VIRGL_CONTEXT_ATTACH).u32(*context_id).u32(*resource_id);
        }
        VirglCommand::ContextDetachResource {
            context_id,
            resource_id,
        } => {
            request.u8(VIRGL_CONTEXT_DETACH).u32(*context_id).u32(*resource_id);
        }
        VirglCommand::Submit3d {
            context_id,
            commands,
            length,
        } => {
            request.u8(VIRGL_SUBMIT).u32(*context_id).u64(*length as u64);
            handles.push(backing_handle(commands)?);
        }
        VirglCommand::Transfer3d {
            direction,
            context_id,
            offset,
            resource_id,
            level,
            region,
            stride,
            layer_stride,
        } => {
            request
                .u8(VIRGL_TRANSFER)
                .bool(*direction == VirglTransferDirection::FromHost)
                .u32(*context_id)
                .u64(*offset)
                .u32(*resource_id)
                .u32(*level);
            for value in [region.x, region.y, region.z, region.width, region.height, region.depth] {
                request.u32(value);
            }
            request.u32(*stride).u32(*layer_stride);
        }
        VirglCommand::SetScanout { mode, resource_id } => {
            request.u8(VIRGL_SET_SCANOUT);
            put_mode(request, *mode);
            request.u32(*resource_id);
        }
        VirglCommand::Flush { resource_id, rectangle } => {
            request.u8(VIRGL_FLUSH).u32(*resource_id);
            put_rect(request, *rectangle);
        }
        VirglCommand::ResourceUnref { resource_id } => {
            request.u8(VIRGL_RESOURCE_UNREF).u32(*resource_id);
        }
    }
    Ok(())
}

impl GraphicsDevice for RemoteDisplay {
    fn platform_identity(&self) -> Option<(u64, u32)> {
        self.platform_identity
    }

    fn submit_cursor_upload(&self, identity: u64, backing: Arc<DeviceBacking>) -> Result<u64, DisplayError> {
        let mut request = Writer::request(SUBMIT_CURSOR_UPLOAD);
        request.u64(identity);
        self.fence(&request, alloc::vec![backing_handle(&backing)?])
    }

    fn submit_cursor(&self, command: CursorCommand) -> Result<u64, DisplayError> {
        let mut request = Writer::request(SUBMIT_CURSOR);
        match command {
            CursorCommand::Move { x, y, visible } => {
                request.u32(x).u32(y).bool(visible).bool(false);
            }
            CursorCommand::Update {
                x,
                y,
                visible,
                hot_x,
                hot_y,
            } => {
                request.u32(x).u32(y).bool(visible).bool(true).u32(hot_x).u32(hot_y);
            }
        }
        self.fence(&request, Vec::new())
    }

    fn move_cursor(&self, x: u32, y: u32) -> Result<(), DisplayError> {
        let mut request = Writer::request(MOVE_CURSOR);
        request.u32(x).u32(y);
        call(&self.channel, &request, Vec::new()).map_err(decode_error)?;
        Ok(())
    }

    fn first_dynamic_resource_id(&self) -> u32 {
        self.first_dynamic_resource_id
    }

    fn virgl_capset_info(&self) -> Option<VirglCapsetInfo> {
        self.capset
    }

    fn supports_virgl_context_init(&self) -> bool {
        self.context_init
    }

    fn copy_virgl_capset(&self, version: u32, output: &mut [u8]) -> Result<usize, DisplayError> {
        let mut request = Writer::request(COPY_CAPSET);
        request.u32(version).u64(output.len() as u64);
        let reply = call(&self.channel, &request, Vec::new()).map_err(decode_error)?;
        let data = reply.reader().bytes().map_err(|_| DisplayError::Device)?;
        let target = output.get_mut(..data.len()).ok_or(DisplayError::Device)?;
        target.copy_from_slice(data);
        Ok(data.len())
    }

    fn submit_virgl(&self, command: VirglCommand<'_>) -> Result<u64, DisplayError> {
        let mut request = Writer::request(SUBMIT_VIRGL);
        let mut handles = Vec::new();
        put_virgl(&mut request, &command, &mut handles)?;
        self.fence(&request, handles)
    }

    fn submit_virgl_scanout(&self, mode: DisplayMode, resource_id: u32) -> Result<u64, DisplayError> {
        let mut request = Writer::request(SUBMIT_VIRGL_SCANOUT);
        put_mode(&mut request, mode);
        request.u32(resource_id);
        self.fence(&request, Vec::new())
    }
}
