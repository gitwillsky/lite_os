//! 用实际 OPEN codec 驱动协议 owner；格式夹具不能证明状态/计时/撤回动作的组合与时序。
use ::bgp::{Config, Effect, Input, Notice, Session, State, prefer_outgoing};
use core::net::Ipv4Addr;
use packet_wire::bgp::{
    self, Frame, Kind, Message,
    open::{Open, Parameter},
};
const SECOND: u64 = 1_000_000_000;
fn config() -> Config {
    Config {
        local_as: 65000,
        confederation_peer: None,
        peer_as: 65001,
        identifier: Ipv4Addr::new(192, 0, 2, 1),
        hold_seconds: 9,
        retry_seconds: 5,
        passive: false,
        ipv4_unicast: true,
        ipv6_unicast: true,
        route_refresh: true,
        extended_messages: false,
    }
}
fn remote<'a>(
    bytes: &'a mut [u8],
    asn: u16,
    hold: u16,
    id: Ipv4Addr,
    capabilities: &[u8],
) -> Frame<'a> {
    let length = Open::emit(
        bytes,
        asn,
        hold,
        id,
        &[Parameter {
            kind: 2,
            data: capabilities,
        }],
    )
    .unwrap();
    Frame::parse(&bytes[..length], false).unwrap()
}
fn connected(session: &mut Session) {
    assert!(
        session
            .on(0, Input::Start)
            .into_iter()
            .eq([Effect::Connect])
    );
    assert!(
        session
            .on(SECOND, Input::Connected(::bgp::OpenOffer::Capabilities))
            .into_iter()
            .eq([Effect::SendOpen])
    );
}
fn established(session: &mut Session) {
    connected(session);
    let mut bytes = [0; 128];
    let frame = remote(
        &mut bytes,
        65001,
        30,
        Ipv4Addr::new(192, 0, 2, 2),
        &[
            65, 4, 0, 0, 0xfd, 0xe9, 2, 0, 1, 4, 0, 1, 0, 1, 1, 4, 0, 2, 0, 1,
        ],
    );
    assert!(
        session
            .on(2 * SECOND, Input::Frame(frame))
            .into_iter()
            .eq([Effect::SendKeepalive])
    );
    assert_eq!(session.state(), State::OpenConfirm);
    let peer = session.negotiated().unwrap();
    assert!(!peer.send_ipv4_via_ipv6 && peer.receive_ipv4_via_ipv6);
    assert_eq!(
        session
            .on(3 * SECOND, Input::Sent(Kind::Keepalive))
            .into_iter()
            .count(),
        0
    );
    let mut keepalive = [0; 19];
    bgp::emit(&mut keepalive, Kind::Keepalive, false).unwrap();
    assert!(
        session
            .on(
                4 * SECOND,
                Input::Frame(Frame::parse(&keepalive, false).unwrap())
            )
            .into_iter()
            .eq([Effect::Established])
    );
    assert_eq!(session.state(), State::Established);
}

#[test]
fn handshake_timers_start_on_actual_send_and_timeout_withdraws_before_retry() {
    let mut session = Session::new(config()).unwrap();
    established(&mut session);
    assert_eq!(session.next_deadline(), Some(6 * SECOND));
    // 重复就绪不能关闭当前连接，发送 UPDATE 后沿同一计时器推迟 KEEPALIVE。
    assert_eq!(
        session
            .on(5 * SECOND, Input::Connected(::bgp::OpenOffer::Capabilities))
            .into_iter()
            .count(),
        0
    );
    assert!(
        session
            .on(6 * SECOND, Input::Timer)
            .into_iter()
            .eq([Effect::SendKeepalive])
    );
    assert_eq!(session.next_deadline(), Some(13 * SECOND));
    assert_eq!(session.on(6 * SECOND, Input::Timer).into_iter().count(), 0);
    session.on(6 * SECOND, Input::Sent(Kind::Keepalive));
    session.on(7 * SECOND, Input::Sent(Kind::Update));
    assert_eq!(session.next_deadline(), Some(10 * SECOND));
    assert_eq!(session.on(9 * SECOND, Input::Timer).into_iter().count(), 0);
    let mut keepalive = [0; 19];
    bgp::emit(&mut keepalive, Kind::Keepalive, false).unwrap();
    session.on(
        9 * SECOND,
        Input::Frame(Frame::parse(&keepalive, false).unwrap()),
    );
    assert!(
        session
            .on(10 * SECOND, Input::Timer)
            .into_iter()
            .eq([Effect::SendKeepalive])
    );
    // 发送阻塞后不忙转保活期限；HoldTimer 仍会终止当前连接并要求撤回。
    assert_eq!(session.next_deadline(), Some(18 * SECOND));
    let effects = session
        .on(18 * SECOND, Input::Timer)
        .into_iter()
        .collect::<Vec<_>>();
    assert_eq!(
        effects,
        [Effect::Close {
            received: None,
            notice: Some(Notice {
                code: 4,
                subcode: 0,
                data: &[]
            }),
            withdraw_routes: true,
            send_extended: false,
        }]
    );
    assert_eq!(session.state(), State::Idle);
    assert_eq!(session.negotiated(), None);
    assert_eq!(session.next_deadline(), Some(23 * SECOND));
    assert!(
        session
            .on(23 * SECOND, Input::Timer)
            .into_iter()
            .eq([Effect::Connect])
    );
    assert_eq!(session.state(), State::Connect);
}

