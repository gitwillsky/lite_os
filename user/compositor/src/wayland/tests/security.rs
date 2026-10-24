use super::socket::{pump, read_frames, socket_path};
use super::*;
use crate::wayland::Server;
use std::{
    fs,
    os::unix::net::{UnixListener, UnixStream},
};
#[cfg(target_os = "linux")]
use wayland_wire::protocol::wp_security_context_manager_v1 as manager;
use wayland_wire::protocol::wp_security_context_v1 as context;

struct SocketPath(std::path::PathBuf);
impl Drop for SocketPath {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn setup(name: &str) -> (Harness, UnixStream, SocketPath) {
    let mut harness = Harness::privileged();
    harness
        .send(|writer| {
            wl_registry::Request::Bind {
                name: 8,
                id: DynamicNewId {
                    interface: "wp_security_context_manager_v1".into(),
                    version: 1,
                    id: id(60),
                },
            }
            .encode(id(2), writer)
            .unwrap()
        })
        .unwrap();
    let path = socket_path(name);
    let listener = UnixListener::bind(&path).unwrap();
    let path = SocketPath(path);
    let (notification, lease) = UnixStream::pair().unwrap();
    listener.set_nonblocking(true).unwrap();
    // 宿主不提供 Linux SO_ACCEPTCONN；元数据/所有权测试从 std 创建的真实 typed listener 开始。
    // 原始 fd 验证与 SCM_RIGHTS 收发由 Linux guest 的 session-probe 覆盖。
    harness
        .client
        .create(
            id(61),
            Interface::WpSecurityContextV1,
            1,
            super::super::client::Data::SecurityContext(super::super::security::Context::pending(
                listener,
                notification.into(),
            )),
        )
        .unwrap_or_else(|error| panic!("{}", error.message));
    (harness, lease, path)
}

fn metadata(harness: &mut Harness) {
    for request in [
        context::Request::SetSandboxEngine {
            name: "org.liteos.session".into(),
        },
        context::Request::SetAppId {
            app_id: "org.liteos.desktop".into(),
        },
        context::Request::SetInstanceId {
            instance_id: "test-session".into(),
        },
    ] {
        harness
            .send(|writer| request.encode(id(61), writer).unwrap())
            .unwrap();
    }
}

#[test]
fn metadata_is_set_once_and_commit_rejects_further_mutation() {
    let (mut harness, _lease, _path) = setup("context-metadata-once");
    metadata(&mut harness);
    assert!(
        harness
            .send(|writer| context::Request::SetAppId {
                app_id: "again".into()
            }
            .encode(id(61), writer)
            .unwrap())
            .is_err()
    );
    harness.expect_error(context::Error::AlreadySet as u32);
    let (mut harness, _lease, _path) = setup("context-commit-once");
    metadata(&mut harness);
    harness
        .send(|writer| context::Request::Commit.encode(id(61), writer).unwrap())
        .unwrap();
    assert_eq!(harness.client.take_security_contexts().len(), 1);
    assert!(
        harness
            .send(|writer| context::Request::Commit.encode(id(61), writer).unwrap())
            .is_err()
    );
    harness.expect_error(context::Error::AlreadyUsed as u32);
}

#[test]
fn incomplete_context_is_never_published_and_pending_destroy_drops_fds() {
    let (mut harness, _lease, _path) = setup("context-invalid-metadata");
    assert!(
        harness
            .send(|writer| context::Request::Commit.encode(id(61), writer).unwrap())
            .is_err()
    );
    harness.expect_error(context::Error::InvalidMetadata as u32);
    assert!(harness.client.take_security_contexts().is_empty());
    let (mut harness, _lease, _path) = setup("context-pending-destroy");
    harness
        .send(|writer| context::Request::Destroy.encode(id(61), writer).unwrap())
        .unwrap();
    assert!(harness.client.take_security_contexts().is_empty());
    assert!(!harness.client.objects.contains_key(&61));
}

#[cfg(target_os = "linux")]
#[test]
fn a_connected_socket_cannot_be_registered_as_a_listener() {
    let mut harness = Harness::privileged();
    harness
        .send(|writer| {
            wl_registry::Request::Bind {
                name: 8,
                id: DynamicNewId {
                    interface: "wp_security_context_manager_v1".into(),
                    version: 1,
                    id: id(60),
                },
            }
            .encode(id(2), writer)
            .unwrap()
        })
        .unwrap();
    let (connected, _peer) = UnixStream::pair().unwrap();
    let (notification, _lease) = UnixStream::pair().unwrap();
    let request = manager::Request::CreateListener {
        id: id(61),
        listen_fd: connected.as_fd().try_clone_to_owned().unwrap(),
        close_fd: notification.as_fd().try_clone_to_owned().unwrap(),
    };
    assert!(
        harness
            .send_with_fds(
                |writer| request.encode(id(60), writer).unwrap(),
                vec![connected.into(), notification.into()]
            )
            .is_err()
    );
    harness.expect_error(manager::Error::InvalidListenFd as u32);
}

#[test]
fn committed_listener_survives_its_creator_and_hangup_precedes_accept() {
    let public = socket_path("context-server");
    let mut server = Server::bind(&public, OUTPUT).unwrap();
    let (session, peer) = UnixStream::pair().unwrap();
    let owner = server.add_session(session).unwrap();
    let (mut harness, lease, delegated) = setup("context-transfer");
    // 独立状态机只用于准备真实 listener；提交后把唯一 capability 移入 Server。
    metadata(&mut harness);
    harness
        .send(|writer| context::Request::Commit.encode(id(61), writer).unwrap())
        .unwrap();
    let contexts = harness.client.take_security_contexts();
    server.with_client(owner, |client| client.security_contexts.extend(contexts));
    pump(&mut server);
    assert_eq!(server.context_count(), 1);
    drop(peer);
    pump(&mut server);
    assert_eq!(server.connection_count(), 0);
    assert_eq!(server.context_count(), 1);
    let mut client = UnixStream::connect(&delegated.0).unwrap();
    pump(&mut server);
    assert_eq!(server.connection_count(), 1);
    let mut writer = Writer::new();
    wl_display::Request::GetRegistry { registry: id(2) }
        .encode(ObjectId::DISPLAY, &mut writer)
        .unwrap();
    std::io::Write::write_all(&mut client, writer.bytes()).unwrap();
    let globals = read_frames(&mut client, &mut server, 7);
    assert!(globals.iter().any(|event| matches!(event, AnyEvent::WlRegistry(wl_registry::Event::Global { interface, .. }) if interface == "zwlr_layer_shell_v1")));
    assert!(!globals.iter().any(|event| matches!(event, AnyEvent::WlRegistry(wl_registry::Event::Global { interface, .. }) if interface == "wp_security_context_manager_v1")));
    // 同一轮既有新连接又有 close fd HUP：撤销优先，已接纳客户端不受影响。
    let _pending = UnixStream::connect(&delegated.0).unwrap();
    drop(lease);
    // 单元测试直接给出同轮语义快照；macOS poll(events=0) 不报告 Linux 的无条件 HUP。
    // guest 门禁另外要求真实 close_fd HUP 触发 Server 撤销。
    let mut returned = vec![linux_uapi::unix::PollEvents::EMPTY; server.descriptors().len()];
    returned[2] = linux_uapi::unix::PollEvents::READ;
    returned[3] = linux_uapi::unix::PollEvents::HANGUP;
    server.dispatch(&returned).unwrap();
    assert_eq!(server.context_count(), 0);
    assert_eq!(server.connection_count(), 1);
    drop(server);
    fs::remove_file(public).unwrap();
}

#[test]
fn a_failed_flush_cannot_leave_a_closing_connection_in_the_poll_set() {
    let path = socket_path("context-broken-output");
    let mut server = Server::bind(&path, OUTPUT).unwrap();
    let (session, peer) = UnixStream::pair().unwrap();
    let owner = server.add_session(session).unwrap();
    drop(peer);
    server.with_client(owner, |client| {
        let mut writer = Writer::new();
        wl_display::Request::Sync { callback: id(2) }
            .encode(ObjectId::DISPLAY, &mut writer)
            .unwrap();
        client.receive(writer.bytes(), vec![]).unwrap();
    });
    pump(&mut server);
    assert_eq!(server.connection_count(), 0);
    drop(server);
    fs::remove_file(path).unwrap();
}
