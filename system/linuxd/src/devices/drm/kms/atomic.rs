//! 原子属性事务：用户数据在 completion 锁外解码/固定，锁内只把变更应用到最新逻辑状态。
use super::{CRTC_ID, Configuration, Object, Property, configuration::Mode};
use crate::devices::drm::{DrmError, DrmFile, Framebuffer, property::Blob};
use alloc::sync::Arc;

/// 固定硬件 schema 的最后赋值集合；不是用户条目上限。重复赋值逐次校验，最后一个有效值生效。
/// Arc 由请求保活到提交锁外，失败/覆盖时不能在 completion 锁中触发 payload 的最终析构。
pub(in crate::devices::drm) struct Changes {
    values: [[Option<u64>; 14]; 3],
    framebuffer: Option<Arc<Framebuffer>>,
    mode: Option<Mode>,
    damage: Option<Arc<Blob>>,
}
impl Changes {
    pub(in crate::devices::drm) const fn new() -> Self {
        Self {
            values: [[None; 14]; 3],
            framebuffer: None,
            mode: None,
            damage: None,
        }
    }
    pub(in crate::devices::drm) fn is_empty(&self) -> bool {
        self.values.iter().flatten().all(Option::is_none)
    }
    /// 只接收当前对象实际附着的标准属性；所有用户引用在硬件状态锁外解析。
    pub(in crate::devices::drm) fn set(
        &mut self,
        file: &DrmFile,
        object: Object,
        property: Property,
        value: u64,
    ) -> Result<(), DrmError> {
        if !object.properties().contains(&property) {
            return Err(DrmError::NotFound);
        }
        match property {
            Property::PlaneType => return Err(DrmError::Invalid),
            Property::Framebuffer => {
                let id = u32::try_from(value).map_err(|_| DrmError::Invalid)?;
                self.framebuffer = if id == 0 {
                    None
                } else {
                    Some(file.lookup_framebuffer(id).map_err(|_| DrmError::Invalid)?)
                };
            }
            Property::Mode | Property::Damage => {
                let id = u32::try_from(value).map_err(|_| DrmError::Invalid)?;
                let blob = if id == 0 {
                    None
                } else {
                    Some(file.property_blob(id).map_err(|_| DrmError::Invalid)?)
                };
                if property == Property::Mode {
                    self.mode = blob.map(|blob| Mode::parse(file, blob)).transpose()?;
                } else {
                    // drm_mode_rect 是四个 s32（16 字节），与 legacy DIRTYFB 的 u16 clip 不同。
                    if blob
                        .as_ref()
                        .is_some_and(|blob| !blob.bytes.len().is_multiple_of(16))
                    {
                        return Err(DrmError::Invalid);
                    }
                    self.damage = blob;
                }
            }
            Property::Crtc if value != 0 && value != u64::from(CRTC_ID) => {
                return Err(DrmError::Invalid);
            }
            Property::Active if value > 1 => return Err(DrmError::Invalid),
            Property::CrtcX | Property::CrtcY => {
                if value as i64 != value as i32 as i64 {
                    return Err(DrmError::Invalid);
                }
            }
            _ if value > u64::from(u32::MAX) => return Err(DrmError::Invalid),
            _ => {}
        }
        self.values[object.index()][property as usize - Property::Framebuffer as usize] =
            Some(value);
        Ok(())
    }
    /// 重试必须从最新逻辑配置重新应用，而非重新发表过期快照覆盖其他提交。
    pub(in crate::devices::drm) fn apply(
        &self,
        current: Option<&Configuration>,
        owner: u64,
        allow_modeset: bool,
    ) -> Result<Configuration, DrmError> {
        let mut next = current
            .cloned()
            .unwrap_or_else(|| Configuration::empty(owner));
        next.owner = owner;
        // 与 DRM plane duplicate state 一致：damage 只描述本次提交，不能把上一帧提示继承为新损伤。
        next.damage = None;
        for object in [Object::Crtc, Object::Connector, Object::Primary] {
            for &property in object.properties() {
                let Some(value) =
                    self.values[object.index()][property as usize - Property::Framebuffer as usize]
                else {
                    continue;
                };
                match property {
                    Property::Framebuffer => next.framebuffer = self.framebuffer.clone(),
                    Property::Mode => next.mode = self.mode.clone(),
                    Property::Damage => next.damage = self.damage.clone(),
                    Property::Crtc if object == Object::Connector => next.connected = value != 0,
                    Property::Crtc => next.plane_crtc = value != 0,
                    Property::SourceX => next.source[0] = value as u32,
                    Property::SourceY => next.source[1] = value as u32,
                    Property::SourceWidth => next.source[2] = value as u32,
                    Property::SourceHeight => next.source[3] = value as u32,
                    Property::CrtcX => next.target.0 = value as i32,
                    Property::CrtcY => next.target.1 = value as i32,
                    Property::CrtcWidth => next.target.2 = value as u32,
                    Property::CrtcHeight => next.target.3 = value as u32,
                    Property::Active => next.active = value != 0,
                    Property::PlaneType => unreachable!("immutable property passed validation"),
                }
            }
        }
        next.validate()?;
        let previous = current
            .cloned()
            .unwrap_or_else(|| Configuration::empty(owner));
        if !allow_modeset
            && (previous.active != next.active
                || previous.connected != next.connected
                || previous.native_mode() != next.native_mode())
        {
            return Err(DrmError::Invalid);
        }
        Ok(next)
    }
}