#[test]
fn passive_stop_and_zero_hold_do_not_leave_polling_or_stale_renewal_work() {
    let mut policy = config();
    policy.passive = true;
    policy.hold_seconds = 0;
    let mut session = Session::new(policy).unwrap();
    assert_eq!(session.on(0, Input::Start).into_iter().count(), 0);
    assert_eq!(session.state(), State::Active);
    assert_eq!(session.next_deadline(), None);
    assert_eq!(
        session.on(100 * SECOND, Input::Timer).into_iter().count(),
        0
    );
    assert!(
        session
            .on(
                101 * SECOND,
                Input::Connected(::bgp::OpenOffer::Capabilities)
            )
            .into_iter()
            .eq([Effect::SendOpen])
    );
    assert_eq!(session.next_deadline(), Some(341 * SECOND));
    let mut bytes = [0; 128];
    let frame = remote(&mut bytes, 65001, 9, Ipv4Addr::new(192, 0, 2, 2), &[]);
    assert!(
        session
            .on(102 * SECOND, Input::Frame(frame))
            .into_iter()
            .eq([Effect::SendKeepalive])
    );
    session.on(103 * SECOND, Input::Sent(Kind::Keepalive));
    assert_eq!(session.next_deadline(), None);
    assert_eq!(
        session.on(1000 * SECOND, Input::Timer).into_iter().count(),
        0
    );
    assert!(
        session
            .on(1001 * SECOND, Input::TransportLost)
            .into_iter()
            .eq([Effect::Close {
                received: None,
                notice: None,
                withdraw_routes: false,
                send_extended: false,
            }])
    );
    assert!(session.next_deadline().is_some());
    session.on(1002 * SECOND, Input::Stop);
    assert_eq!(session.next_deadline(), None);
    assert_eq!(
        session.on(2000 * SECOND, Input::Timer).into_iter().count(),
        0
    );
}

