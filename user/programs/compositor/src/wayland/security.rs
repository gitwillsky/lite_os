//! security-context-v1 的待提交配置与 listener 所有权。来源是受信任的 Session 连接，metadata 只做能力收窄。

use super::client::{Client, Data, Entry, Fail};
use linux_uapi::unix::{PollEvents, PollFd, unix_listener_from_fd};
use std::{
    io,
    os::{
        fd::{AsFd, OwnedFd},
        unix::net::UnixListener,
    },
};
use wayland_wire::{
    protocol::{
        Interface, wp_security_context_manager_v1 as manager, wp_security_context_v1 as context,
    },
    wire::ObjectId,
};

/// 提交后独立于创建者连接存活；close fd 的 HUP 撤销接受新连接的能力，不终止既有客户端。
pub(super) struct Listener {
    pub listener: UnixListener,
    close: OwnedFd,
}

impl Listener {
    pub fn descriptors(&self) -> [PollFd<'_>; 2] {
        [
            PollFd::new(self.listener.as_fd(), PollEvents::READ),
            PollFd::new(self.close.as_fd(), PollEvents::EMPTY),
        ]
    }
    pub fn revoked(events: &[PollEvents]) -> bool {
        events[1].contains(PollEvents::HANGUP) || events[1].contains(PollEvents::ERROR)
    }
}

struct Pending {
    listener: Listener,
    engine: Option<String>,
    app: Option<String>,
    instance: Option<String>,
}

enum State {
    Pending(Pending),
    Committed,
}

/// 协议对象只拥有未提交的资源；commit 后唯一 listener owner 转移到 Server。
pub(super) struct Context {
    state: State,
}

impl Context {
    /// 接收已验证的 typed listener，建立唯一未提交状态；生产 caller 是 security_manager。
    pub(super) fn pending(listener: UnixListener, close: OwnedFd) -> Self {
        Self {
            state: State::Pending(Pending {
                listener: Listener { listener, close },
                engine: None,
                app: None,
                instance: None,
            }),
        }
    }
}

impl Client {
    pub(super) fn security_manager(
        &mut self,
        object: ObjectId,
        request: manager::Request,
    ) -> Result<(), Fail> {
        let manager::Request::CreateListener {
            id,
            listen_fd,
            close_fd,
        } = request
        else {
            return Ok(());
        };
        let invalid = |error: io::Error| {
            Fail::new(
                object,
                manager::Error::InvalidListenFd as u32,
                error.to_string(),
            )
        };
        let listener = unix_listener_from_fd(listen_fd).map_err(invalid)?;
        listener.set_nonblocking(true).map_err(invalid)?;
        self.create(
            id,
            Interface::WpSecurityContextV1,
            1,
            Data::SecurityContext(Context::pending(listener, close_fd)),
        )
    }

    pub(super) fn security_context(
        &mut self,
        object: ObjectId,
        request: context::Request,
    ) -> Result<(), Fail> {
        if matches!(request, context::Request::Destroy) {
            return Ok(());
        }
        let Some(Entry {
            data: Data::SecurityContext(context),
            ..
        }) = self.objects.get_mut(&object.get())
        else {
            return Err(Fail::implementation(object));
        };
        let State::Pending(pending) = &mut context.state else {
            return Err(Fail::new(
                object,
                context::Error::AlreadyUsed as u32,
                "security context has already been committed",
            ));
        };
        let set = |slot: &mut Option<String>, value: String| {
            if slot.is_some() {
                return Err(Fail::new(
                    object,
                    context::Error::AlreadySet as u32,
                    "security metadata was already set",
                ));
            }
            *slot = Some(value);
            Ok(())
        };
        match request {
            context::Request::SetSandboxEngine { name } => set(&mut pending.engine, name),
            context::Request::SetAppId { app_id } => set(&mut pending.app, app_id),
            context::Request::SetInstanceId { instance_id } => {
                set(&mut pending.instance, instance_id)
            }
            context::Request::Commit => {
                // Session 来源已拥有外壳能力；本 engine 只委派桌面子集，禁止委派新的 session manager。
                if pending.engine.as_deref() != Some("org.liteos.session")
                    || pending.app.as_deref() != Some("org.liteos.desktop")
                    || pending
                        .instance
                        .as_ref()
                        .is_none_or(|instance| instance.is_empty())
                {
                    return Err(Fail::new(
                        object,
                        context::Error::InvalidMetadata as u32,
                        "unsupported session security context",
                    ));
                }
                let State::Pending(pending) =
                    std::mem::replace(&mut context.state, State::Committed)
                else {
                    unreachable!("matched above")
                };
                self.security_contexts.push(pending.listener);
                Ok(())
            }
            context::Request::Destroy => unreachable!("handled before checking committed state"),
        }
    }

    pub(super) fn take_security_contexts(&mut self) -> Vec<Listener> {
        std::mem::take(&mut self.security_contexts)
    }
}
