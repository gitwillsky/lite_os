use alloc::{sync::Arc, vec::Vec};

use crate::{
    fs::{CharacterDevice, Epoll, OpenFileDescription, OpenFileKind},
    socket::{SocketWaitGuard, SocketWaitSource},
    task::PollWaitKey,
};

use super::{POLLHUP, POLLIN, POLLOUT};

/// @description 一次 poll/epoll wait publication 的唯一 transient source-key builder。
///
/// 所有 OFD 与嵌套 epoll source 直接追加到同一个摊销增长 Vec；caller 只负责选择
/// interest，不能为每个 interest 构造临时 key collection。
pub(in crate::syscall) struct PollWaitKeys {
    keys: Vec<PollWaitKey>,
    guards: Vec<PollWaitGuard>,
}

enum PollWaitGuard {
    Epoll { epoll: Arc<Epoll>, generation: u64 },
    Socket(SocketWaitGuard),
}

/// @description wait publication 前捕获的全部 source-snapshot guards。
pub(in crate::syscall) struct PollWaitGuards {
    entries: Vec<PollWaitGuard>,
}

impl PollWaitKeys {
    pub(in crate::syscall) const fn new() -> Self {
        Self {
            keys: Vec::new(),
            guards: Vec::new(),
        }
    }

    /// @description 加入使旧 epoll interest snapshot 失效的 ctl/close notification source。
    /// @param epoll notification owner。
    pub(in crate::syscall) fn add_epoll_change_source(
        &mut self,
        epoll: &Arc<Epoll>,
    ) -> Result<(), ()> {
        // Finish every fallible reserve before consuming the token. OOM then
        // cannot discard the only observable evidence for another caller.
        self.keys.try_reserve(1).map_err(|_| ())?;
        self.guards.try_reserve(1).map_err(|_| ())?;
        let generation = epoll.consume_notifications();
        let notification = epoll.notification_pipe();
        self.keys.push(PollWaitKey::pipe(
            &notification,
            crate::ipc::PipeDirection::Read,
            POLLIN,
            false,
            // A control-plane change invalidates every waiter snapshot. Keep
            // this source outside the data-plane wake-one group so one signal
            // detaches every stale membership before any waiter consumes the
            // coalesced token.
            None,
        ));
        self.guards.push(PollWaitGuard::Epoll {
            epoll: epoll.clone(),
            generation,
        });
        Ok(())
    }