#[test]
fn open_negotiates_real_as_direction_capabilities_and_rfc6286_identifiers() {
    let mut policy = config();
    policy.peer_as = 65537;
    policy.extended_messages = true;
    let mut session = Session::new(policy).unwrap();
    connected(&mut session);
    let mut bytes = [0; 128];
    // RFC 6793 用 capability AS 取代 My AS；重复相同值允许，未知 capability 忽略。
    let frame = remote(
        &mut bytes,
        23456,
        6,
        Ipv4Addr::new(224, 0, 0, 1),
        &[
            65, 4, 0, 1, 0, 1, 65, 4, 0, 1, 0, 1, 1, 4, 0, 2, 0, 1, 6, 0, 200, 0,
            5, 6, 0, 1, 0, 1, 0, 2,
        ],
    );
    assert!(
        session
            .on(2 * SECOND, Input::Frame(frame))
            .into_iter()
            .eq([Effect::SendKeepalive])
    );
    let peer = session.negotiated().unwrap();
    assert_eq!(
        (peer.peer_as, peer.hold_seconds, peer.four_octet_asn),
        (65537, 6, true)
    );
    assert!(!peer.ipv4_unicast && peer.ipv6_unicast);
    assert!(!peer.send_ipv4_via_ipv6 && !peer.receive_ipv4_via_ipv6);
    assert!(peer.send_extended && peer.receive_extended);
    assert!(!peer.send_refresh && peer.receive_refresh);
    assert!(!prefer_outgoing(policy, peer));
    let mut output = [0; 128];
    policy.local_as = 65538;
    let length = policy
        .emit_open(::bgp::OpenOffer::Capabilities, &mut output)
        .unwrap();
    let Message::Open(open) = Frame::parse(&output[..length], false)
        .unwrap()
        .message()
        .unwrap()
    else {
        panic!("local OPEN")
    };
    assert_eq!(open.asn(), 23456);
    assert!(open.parameters().flat_map(|parameter| parameter.capabilities().unwrap())
        .any(|capability| capability == packet_wire::bgp::open::IPV4_VIA_IPV6));
    assert!(
        open.parameters()
            .flat_map(|parameter| parameter.capabilities().unwrap())
            .any(|capability| capability.code == 65 && capability.data == 65538u32.to_be_bytes())
    );
    // 同 ID 的 eBGP 按实际四字节 AS 解决碰撞，不按截断的 My AS 或地址的单播类别判定。
    let mut same = peer;
    same.peer_identifier = policy.identifier;
    assert!(prefer_outgoing(policy, same));
    // 协商先退休后，关闭命令仍携发送方向授权，不能借查询 None 而漏发/截断扩展错误数据。
    let data = vec![0; 5000];
    let Effect::Close {
        received: None,
        notice: Some(notice),
        withdraw_routes: false,
        send_extended,
    } = session
        .on(
            3 * SECOND,
            Input::ProtocolError(Notice {
                code: 3,
                subcode: 1,
                data: &data,
            }),
        )
        .into_iter()
        .next()
        .unwrap()
    else {
        panic!("close command")
    };
    assert!(send_extended);
    assert_eq!(session.negotiated(), None);
    let mut output = vec![0; 5021];
    let length = notice.emit(&mut output, send_extended).unwrap();
    assert_eq!(length, 5021);
    assert!(
        matches!(Frame::parse(&output, true).unwrap().message(), Ok(Message::Notification { data, .. }) if data.len() == 5000)
    );
}

#[test]
fn rejected_open_returns_standard_error_and_exact_unknown_parameter_wire() {
    for (peer_as, id, capabilities, subcode) in [
        (65002, Ipv4Addr::new(192, 0, 2, 2), &[][..], 2),
        (65001, Ipv4Addr::UNSPECIFIED, &[][..], 3),
        (65001, Ipv4Addr::new(192, 0, 2, 2), &[65, 1, 0][..], 0),
    ] {
        let mut session = Session::new(config()).unwrap();
        connected(&mut session);
        let mut bytes = [0; 128];
        let frame = remote(&mut bytes, peer_as, 30, id, capabilities);
        assert!(
            session
                .on(2 * SECOND, Input::Frame(frame))
                .into_iter()
                .eq([Effect::Close {
                    received: None,
                    notice: Some(Notice {
                        code: 2,
                        subcode,
                        data: &[]
                    }),
                    withdraw_routes: false,
                    send_extended: false,
                }])
        );
        assert_eq!(session.negotiated(), None);
    }
    let mut session = Session::new(config()).unwrap();
    connected(&mut session);
    let mut bytes = [0; 128];
    let length = Open::emit(
        &mut bytes,
        65001,
        30,
        Ipv4Addr::new(192, 0, 2, 2),
        &[Parameter {
            kind: 200,
            data: &[1, 2],
        }],
    )
    .unwrap();
    let effects = session.on(
        2 * SECOND,
        Input::Frame(Frame::parse(&bytes[..length], false).unwrap()),
    );
    let Effect::Close {
        received: None,
        notice: Some(notice),
        withdraw_routes: false,
        send_extended: false,
    } = effects.into_iter().next().unwrap()
    else {
        panic!("OPEN Error")
    };
    assert_eq!(
        notice,
        Notice {
            code: 2,
            subcode: 4,
            data: &[200, 2, 1, 2]
        }
    );
    let mut output = [0xa5; 64];
    let count = notice.emit(&mut output, false).unwrap();
    assert_eq!(
        Frame::parse(&output[..count], false).unwrap().message(),
        Ok(Message::Notification {
            code: 2,
            subcode: 4,
            data: &[200, 2, 1, 2]
        })
    );
    for (version, hold, subcode, data) in [(5, 9, 1, &[0, 4][..]), (4, 1, 6, &[][..])] {
        let mut session = Session::new(config()).unwrap();
        connected(&mut session);
        let mut bytes = [0; 128];
        let frame = remote(&mut bytes, 65001, 9, Ipv4Addr::new(192, 0, 2, 2), &[]);
        let length = frame.bytes().len();
        bytes[19] = version;
        bytes[22..24].copy_from_slice(&u16::to_be_bytes(hold));
        assert!(
            session
                .on(
                    2 * SECOND,
                    Input::Frame(Frame::parse(&bytes[..length], false).unwrap())
                )
                .into_iter()
                .eq([Effect::Close {
                    received: None,
                    notice: Some(Notice {
                        code: 2,
                        subcode,
                        data
                    }),
                    withdraw_routes: false,
                    send_extended: false,
                }])
        );
    }
}

