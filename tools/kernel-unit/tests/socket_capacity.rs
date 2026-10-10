#![feature(allocator_ext)]
#![allow(dead_code)]
//! 实际 capacity facade/owner/backlog/FIFO；endpoint transitions 是显式 producer 输入。
extern crate alloc;
#[path = "../../../kernel/src/fallible_tree/mod.rs"]
mod fallible_tree;
#[path = "../../../kernel/src/id.rs"]
mod id;
#[path = "../src/pipe_fixture.rs"]
mod ipc;
#[path = "../src/sync.rs"]
mod sync;
#[path = "../../../kernel/src/socket"]
mod socket {
    use alloc::sync::Arc;
    #[derive(Clone, Copy)]
    pub(crate) enum SocketError {
        Again,
        NoMemory,
    }
    mod wait_source;
    pub(crate) use wait_source::SocketWaitSource;
    mod capacity;
    use capacity::CapacityKind;
    pub(crate) use capacity::SocketCapacityWait;
    #[path = "unix"]
    mod unix {
        use alloc::sync::Arc;
        use spin::Mutex;
        mod capacity;
        mod connection_backlog;
        mod datagram_queue;
        pub(super) struct ProducedShutdown {
            read: bool,
            write: bool,
        }
        pub(super) type PacketShutdown = ProducedShutdown;
        enum SocketState {
            Stream {
                transmit: Option<()>,
            },
            Closed,
            Datagram {
                messages: datagram_queue::DatagramQueue<usize>,
                peer: Option<Arc<UnixSocket>>,
                shutdown: ProducedShutdown,
            },
            Listening {
                backlog: connection_backlog::ConnectionBacklog<usize>,
                shutdown: ProducedShutdown,
            },
        }
        pub(crate) struct UnixSocket {
            state: Mutex<SocketState>,
            notify_read: Arc<crate::ipc::PipeEnd>,
            _notify_write: Arc<crate::ipc::PipeEnd>,
        }
        impl UnixSocket {
            pub(super) fn node_id(&self) -> u64 {
                42
            }
            pub(super) fn send_memory_available(&self) -> bool {
                unreachable!("capacity producer has no sender memory input")
            }
            pub(crate) fn listener() -> Arc<Self> {
                let (read, write) = crate::ipc::Pipe::notification_pair().unwrap();
                Arc::new(Self {
                    state: Mutex::new(SocketState::Listening {
                        backlog: connection_backlog::ConnectionBacklog::new(0),
                        shutdown: ProducedShutdown {
                            read: false,
                            write: false,
                        },
                    }),
                    notify_read: read,
                    _notify_write: write,
                })
            }
            pub(crate) fn publish(&self) {
                let token = {
                    let mut state = self.state.lock();
                    let SocketState::Listening { backlog, .. } = &mut *state else {
                        panic!("producer state");
                    };
                    backlog.reserve().unwrap()
                };
                let Ok(node) = token.try_stage(1) else {
                    panic!("host allocation");
                };
                let mut state = self.state.lock();
                let SocketState::Listening { backlog, .. } = &mut *state else {
                    panic!("producer state");
                };
                backlog.commit(node);
            }
            pub(crate) fn accept(&self) {
                let retired = {
                    let mut state = self.state.lock();
                    let SocketState::Listening { backlog, .. } = &mut *state else {
                        panic!("producer state");
                    };
                    backlog.pop()
                };
                drop(retired);
            }
            pub(crate) fn close(&self) {
                let retired = {
                    let mut state = self.state.lock();
                    core::mem::replace(&mut *state, SocketState::Closed)
                };
                drop(retired);
            }
            pub(crate) fn replace_packet_state(&self) {
                let retired = {
                    let mut state = self.state.lock();
                    core::mem::replace(
                        &mut *state,
                        SocketState::Datagram {
                            messages: datagram_queue::DatagramQueue::new(),
                            peer: None,
                            shutdown: ProducedShutdown {
                                read: false,
                                write: false,
                            },
                        },
                    )
                };
                drop(retired);
            }
        }
    }
    pub(crate) use unix::UnixSocket;
    pub(crate) fn backlog(socket: Arc<UnixSocket>) -> SocketCapacityWait {
        SocketCapacityWait::new(socket, CapacityKind::Backlog)
    }
    pub(crate) fn packets(socket: Arc<UnixSocket>) -> SocketCapacityWait {
        SocketCapacityWait::new(socket, CapacityKind::PacketQueue { sender: 0 })
    }
}
#[test]
fn actual_listener_capacity_recheck_survives_accept_close_and_stale_notification() {
    let listener = socket::UnixSocket::listener();
    let wait = socket::backlog(listener.clone());
    assert!(wait.is_ready());
    listener.publish();
    assert!(!wait.is_ready());
    wait.prepare_wait();
    assert!(!wait.is_ready());
    listener.accept();
    assert!(wait.is_ready());
    listener.publish();
    listener.close();
    wait.prepare_wait();
    assert!(
        wait.is_ready(),
        "terminal owner must allow retry to report connection error"
    );
}
#[test]
fn actual_packet_capacity_recheck_returns_ready_after_owner_changes_or_closes() {
    let endpoint = socket::UnixSocket::listener();
    endpoint.replace_packet_state();
    let wait = socket::packets(endpoint.clone());
    assert!(wait.is_ready());
    endpoint.close();
    wait.prepare_wait();
    assert!(wait.is_ready());
}
