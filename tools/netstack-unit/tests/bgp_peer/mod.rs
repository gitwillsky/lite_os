//! 通过生产 OPEN/Session 建立邻居的夹具；UPDATE 格式错误与接收表用例共享同一对端路径。
use ::bgp::{Config, Effect, Input, ReceivedUpdate, Session, State};
use core::net::Ipv4Addr;
use packet_wire::bgp::{
    self, Frame, Kind,
    attributes::{Attribute, Attributes},
    open::{Open, Parameter},
};

pub(super) fn established(internal: bool, four: bool) -> Session {
    let config = Config {
        local_as: if internal { 65001 } else { 65000 },
        peer_as: 65001,
        identifier: Ipv4Addr::new(192, 0, 2, 1),
        hold_seconds: 90,
        retry_seconds: 5,
        passive: false,
        ipv4_unicast: true,
        ipv6_unicast: true,
        route_refresh: true,
        extended_messages: false,
    };
    let mut session = Session::new(config).unwrap();
    session.on(0, Input::Start);
    session.on(1, Input::Connected);
    let mut bytes = [0; 128];
    let capabilities = if four {
        &[1, 4, 0, 1, 0, 1, 1, 4, 0, 2, 0, 1, 65, 4, 0, 0, 0xfd, 0xe9][..]
    } else {
        &[1, 4, 0, 1, 0, 1, 1, 4, 0, 2, 0, 1][..]
    };
    let length = Open::emit(
        &mut bytes,
        65001,
        90,
        Ipv4Addr::new(192, 0, 2, 2),
        &[Parameter {
            kind: 2,
            data: capabilities,
        }],
    )
    .unwrap();
    session.on(
        2,
        Input::Frame(Frame::parse(&bytes[..length], false).unwrap()),
    );
    let mut keepalive = [0; 19];
    bgp::emit(&mut keepalive, Kind::Keepalive, false).unwrap();
    session.on(3, Input::Frame(Frame::parse(&keepalive, false).unwrap()));
    assert_eq!(session.state(), State::Established);
    session
}
pub(super) fn base(internal: bool, four: bool) -> Vec<Attribute<'static>> {
    let mut attributes = vec![
        Attribute {
            flags: 0x40,
            code: 1,
            data: &[0],
        },
        Attribute {
            flags: 0x40,
            code: 2,
            data: if four {
                &[2, 1, 0, 0, 0xfd, 0xe9]
            } else {
                &[2, 1, 0xfd, 0xe9]
            },
        },
        Attribute {
            flags: 0x40,
            code: 3,
            data: &[192, 0, 2, 2],
        },
    ];
    if internal {
        attributes.push(Attribute {
            flags: 0x40,
            code: 5,
            data: &[0, 0, 0, 100],
        });
    }
    attributes
}
pub(super) fn attributes(input: &[Attribute<'_>]) -> Vec<u8> {
    let mut bytes = vec![0; input.iter().map(|attribute| 4 + attribute.data.len()).sum()];
    let length = Attributes::emit(&mut bytes, input).unwrap();
    bytes.truncate(length);
    bytes
}
pub(super) fn update(withdrawn: &[u8], attributes: &[u8], announced: &[u8]) -> Vec<u8> {
    // 独立构造两段长度，允许故意损坏属性 framing；不通过生产控制流注入测试分支。
    let mut bytes = vec![0; 23 + withdrawn.len() + attributes.len() + announced.len()];
    bytes[19..21].copy_from_slice(&(withdrawn.len() as u16).to_be_bytes());
    bytes[21..21 + withdrawn.len()].copy_from_slice(withdrawn);
    let at = 21 + withdrawn.len();
    bytes[at..at + 2].copy_from_slice(&(attributes.len() as u16).to_be_bytes());
    bytes[at + 2..at + 2 + attributes.len()].copy_from_slice(attributes);
    bytes[at + 2 + attributes.len()..].copy_from_slice(announced);
    bgp::emit(&mut bytes, Kind::Update, false).unwrap();
    bytes
}
pub(super) fn receive<'a>(session: &mut Session, bytes: &'a [u8]) -> ReceivedUpdate<'a> {
    let effect = session
        .on(4, Input::Frame(Frame::parse(bytes, false).unwrap()))
        .into_iter()
        .next()
        .unwrap();
    let Effect::Update(update) = effect else {
        panic!("expected update, got {effect:?}")
    };
    assert_eq!(session.state(), State::Established);
    update
}