#[test]
fn established_updates_notifications_and_collision_cleanup_share_one_owner() {
    let mut session = Session::new(config()).unwrap();
    established(&mut session);
    let mut update = [0; 23];
    bgp::emit(&mut update, Kind::Update, false).unwrap();
    assert!(matches!(
        session
            .on(
                5 * SECOND,
                Input::Frame(Frame::parse(&update, false).unwrap())
            )
            .into_iter()
            .next(),
        Some(Effect::Update(_))
    ));
    let mut refresh = [0; 23];
    refresh[19..].copy_from_slice(&[0, 2, 0, 1]);
    bgp::emit(&mut refresh, Kind::RouteRefresh, false).unwrap();
    assert!(
        session
            .on(
                5 * SECOND,
                Input::Frame(Frame::parse(&refresh, false).unwrap())
            )
            .into_iter()
            .eq([Effect::Refresh {
                family: bgp::nlri::Family::Ipv6,
                enhanced: false
            }])
    );
    refresh[20] = 100;
    assert_eq!(
        session
            .on(
                5 * SECOND,
                Input::Frame(Frame::parse(&refresh, false).unwrap())
            )
            .into_iter()
            .count(),
        0
    );
    // 已建立的连接不参与新连接碰撞输家关闭。
    assert_eq!(
        session
            .on(5 * SECOND, Input::CollisionLost)
            .into_iter()
            .count(),
        0
    );
    let mut notification = [0; 21];
    bgp::emit(&mut notification, Kind::Notification, false).unwrap();
    assert!(
        session
            .on(
                5 * SECOND,
                Input::Frame(Frame::parse(&notification, false).unwrap())
            )
            .into_iter()
            .eq([Effect::Close {
                received: Some(Notice {
                    code: 0,
                    subcode: 0,
                    data: &[]
                }),
                notice: None,
                withdraw_routes: true,
                send_extended: false,
            }])
    );
    assert_eq!(
        session
            .on(6 * SECOND, Input::TransportLost)
            .into_iter()
            .count(),
        0
    );
    session.on(6 * SECOND, Input::Stop);
    assert_eq!(session.next_deadline(), None);
    let mut loser = Session::new(config()).unwrap();
    connected(&mut loser);
    assert!(
        loser
            .on(2 * SECOND, Input::CollisionLost)
            .into_iter()
            .eq([Effect::Close {
                received: None,
                notice: Some(Notice {
                    code: 6,
                    subcode: 7,
                    data: &[]
                }),
                withdraw_routes: false,
                send_extended: false,
            }])
    );
    assert_eq!(loser.next_deadline(), None);
    let mut unexpected = Session::new(config()).unwrap();
    connected(&mut unexpected);
    assert!(
        unexpected
            .on(
                2 * SECOND,
                Input::Frame(Frame::parse(&update, false).unwrap())
            )
            .into_iter()
            .eq([Effect::Close {
                received: None,
                notice: Some(Notice {
                    code: 5,
                    subcode: 1,
                    data: &[2]
                }),
                withdraw_routes: false,
                send_extended: false,
            }])
    );
    let mut session = Session::new(config()).unwrap();
    established(&mut session);
    let mut large = vec![0; 5000];
    bgp::emit(&mut large, Kind::Update, true).unwrap();
    assert!(
        session
            .on(
                5 * SECOND,
                Input::Frame(Frame::parse(&large, true).unwrap())
            )
            .into_iter()
            .eq([Effect::Close {
                received: None,
                notice: Some(Notice {
                    code: 1,
                    subcode: 2,
                    data: &[0x13, 0x88]
                }),
                withdraw_routes: true,
                send_extended: false,
            }])
    );
}