    /// @description 把一个 OFD interest 递归展开为 source-native wait keys。
    /// @param events interest mask，包含 caller 所需 ERR/HUP policy。
    /// @param exclusive 该 interest 是否参与 EPOLLEXCLUSIVE wake-one。
    /// @param wake_group 同一顶层 epoll instance 的稳定 identity。
    pub(in crate::syscall) fn add_interest(
        &mut self,
        ofd: &Arc<OpenFileDescription>,
        events: i16,
        exclusive: bool,
        wake_group: Option<usize>,
    ) -> Result<(), ()> {
        match &ofd.kind {
            OpenFileKind::Character(CharacterDevice::Terminal { pty, .. }) => {
                if let Some(slave) = pty {
                    self.push(PollWaitKey::pipe(
                        &slave.notification_pipe(),
                        crate::ipc::PipeDirection::Read,
                        POLLIN,
                        exclusive,
                        wake_group,
                    ))?;
                    if events & POLLOUT != 0 {
                        self.push(PollWaitKey::pipe(
                            &slave.output_pipe(),
                            crate::ipc::PipeDirection::Write,
                            events,
                            exclusive,
                            wake_group,
                        ))?;
                    }
                } else {
                    self.push(PollWaitKey::console(events, exclusive, wake_group))?;
                }
            }
            OpenFileKind::Character(CharacterDevice::Input { file, .. }) => {
                self.push(PollWaitKey::pipe(
                    &file.notification_pipe(),
                    crate::ipc::PipeDirection::Read,
                    POLLIN,
                    exclusive,
                    wake_group,
                ))?;
            }
            OpenFileKind::Character(CharacterDevice::Drm(file)) => {
                self.push(PollWaitKey::pipe(
                    &file.notification_pipe(),
                    crate::ipc::PipeDirection::Read,
                    POLLIN,
                    exclusive,
                    wake_group,
                ))?;
            }
            OpenFileKind::Character(CharacterDevice::PtyMaster(master)) => {
                self.push(PollWaitKey::pipe(
                    &master.notification_pipe(),
                    crate::ipc::PipeDirection::Read,
                    POLLIN | POLLOUT | POLLHUP,
                    exclusive,
                    wake_group,
                ))?;
            }
            OpenFileKind::Pipe(endpoint) => {
                self.push(PollWaitKey::pipe(
                    &endpoint.pipe(),
                    endpoint.direction(),
                    events,
                    exclusive,
                    wake_group,
                ))?;
            }
            OpenFileKind::Socket(socket) => {
                let (sources, guard) = socket.wait_sources(events);
                if let Some(guard) = guard {
                    self.guards.try_reserve(1).map_err(|_| ())?;
                    self.guards.push(PollWaitGuard::Socket(guard));
                }
                for source in sources.into_iter().flatten() {
                    self.add_socket_source(source, events, exclusive, wake_group)?;
                }
            }
            OpenFileKind::Epoll(epoll) => {
                debug_assert!(!exclusive, "EPOLLEXCLUSIVE cannot target an epoll fd");
                self.add_epoll_change_source(epoll)?;
            }
            OpenFileKind::EventFd(event) => {
                if events & POLLIN != 0 {
                    self.push(PollWaitKey::pipe(
                        &event.notification_pipe(true),
                        crate::ipc::PipeDirection::Read,
                        POLLIN,
                        exclusive,
                        wake_group,
                    ))?;
                }
                if events & POLLOUT != 0 {
                    self.push(PollWaitKey::pipe(
                        &event.notification_pipe(false),
                        crate::ipc::PipeDirection::Read,
                        POLLOUT,
                        exclusive,
                        wake_group,
                    ))?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// @description 将 facade-provided socket source 追加到唯一 transient key backing。
    pub(super) fn add_socket_source(
        &mut self,
        source: SocketWaitSource,
        events: i16,
        exclusive: bool,
        wake_group: Option<usize>,
    ) -> Result<(), ()> {
        self.push(match source {
            SocketWaitSource::Notification(pipe) => PollWaitKey::pipe(
                &pipe,
                crate::ipc::PipeDirection::Read,
                POLLIN,
                exclusive,
                wake_group,
            ),
            SocketWaitSource::Data { pipe, direction } => {
                PollWaitKey::pipe(&pipe, direction, events, exclusive, wake_group)
            }
        })
    }

    /// @description 把完成的 key backing 与 snapshot guards 转移给 wait orchestration。
    pub(in crate::syscall) fn finish(self) -> (Vec<PollWaitKey>, PollWaitGuards) {
        (
            self.keys,
            PollWaitGuards {
                entries: self.guards,
            },
        )
    }

    fn push(&mut self, key: PollWaitKey) -> Result<(), ()> {
        self.keys.try_reserve(1).map_err(|_| ())?;
        self.keys.push(key);
        Ok(())
    }
}

impl PollWaitGuards {
    /// @description 在 registry owner lock 内排空 change token 并验证每份预建 snapshot。
    /// @return 任一 generation 已变化时返回 true；不分配、不 clone、不展开 source keys。
    pub(in crate::syscall) fn changed(&self) -> bool {
        let mut changed = false;
        for guard in &self.entries {
            // Do not short-circuit: drain every coalesced epoll token and
            // inspect every socket identity so the rebuild starts from one
            // stable source snapshot.
            changed |= match guard {
                PollWaitGuard::Epoll { epoll, generation } => epoll.recheck_changed(*generation),
                PollWaitGuard::Socket(socket) => socket.changed(),
            };
        }
        changed
    }
}