#[test]
fn optional_parameter_rejection_retries_a_real_basic_open_and_receives_two_octet_paths() {
    use ::bgp::OpenOffer;
    use packet_wire::bgp::{
        attributes::{Attribute, Attributes},
        nlri::{Family, Nlri},
        update::Update,
    };
    // 原夹具只发送正常带能力的 OPEN，无法捕获“重试仍声明旧能力”或误用四字节路径的回归。
    let mut policy = config();
    policy.local_as = 70000;
    policy.extended_messages = true;
    let mut session = Session::new(policy).unwrap();
    connected(&mut session);
    let refusal = Notice {
        code: 2,
        subcode: 4,
        data: &[2, 0],
    };
    let mut bytes = [0; 256];
    let size = refusal.emit(&mut bytes, false).unwrap();
    let effects = session
        .on(
            2 * SECOND,
            Input::Frame(Frame::parse(&bytes[..size], false).unwrap()),
        )
        .into_iter()
        .collect::<Vec<_>>();
    assert_eq!(
        effects,
        [
            Effect::Close {
                notice: None,
                received: Some(refusal),
                withdraw_routes: false,
                send_extended: false
            },
            Effect::OpenRejected {
                retry: Some(OpenOffer::BasicIpv4)
            },
        ]
    );
    assert_eq!(session.next_deadline(), Some(7 * SECOND));
    assert_eq!(session.on(3 * SECOND, Input::Timer).into_iter().count(), 0);
    assert!(
        session
            .on(7 * SECOND, Input::Timer)
            .into_iter()
            .eq([Effect::Connect])
    );
    assert!(
        session
            .on(8 * SECOND, Input::Connected(OpenOffer::BasicIpv4))
            .into_iter()
            .eq([Effect::SendOpen])
    );
    let size = policy.emit_open(session.open_offer(), &mut bytes).unwrap();
    assert_eq!(size, 29);
    let Message::Open(open) = Frame::parse(&bytes[..size], false)
        .unwrap()
        .message()
        .unwrap()
    else {
        panic!("OPEN")
    };
    assert_eq!(open.asn(), 23456);
    assert_eq!(open.parameters().count(), 0);
    let size = Open::emit(&mut bytes, 65001, 30, "192.0.2.2".parse().unwrap(), &[]).unwrap();
    assert!(
        session
            .on(
                9 * SECOND,
                Input::Frame(Frame::parse(&bytes[..size], false).unwrap())
            )
            .into_iter()
            .eq([Effect::SendKeepalive])
    );
    let peer = session.negotiated().unwrap();
    assert!(peer.ipv4_unicast);
    assert!(!peer.ipv6_unicast && !peer.four_octet_asn);
    assert!(!peer.send_ipv4_via_ipv6 && !peer.receive_ipv4_via_ipv6);
    assert!(
        !peer.send_refresh
            && !peer.receive_refresh
            && !peer.send_extended
            && !peer.receive_extended
    );
    bgp::emit(&mut bytes[..19], Kind::Keepalive, false).unwrap();
    assert!(
        session
            .on(
                10 * SECOND,
                Input::Frame(Frame::parse(&bytes[..19], false).unwrap())
            )
            .into_iter()
            .eq([Effect::Established])
    );
    let mut content = [0; 128];
    let size = Attributes::emit(
        &mut content,
        &[
            Attribute {
                flags: 0x40,
                code: 1,
                data: &[0],
            },
            Attribute {
                flags: 0x40,
                code: 2,
                data: &[2, 1, 0xfd, 0xe9],
            },
            Attribute {
                flags: 0x40,
                code: 3,
                data: &[192, 0, 2, 2],
            },
        ],
    )
    .unwrap();
    let size = Update::emit(
        &mut bytes,
        Nlri::parse(&[], Family::Ipv4).unwrap(),
        Attributes::parse(&content[..size]).unwrap(),
        Nlri::parse(&[24, 198, 51, 100], Family::Ipv4).unwrap(),
        false,
    )
    .unwrap();
    let Effect::Update(route) = session
        .on(
            11 * SECOND,
            Input::Frame(Frame::parse(&bytes[..size], false).unwrap()),
        )
        .into_iter()
        .next()
        .unwrap()
    else {
        panic!("UPDATE")
    };
    assert!(
        route
            .path()
            .unwrap()
            .segments()
            .flat_map(|segment| segment.asns())
            .eq([65001])
    );
    assert_eq!(session.state(), State::Established);

    // 对方单方向仍发送能力，也不能放开本地从未声明的扩展消息/刷新或四字节路径。
    let mut other = Session::new(policy).unwrap();
    other.on(0, Input::Start);
    other.on(SECOND, Input::Connected(OpenOffer::BasicIpv4));
    let frame = remote(
        &mut bytes,
        65001,
        30,
        "192.0.2.2".parse().unwrap(),
        &[65, 4, 0, 0, 0xfd, 0xe9, 2, 0, 6, 0],
    );
    other.on(2 * SECOND, Input::Frame(frame));
    let peer = other.negotiated().unwrap();
    assert!(
        !peer.four_octet_asn
            && !peer.send_refresh
            && !peer.receive_refresh
            && !peer.send_extended
            && !peer.receive_extended
    );
}

#[test]
fn incompatible_or_repeated_capability_rejection_stops_retry_and_reports_required_family() {
    use ::bgp::OpenOffer;
    for (offer, ipv4, subcode) in [
        (OpenOffer::Capabilities, true, 7),
        (OpenOffer::BasicIpv4, true, 4),
        (OpenOffer::Capabilities, false, 4),
    ] {
        let mut policy = config();
        policy.ipv4_unicast = ipv4;
        let mut session = Session::new(policy).unwrap();
        session.on(0, Input::Start);
        session.on(SECOND, Input::Connected(offer));
        let notice = Notice {
            code: 2,
            subcode,
            data: &[],
        };
        let mut bytes = [0; 128];
        let size = notice.emit(&mut bytes, false).unwrap();
        assert!(
            session
                .on(
                    2 * SECOND,
                    Input::Frame(Frame::parse(&bytes[..size], false).unwrap())
                )
                .into_iter()
                .eq([
                    Effect::Close {
                        notice: None,
                        received: Some(notice),
                        withdraw_routes: false,
                        send_extended: false
                    },
                    Effect::OpenRejected { retry: None },
                ])
        );
        assert_eq!(session.state(), State::Idle);
        assert_eq!(session.next_deadline(), None);
        assert_eq!(
            session.on(100 * SECOND, Input::Timer).into_iter().count(),
            0
        );
    }
    for (offer, ipv4, ipv6) in [
        (OpenOffer::Capabilities, true, false),
        (OpenOffer::Capabilities, false, true),
        (OpenOffer::BasicIpv4, true, true),
    ] {
        let mut policy = config();
        policy.ipv4_unicast = ipv4;
        policy.ipv6_unicast = ipv6;
        let mut session = Session::new(policy).unwrap();
        session.on(0, Input::Start);
        session.on(SECOND, Input::Connected(offer));
        let mut bytes = [0; 128];
        let frame = remote(
            &mut bytes,
            65001,
            30,
            "192.0.2.2".parse().unwrap(),
            if ipv4 {
                &[1, 4, 0, 2, 0, 1]
            } else {
                &[1, 4, 0, 1, 0, 1]
            },
        );
        let required = if ipv4 {
            &[1, 4, 0, 1, 0, 1]
        } else {
            &[1, 4, 0, 2, 0, 1]
        };
        let effects = session
            .on(2 * SECOND, Input::Frame(frame))
            .into_iter()
            .collect::<Vec<_>>();
        assert_eq!(
            effects,
            [
                Effect::Close {
                    notice: Some(Notice {
                        code: 2,
                        subcode: 7,
                        data: required
                    }),
                    received: None,
                    withdraw_routes: false,
                    send_extended: false
                },
                Effect::OpenRejected { retry: None },
            ]
        );
        assert_eq!(session.negotiated(), None);
        assert_eq!(session.next_deadline(), None);
        assert_eq!(
            session.on(100 * SECOND, Input::Timer).into_iter().count(),
            0
        );
        if !ipv4 {
            let mut output = [0x55; 128];
            assert!(policy.emit_open(OpenOffer::BasicIpv4, &mut output).is_err());
            assert_eq!(output, [0x55; 128]);
        }
    }
}
