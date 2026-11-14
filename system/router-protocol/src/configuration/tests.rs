use super::*;
use alloc::vec;

#[test]
fn configuration_difference_keeps_ordered_arrays_null_and_escaped_object_names() {
    let mut before = Configuration::default();
    before
        .ensure_interface(b"eth0", Some([2, 0, 0, 0, 0, 1]))
        .unwrap();
    let profile = before.interfaces.pop_first().unwrap().1;
    let name = Name::try_from("uplink~/wan".to_string()).unwrap();
    before.interfaces.insert(name.clone(), profile);
    before.dns.search_list = vec!["first.test".into(), "second.test".into()];
    let mut after = before.clone();
    after.interfaces.get_mut(&name).unwrap().display_name = Some("Office".into());
    after.dns.search_list.reverse();
    let changes: serde_json::Value =
        serde_json::from_slice(&Configuration::difference_json(Some(&before), &after).unwrap())
            .unwrap();
    let changes = changes.as_array().unwrap();
    assert_eq!(changes.len(), 3);
    let display = changes
        .iter()
        .find(|change| change["path"] == "/interfaces/uplink~0~1wan/display_name")
        .unwrap();
    assert!(display.as_object().unwrap().contains_key("before"));
    assert_eq!(display["before"], serde_json::Value::Null);
    assert_eq!(display["after"], "Office");
    let search = changes
        .iter()
        .find(|change| change["path"] == "/dns/search_list/0")
        .unwrap();
    assert_eq!(search["after"], "second.test");
    after.interfaces.clear();
    let removed: serde_json::Value =
        serde_json::from_slice(&Configuration::difference_json(Some(&before), &after).unwrap())
            .unwrap();
    let removed = removed
        .as_array()
        .unwrap()
        .iter()
        .find(|change| change["path"] == "/interfaces/uplink~0~1wan")
        .unwrap();
    assert!(removed.as_object().unwrap().contains_key("before"));
    assert!(!removed.as_object().unwrap().contains_key("after"));

    before.dns.search_list.push("third.test".into());
    let mut after = before.clone();
    after.dns.search_list.remove(1);
    for (old, new, side) in [(&before, &after, "before"), (&after, &before, "after")] {
        let delta = Configuration::difference_json(Some(old), new).unwrap();
        let rows: Vec<serde_json::Value> = serde_json::from_slice(&delta).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["path"], "/dns/search_list/1");
        assert_eq!(rows[0][side], "second.test");
        assert_eq!(rows[0].as_object().unwrap().len(), 2);
    }
}

#[test]
fn ospf_review_masks_changed_keys_in_fields_and_whole_protocols() {
    let mut before = Configuration::default();
    before.ensure_interface(b"eth0", None).unwrap();
    let name = before.interfaces.keys().next().unwrap().clone();
    before.ospf = Some(
        serde_json::from_value(serde_json::json!({
            "router_id": "192.0.2.1",
            "links": { (name.as_str()): {
                "area": "0.0.0.0",
                "v2": { "hello": 10, "dead": 40 },
                "v3": { "timers": { "hello": 10, "dead": 40 } },
                "v2_authentication": { "send_id": 1, "keys": { "1": "fixture-v2-before" } },
                "v3_authentication": { "send_id": 2, "keys": { "2": "fixture-v3-before" } },
            } }
        }))
        .unwrap(),
    );
    let mut after = before.clone();
    let link = after.ospf.as_mut().unwrap().links.get_mut(&name).unwrap();
    let auth = link.v2_authentication.as_mut().unwrap();
    auth.keys.insert(
        1,
        crate::ospf::Secret::new("fixture-v2-after".into()).unwrap(),
    );

    let difference = |old: Option<&Configuration>, new: &Configuration| {
        let diff = Configuration::difference_json(old, new).unwrap();
        let diff = core::str::from_utf8(&diff).unwrap();
        assert!(!diff.contains("fixture-v"));
        serde_json::from_str::<Vec<serde_json::Value>>(diff).unwrap()
    };
    let changed = difference(Some(&before), &after);
    assert_eq!(changed.len(), 1);
    assert!(
        changed[0]["path"]
            .as_str()
            .unwrap()
            .ends_with("/v2_authentication/keys/1")
    );
    assert_eq!(changed[0]["before"], "[已隐藏]");
    assert_eq!(changed[0]["after"], "[已隐藏]");
    let added = difference(None, &before);
    let added = added.iter().find(|row| row["path"] == "/ospf").unwrap();
    assert_eq!(
        added["after"]["links"][name.as_str()]["v3_authentication"]["keys"]["2"],
        "[已隐藏]"
    );
    after.ospf = None;
    let removed = difference(Some(&before), &after);
    assert_eq!(
        removed[0]["before"]["links"][name.as_str()]["v2_authentication"]["keys"]["1"],
        "[已隐藏]"
    );
    assert!(
        core::str::from_utf8(&before.encode().unwrap())
            .unwrap()
            .contains("fixture-v2-before")
    );

    // 新增整个具名域时差异直接输出子树，认证密钥仍须遮蔽。
    let mut isolated = Configuration::default();
    let domain_name = Name::try_from("edge/~".to_string()).unwrap();
    let mac = [2, 0, 0, 0, 0, 2];
    let mut domain: RoutingDomain = serde_json::from_str("{}").unwrap();
    domain
        .interfaces
        .insert(name.clone(), Interface::new(DeviceMatch::Mac(mac)));
    domain.ipv4_forwarding = false;
    domain.ospf = before.ospf.clone();
    let session = domain.ospf_session().unwrap().unwrap();
    assert_eq!(
        session.links[&name].device,
        crate::ospf::Binding::Physical(DeviceMatch::Mac(mac))
    );
    assert_eq!(session.forwarding, [false, true]);
    assert!(session.needs_authentication_epoch());
    isolated.routing_domains.insert(domain_name.clone(), domain);
    let added = difference(None, &isolated);
    let row = added
        .iter()
        .find(|row| row["path"] == "/routing_domains")
        .unwrap();
    assert_eq!(
        row["after"][domain_name.as_str()]["ospf"]["links"][name.as_str()]["v3_authentication"]["keys"]
            ["2"],
        "[已隐藏]"
    );
    isolated
        .routing_domains
        .get_mut(&domain_name)
        .unwrap()
        .interfaces
        .clear();
    assert_eq!(
        isolated.validate().unwrap_err().path,
        "/routing_domains/edge~1~0/ospf/links/eth0"
    );
}

#[test]
fn configuration_round_trip_keeps_native_identity_and_full_dns_policy() {
    let mac = [2, 0, 0, 0, 0, 1];
    let mut configuration = Configuration::default();
    let profile = configuration.ensure_interface(b"eth0", Some(mac)).unwrap();
    let mut projection = InterfaceConfig::dhcp();
    projection.dns = (1..=8).map(|last| Ipv4Addr::new(10, 0, 0, last)).collect();
    profile.ipv4 = Some(Ipv4Configuration::from_projection(&projection));
    profile.ipv6 = Some(Ipv6Configuration {
        accept_ra: RouterAdvertisementPolicy::Always,
        advertise: RouterAdvertisements::Enabled {
            max_interval_seconds: 20,
            router_lifetime_seconds: 60,
            dns: AdvertisedDns::new(&["2001:db8::53".parse().unwrap()]).unwrap(),
        },
        addresses: vec![Ipv6Address {
            address: "2001:db8::1".parse().unwrap(),
            prefix_length: 64,
            on_link: false,
        }],
    });
    profile.mtu = 1280;
    profile.default_route = Some(crate::routing::Attributes {
        preference: 150,
        metric: 42,
    });
    let profile = configuration.interfaces.pop_first().unwrap().1;
    configuration.interfaces.insert(
        Name::try_from("楼上办公网与服务器网络".to_string()).unwrap(),
        profile,
    );
    configuration.dns.search_list = projection
        .dns
        .iter()
        .enumerate()
        .map(|(index, _)| format!("{}.zone{index}.test", "a".repeat(40)))
        .collect();
    configuration.routes.push(StaticRoute {
        destination: Ipv4Addr::new(10, 20, 0, 0).into(),
        prefix_length: 16,
        preference: 1,
        metric: 5,
        action: StaticAction::Forward {
            members: alloc::vec![
                StaticMember {
                    weight: 2,
                    target: StaticTarget::Direct {
                        interface: configuration.interfaces.keys().next().unwrap().clone(),
                        gateway: Some(Ipv4Addr::new(10, 0, 0, 1).into()),
                        on_link: false
                    }
                },
                StaticMember {
                    weight: 1,
                    target: StaticTarget::Recursive {
                        address: Ipv4Addr::new(192, 0, 2, 1).into()
                    }
                },
            ],
        },
    });
    configuration.bgp = Some(
        crate::bgp::Configuration::parse(
            br#"{
        "local_as": 4200000001, "identifier": "192.0.2.1", "redistribute":{"connected":true,"protocols":[4]}, "networks":[{"network":"203.0.113.0","length":24}],
        "neighbors": {"uplink/~1": {
            "address": "192.0.2.2", "asn": 4200000002, "preference": 20,
            "hold_seconds": 90, "retry_seconds": 5, "passive": false,
            "resolve_via_default": true,
            "md5_password": "fixture-shared-key",
            "import": {"default":"deny","preference":100,"rules":[{"as_path_regex":"^65001_","prefix":{"network":"198.51.100.0","length":24},"action":{"kind":"permit","preference":0,"as_path_prepend":[70000,65001,70000],"as_path_exclude":[70000,65001,70000],"as_path_replace":{"from":65001,"to":90000},"origin":"egp","med":{"kind":"set","value":0},"communities":{"replace":[2,1,2],"remove":[7],"add":[3,2,3]},"extended_communities":{"add":[[0,2,0,1,0,0,0,9]]},"large_communities":{"replace":[]}}}]},
            "export": {"default":"deny","rules":[{"prefix":{"network":"198.51.100.0","length":24},"action":{"kind":"permit"}}]},
            "advertisement_seconds": 0
        }}
    }"#,
        )
        .unwrap(),
    );
    configuration.ipv4_forwarding = false;
    let bytes = configuration.encode().unwrap();
    let loaded = Configuration::decode(&bytes).unwrap();
    assert_eq!(loaded, configuration);
    assert!(
        !format!("{loaded:?}").contains("fixture-shared-key"),
        "configuration Debug must redact authentication secrets"
    );
    let review = Configuration::difference_json(None, &configuration).unwrap();
    let review = core::str::from_utf8(&review).unwrap();
    assert!(review.contains("[已隐藏]") && !review.contains("fixture-shared-key"));
    let permit = &loaded
        .bgp
        .as_ref()
        .unwrap()
        .neighbors
        .values()
        .next()
        .unwrap()
        .import
        .as_ref()
        .unwrap()
        .rules[0]
        .action;
    let crate::bgp::RouteAction::Permit {
        as_path_exclude, ..
    } = permit
    else {
        unreachable!()
    };
    assert!(
        as_path_exclude
            .as_ref()
            .unwrap()
            .iter()
            .map(|asn| asn.get())
            .eq([65001, 70000])
    );
    // 角色、缺省簇与策略边界沿完整配置校验，不能留到建立连接后才发现。
    let mut reflector = loaded.clone();
    let bgp = reflector.bgp.as_mut().unwrap();
    let neighbor = bgp.neighbors.values_mut().next().unwrap();
    neighbor.route_reflector_client = true;
    assert_eq!(
        reflector.validate().unwrap_err().path,
        "/bgp/neighbors/uplink~1~01/route_reflector_client"
    );
    let bgp = reflector.bgp.as_mut().unwrap();
    let neighbor = bgp.neighbors.values_mut().next().unwrap();
    neighbor.asn = bgp.local_as;
    neighbor.import = None;
    assert_eq!(bgp.reflection_cluster(), Some(bgp.identifier));
    assert!(reflector.validate().is_ok());
    let bgp = reflector.bgp.as_mut().unwrap();
    bgp.cluster_id = Some("10.0.0.7".parse().unwrap());
    assert_eq!(bgp.reflection_cluster(), bgp.cluster_id);
    let action = &mut bgp
        .neighbors
        .values_mut()
        .next()
        .unwrap()
        .export
        .as_mut()
        .unwrap()
        .rules[0]
        .action;
    *action = crate::bgp::RouteAction::Permit {
        preference: Some(100),
        next_hop: None,
        origin: None,
        med: None,
        as_path_prepend: None,
        as_path_exclude: None,
        as_path_replace: None,
        communities: None,
        extended_communities: None,
        large_communities: None,
    };
    assert_eq!(
        reflector.validate().unwrap_err().path,
        "/bgp/neighbors/uplink~1~01/export/rules/0/action"
    );
    let action = &mut reflector
        .bgp
        .as_mut()
        .unwrap()
        .neighbors
        .values_mut()
        .next()
        .unwrap()
        .export
        .as_mut()
        .unwrap()
        .rules[0]
        .action;
    let crate::bgp::RouteAction::Permit {
        preference,
        as_path_prepend,
        ..
    } = action
    else {
        unreachable!()
    };
    *preference = None;
    *as_path_prepend =
        Some(alloc::vec![core::num::NonZeroU32::new(70000).unwrap()].into_boxed_slice());
    assert_eq!(
        reflector.validate().unwrap_err().path,
        "/bgp/neighbors/uplink~1~01/export/rules/0/action"
    );
    let action = &mut reflector
        .bgp
        .as_mut()
        .unwrap()
        .neighbors
        .values_mut()
        .next()
        .unwrap()
        .export
        .as_mut()
        .unwrap()
        .rules[0]
        .action;
    let crate::bgp::RouteAction::Permit {
        as_path_prepend,
        as_path_exclude,
        ..
    } = action
    else {
        unreachable!()
    };
    *as_path_prepend = None;
    *as_path_exclude =
        Some(alloc::vec![core::num::NonZeroU32::new(65001).unwrap()].into_boxed_slice());
    assert_eq!(
        reflector.validate().unwrap_err().path,
        "/bgp/neighbors/uplink~1~01/export/rules/0/action"
    );
    let action = &mut reflector
        .bgp
        .as_mut()
        .unwrap()
        .neighbors
        .values_mut()
        .next()
        .unwrap()
        .export
        .as_mut()
        .unwrap()
        .rules[0]
        .action;
    let crate::bgp::RouteAction::Permit {
        as_path_exclude, ..
    } = action
    else {
        unreachable!()
    };
    *as_path_exclude = Some(
        alloc::vec![
            core::num::NonZeroU32::new(70000).unwrap(),
            core::num::NonZeroU32::new(65001).unwrap()
        ]
        .into_boxed_slice(),
    );
    assert!(
        reflector
            .validate()
            .unwrap_err()
            .path
            .ends_with("/action/as_path_exclude")
    );
    let action = &mut reflector
        .bgp
        .as_mut()
        .unwrap()
        .neighbors
        .values_mut()
        .next()
        .unwrap()
        .export
        .as_mut()
        .unwrap()
        .rules[0]
        .action;
    let crate::bgp::RouteAction::Permit {
        as_path_exclude,
        as_path_replace,
        ..
    } = action
    else {
        unreachable!()
    };
    *as_path_exclude = None;
    *as_path_replace = Some(alloc::boxed::Box::new(crate::bgp::AsPathReplacement {
        from: None,
        to: core::num::NonZeroU32::new(90000).unwrap(),
    }));
    assert_eq!(
        reflector.validate().unwrap_err().path,
        "/bgp/neighbors/uplink~1~01/export/rules/0/action"
    );
    let encoded = core::str::from_utf8(&bytes).unwrap();
    assert!(
        Configuration::decode(
            encoded
                .replace("[70000,65001,70000]", "[70000,0,70000]")
                .as_bytes()
        )
        .is_err()
    );
    assert!(Configuration::decode(encoded.replace("\"to\":90000", "\"to\":0").as_bytes()).is_err());
    let mut invalid_policy = loaded.clone();
    invalid_policy
        .bgp
        .as_mut()
        .unwrap()
        .neighbors
        .values_mut()
        .next()
        .unwrap()
        .import
        .as_mut()
        .unwrap()
        .rules[0]
        .prefix
        .as_mut()
        .unwrap()
        .length = 33;
    assert_eq!(
        invalid_policy.validate().unwrap_err().path,
        "/bgp/neighbors/uplink~1~01/import/rules/0/prefix"
    );
    let mut invalid_bgp = loaded.clone();
    let bgp = invalid_bgp.bgp.as_mut().unwrap();
    bgp.neighbors.values_mut().next().unwrap().hold_seconds = 2;
    assert_eq!(
        invalid_bgp.validate().unwrap_err().path,
        "/bgp/neighbors/uplink~1~01/hold_seconds"
    );
    let bgp = invalid_bgp.bgp.as_mut().unwrap();
    bgp.neighbors.values_mut().next().unwrap().hold_seconds = 0;
    let neighbor = bgp.neighbors.values().next().unwrap().clone();
    bgp.neighbors
        .insert(Name::try_from("second".to_string()).unwrap(), neighbor);
    assert!(
        invalid_bgp
            .validate()
            .unwrap_err()
            .message
            .contains("duplicate")
    );
    let bgp = invalid_bgp.bgp.as_mut().unwrap();
    bgp.neighbors.pop_last();
    bgp.identifier = Ipv4Addr::UNSPECIFIED;
    assert_eq!(invalid_bgp.validate().unwrap_err().path, "/bgp/identifier");
    // 配置身份跨启动保持不变，来源只绑定本次解析得到的实际接口；缺席成员暂退而不改写持久意图。
    let source = loaded
        .route_source(crate::routing::Family::Ipv4, |_| Ok(Some(9)))
        .unwrap();
    let source = crate::routing::Batch::parse(&source).unwrap();
    assert!(
        source
            .next_hops()
            .next()
            .unwrap()
            .members()
            .any(|member| matches!(
                member.target,
                crate::routing::Target::Direct {
                    interface_index: 9,
                    ..
                }
            ))
    );
    let absent = loaded
        .route_source(crate::routing::Family::Ipv4, |_| Ok(None))
        .unwrap();
    assert_eq!(
        crate::routing::Batch::parse(&absent)
            .unwrap()
            .next_hops()
            .next()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(loaded, configuration);
    let mut invalid_source = loaded.clone();
    invalid_source.routes[0].preference = 0;
    assert_eq!(invalid_source.validate().unwrap_err().path, "/routes/0");
    invalid_source.routes[0].preference = 1;
    invalid_source.interfaces.clear();
    assert!(
        invalid_source
            .validate()
            .unwrap_err()
            .path
            .ends_with("/target/interface")
    );
    let profile = loaded.matching(b"eth9", mac).unwrap().unwrap();
    assert_eq!(profile.project(InterfaceConfig::dhcp()), projection);
    assert_eq!(profile.mtu, 1280);
    assert_eq!(
        profile.default_route,
        Some(crate::routing::Attributes {
            preference: 150,
            metric: 42
        })
    );
    let mut invalid = loaded.clone();
    invalid
        .interfaces
        .values_mut()
        .next()
        .unwrap()
        .default_route
        .as_mut()
        .unwrap()
        .preference = 0;
    assert!(
        invalid
            .validate()
            .unwrap_err()
            .path
            .ends_with("/default_route/preference")
    );
    invalid = loaded.clone();
    invalid.interfaces.values_mut().next().unwrap().mtu = 67;
    assert_eq!(
        invalid.validate().unwrap_err().message,
        "invalid Ethernet MTU"
    );
    let mut without_mtu: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    without_mtu["interfaces"]
        .as_object_mut()
        .unwrap()
        .values_mut()
        .next()
        .unwrap()
        .as_object_mut()
        .unwrap()
        .remove("mtu");
    let defaulted = Configuration::decode(&serde_json::to_vec(&without_mtu).unwrap()).unwrap();
    assert_eq!(
        defaulted.interfaces.values().next().unwrap().mtu,
        DEFAULT_MTU
    );
    let mut historical: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    historical.as_object_mut().unwrap().remove("routes");
    historical.as_object_mut().unwrap().remove("route_policy");
    historical.as_object_mut().unwrap().remove("bgp");
    for profile in historical["interfaces"]
        .as_object_mut()
        .unwrap()
        .values_mut()
    {
        profile.as_object_mut().unwrap().remove("default_route");
    }
    assert!(
        Configuration::decode(&serde_json::to_vec(&historical).unwrap())
            .unwrap()
            .routes
            .is_empty()
    );
    assert!(
        Configuration::decode(&serde_json::to_vec(&historical).unwrap())
            .unwrap()
            .bgp
            .is_none()
    );
    // 只改名字不能冻结默认 IP；显式启用也必须能覆盖 disabled 默认值。
    let mut rename_only = profile.clone();
    rename_only.ipv4 = None;
    rename_only.display_name = Some("办公网".into());
    let mut inherited = InterfaceConfig::dhcp();
    inherited.method = Method::Disabled;
    rename_only.enabled = Some(true);
    assert_eq!(rename_only.project(inherited).method, Method::Dhcp);
}

#[test]
fn configuration_rejects_ambiguous_or_unrecognized_input_and_exposes_field_metadata() {
    for bytes in [
        br#"{"version":2}"#.as_slice(),
        br#"{"version":1,"typo":true}"#,
        br#"{"version":1,"interfaces":{"lan":{"device":{"kind":"name","value":"eth0"}},"lan":{"device":{"kind":"name","value":"eth1"}}}}"#,
        br#"{"version":1,"bgp":{"local_as":65000,"identifier":"192.0.2.1","neighbors":{"peer":{"address":"192.0.2.2","asn":65001,"preference":20,"hold_seconds":90,"retry_seconds":5,"passive":false},"peer":{"address":"192.0.2.3","asn":65002,"preference":20,"hold_seconds":90,"retry_seconds":5,"passive":false}}}}"#,
    ] {
        assert!(Configuration::decode(bytes).is_err());
    }
    for address in ["::", "::1", "ff02::1", "::ffff:192.0.2.1"] {
        let mut config = Configuration::default();
        config.ensure_interface(b"eth0", None).unwrap().ipv6 = Some(Ipv6Configuration {
            accept_ra: RouterAdvertisementPolicy::default(),
            advertise: RouterAdvertisements::default(),
            addresses: vec![Ipv6Address {
                address: address.parse().unwrap(),
                prefix_length: 64,
                on_link: true,
            }],
        });
        assert_eq!(
            config.validate().unwrap_err().path,
            "/interfaces/eth0/ipv6/addresses/0"
        );
    }
    let mut configuration = Configuration::default();
    let profile = configuration.ensure_interface(b"eth0", None).unwrap();
    profile.ipv4 = Some(Ipv4Configuration::from_projection(&InterfaceConfig::dhcp()));
    profile.ipv4.as_mut().unwrap().prefix_length = 33;
    assert_eq!(
        configuration.validate().unwrap_err().path,
        "/interfaces/eth0/ipv4"
    );
    configuration.interfaces.clear();
    configuration.ensure_interface(b"eth0", None).unwrap();
    configuration
        .ensure_interface(b"eth1", Some([2, 0, 0, 0, 0, 1]))
        .unwrap();
    assert_eq!(
        configuration.matching(b"eth0", [2, 0, 0, 0, 0, 1]),
        Err(Invalid)
    );
    configuration.dns.search_list = vec!["office.test".into(), "OFFICE.test".into()];
    assert!(configuration.validate().is_err());
    let schema = Configuration::schema();
    let schema = schema.as_value();
    assert_eq!(
        schema["$schema"],
        "https://json-schema.org/draft/2020-12/schema"
    );
    assert_eq!(schema["properties"]["version"]["maximum"], VERSION);
    assert_eq!(
        schema["$defs"]["Ipv4Configuration"]["properties"]["prefix_length"]["maximum"],
        32
    );
    assert_eq!(schema["additionalProperties"], false);
}

// 原 IPv4 持久化用例不能保护相同 link-local 地址在不同接口的身份、范围遗失和两族独立开关。
#[test]
fn bgp_neighbor_scopes_and_families_survive_configuration_roundtrip() {
    let mut bgp = crate::bgp::Configuration::parse(
        br#"{
      "local_as":65000,"identifier":"192.0.2.1","neighbors":{"six":{
        "address":"fe80::2","interface":"eth0","asn":65001,"preference":20,
        "hold_seconds":90,"retry_seconds":5,"passive":false,"ipv4_unicast":false,"ipv6_unicast":true,
        "tcp_ao":{"send_id":9,"receive_id":19,"keys":[
          {"send_id":9,"receive_id":19,"algorithm":"hmac_sha1","secret":"fixture-ao-old"},
          {"send_id":10,"receive_id":20,"algorithm":"aes128_cmac","secret":"fixture-ao-new"}
        ]}
      }}}"#,
    )
    .unwrap();
    let name = Name::try_from("six".to_string()).unwrap();
    let mut other = bgp.neighbors[&name].clone();
    other.interface = Some(Name::try_from("eth1".to_string()).unwrap());
    bgp.neighbors.insert(
        Name::try_from("another-link".to_string()).unwrap(),
        other.clone(),
    );
    bgp.validate().unwrap();
    let mut configuration = Configuration {
        bgp: Some(bgp.clone()),
        ..Configuration::default()
    };
    let loaded = Configuration::decode(&configuration.encode().unwrap()).unwrap();
    assert_eq!(loaded, configuration);
    assert!(!format!("{loaded:?}").contains("fixture-ao-"));
    let review = Configuration::difference_json(None, &configuration).unwrap();
    let review = core::str::from_utf8(&review).unwrap();
    assert!(review.contains("[已隐藏]") && !review.contains("fixture-ao-"));
    // 整个路由域新增时，差异以一个子树输出。子树和单字段都必须遮蔽密钥。
    let domain_name = Name::try_from("edge/~".to_string()).unwrap();
    let mut domain: RoutingDomain = serde_json::from_value(serde_json::json!({"interfaces": {
        "eth0": { "device": { "kind": "mac", "value": [2, 0, 0, 0, 0, 1] } },
        "eth1": { "device": { "kind": "mac", "value": [2, 0, 0, 0, 0, 2] } }
    }}))
    .unwrap();
    domain.bgp = Some(bgp.clone());
    let mut scoped = Configuration::default();
    scoped.routing_domains.insert(domain_name.clone(), domain);
    let review = Configuration::difference_json(None, &scoped).unwrap();
    let review = core::str::from_utf8(&review).unwrap();
    assert!(review.contains("[已隐藏]") && !review.contains("fixture-ao-"));
    let mut rotated = bgp.clone();
    let ao = rotated
        .neighbors
        .get_mut(&name)
        .unwrap()
        .tcp_ao
        .as_mut()
        .unwrap();
    ao.send_id = 10;
    ao.receive_id = 20;
    ao.keys.remove(0);
    rotated.validate().unwrap();
    assert!(bgp.supports_live_update(&rotated));
    let mut changed = scoped.clone();
    changed.routing_domains.get_mut(&domain_name).unwrap().bgp = Some(rotated.clone());
    let review = Configuration::difference_json(Some(&scoped), &changed).unwrap();
    let review = core::str::from_utf8(&review).unwrap();
    assert!(review.contains("[已隐藏]") && !review.contains("fixture-ao-"));
    changed
        .routing_domains
        .get_mut(&domain_name)
        .unwrap()
        .interfaces
        .remove(&Name::try_from("eth1".to_string()).unwrap());
    assert_eq!(
        changed.validate().unwrap_err().path,
        "/routing_domains/edge~1~0/bgp/neighbors/another-link/interface"
    );
    assert!(!bgp.neighbors[&name].shutdown);
    rotated.neighbors.get_mut(&name).unwrap().shutdown = true;
    let stopped = crate::bgp::Configuration::parse(&rotated.encode().unwrap()).unwrap();
    assert!(stopped.neighbors[&name].shutdown);
    assert!(bgp.supports_live_update(&stopped) && stopped.supports_live_update(&bgp));
    let peer = rotated.neighbors.get_mut(&name).unwrap();
    peer.md5_password = serde_json::from_str(r#""fixture-md5""#).unwrap();
    assert!(rotated.validate().unwrap_err().path.ends_with("/tcp_ao"));
    let peer = rotated.neighbors.get_mut(&name).unwrap();
    peer.md5_password = None;
    peer.tcp_ao = None;
    assert!(!bgp.supports_live_update(&rotated));
    bgp.neighbors.get_mut(&name).unwrap().interface = other.interface;
    assert_eq!(
        bgp.validate().unwrap_err().message,
        "duplicate neighbor transport address"
    );
    bgp.neighbors
        .remove(&Name::try_from("another-link".to_string()).unwrap());
    bgp.neighbors.get_mut(&name).unwrap().interface = None;
    assert!(bgp.validate().unwrap_err().path.ends_with("/interface"));
    for ip in ["2001:db8::2", "192.0.2.2"] {
        let neighbor = bgp.neighbors.get_mut(&name).unwrap();
        neighbor.address = ip.parse().unwrap();
        bgp.validate().unwrap();
        bgp.neighbors.get_mut(&name).unwrap().interface =
            Some(Name::try_from("eth0".to_string()).unwrap());
        assert!(bgp.validate().unwrap_err().path.ends_with("/interface"));
        bgp.neighbors.get_mut(&name).unwrap().interface = None;
    }
    let neighbor = bgp.neighbors.get_mut(&name).unwrap();
    neighbor.ipv6_unicast = false;
    assert!(bgp.validate().unwrap_err().path.ends_with("/ipv4_unicast"));
    let neighbor = bgp.neighbors.get_mut(&name).unwrap();
    neighbor.ipv4_unicast = true;
    neighbor.address = "::ffff:192.0.2.2".parse().unwrap();
    assert!(bgp.validate().unwrap_err().path.ends_with("/address"));
    configuration
        .bgp
        .as_mut()
        .unwrap()
        .neighbors
        .get_mut(&name)
        .unwrap()
        .interface = None;
    assert!(
        configuration.encode().is_err(),
        "invalid scope cannot enter persistent configuration"
    );
}

// 配置往返和租约分配必须共用同一身份与下游编号。重叠的上联授权不能在两个 LAN 发布同一 /64。
#[test]
fn prefix_delegation_keeps_identity_and_assigns_each_subnet_once() {
    use crate::delegation::DelegatedPrefix;

    let mut configuration = Configuration::default();
    configuration.ensure_interface(b"wan", None).unwrap();
    for lan in [b"office", b"guests"] {
        configuration.ensure_interface(lan, None).unwrap().ipv6 = Some(Ipv6Configuration {
            addresses: Vec::new(),
            accept_ra: RouterAdvertisementPolicy::Disabled,
            advertise: RouterAdvertisements::Enabled {
                max_interval_seconds: 20,
                router_lifetime_seconds: 60,
                dns: Default::default(),
            },
        });
    }
    let mac = [2, 0, 0, 0, 0, 9];
    let name = |name: &str| Name::try_from(name.to_string()).unwrap();
    configuration.prefix_delegation = Some(PrefixDelegation {
        upstream: name("wan"),
        duid_mac: mac,
        iaid: 34,
        accept_reconfigure: true,
        downstream: [(name("office"), 1), (name("guests"), 17)]
            .into_iter()
            .collect(),
    });
    let restored = Configuration::decode(&configuration.encode().unwrap()).unwrap();
    assert_eq!(restored, configuration);
    let policy = restored.prefix_delegation.unwrap();
    assert_eq!(
        (policy.duid(), policy.iaid),
        ([0, 3, 0, 1, 2, 0, 0, 0, 0, 9], 34)
    );

    let broad = DelegatedPrefix {
        address: "2001:db8:1234:5600::".parse().unwrap(),
        length: 56,
        preferred_until: 60,
        valid_until: 120,
    };
    let narrow = DelegatedPrefix {
        address: "2001:db8:1234:5610::".parse().unwrap(),
        length: 60,
        preferred_until: 30,
        valid_until: 90,
    };
    let parents = [broad, narrow];
    let expected = "2001:db8:1234:5611::".parse().unwrap();
    let office = policy.subnets(&parents, 1, 0).unwrap();
    assert_eq!(office.len(), 2);
    assert_eq!(
        office[1],
        DelegatedPrefix {
            address: expected,
            length: 64,
            ..narrow
        }
    );
    assert!(policy.subnets(&parents, 17, 0).unwrap().is_empty());
    assert_eq!(
        policy.subnets(&parents, 17, 90).unwrap(),
        [DelegatedPrefix {
            address: expected,
            length: 64,
            ..broad
        }]
    );
    assert!(policy.subnets(&parents, 17, 120).unwrap().is_empty());
    assert!(policy.subnets(&parents, 256, 0).unwrap().is_empty());

    let policy = configuration.prefix_delegation.as_mut().unwrap();
    *policy.downstream.get_mut(&name("office")).unwrap() = 17;
    assert_eq!(
        configuration.validate().unwrap_err().path,
        "/prefix_delegation/downstream"
    );
}

#[test]
fn dhcp_pool_derives_its_subnet_and_rejects_the_server_in_the_range() {
    let mut configuration = Configuration::default();
    let profile = configuration.ensure_interface(b"eth1", None).unwrap();
    let server: Ipv4Addr = "192.0.2.1".parse().unwrap();
    profile.ipv4 = Some(Ipv4Configuration {
        assignment: Assignment::Static,
        address: server,
        prefix_length: 24,
        gateway: Ipv4Addr::UNSPECIFIED,
        dns: Vec::new(),
        dns_suffix: String::new(),
    });
    profile.dhcp_v4_server = Some(DhcpV4Pool {
        first: "192.0.2.10".parse().unwrap(),
        last: "192.0.2.20".parse().unwrap(),
        router: Some(server),
        dns: vec![],
        lease_seconds: 3600,
        probe_addresses: true,
    });
    let roundtrip = Configuration::decode(&configuration.encode().unwrap()).unwrap();
    let pool = roundtrip
        .interfaces
        .values()
        .next()
        .unwrap()
        .dhcp_pool()
        .unwrap()
        .unwrap();
    assert_eq!(pool.server, server);
    assert_eq!(pool.mask(), Some(0xffff_ff00));
    configuration
        .interfaces
        .values_mut()
        .next()
        .unwrap()
        .dhcp_v4_server
        .as_mut()
        .unwrap()
        .first = server;
    assert_eq!(
        configuration.validate().unwrap_err().path,
        "/interfaces/eth1/dhcp_v4_server"
    );
}

// 使用实际导入格式覆盖 VLAN 名称与设备名混淆，以及缺少可用上游时的执行正文。
#[test]
fn dns_forwarding_accepts_vlan_bindings_and_unavailable_scoped_upstreams() {
    use crate::dns_cache::{Binding, Settings};

    let mut configuration = Configuration::decode(br#"{
        "version":1,
        "interfaces":{"trunk":{"device":{"kind":"name","value":"eth0"}}},
        "bridges":{"lan":{"ports":{"trunk":{"vlans":{"10":"tagged","20":"tagged"}}},
            "gateways":{
                "10":{"name":"office","address":"192.0.2.1","prefix_length":24,
                      "ipv6":[{"address":"fd00:10::1","prefix_length":64}]},
                "20":{"name":"resolvers","ipv6":[{"address":"fd00:20::1","prefix_length":64}]}
            }
        }},
        "dns":{"search_list":[],"primary_suffix":"","append_parent_suffixes":false,"forwarding":{
            "listen":[{"interface":"office","address":"192.0.2.1"},{"interface":"office","address":"fd00:10::1"}],
            "upstream":[{"interface":"resolvers","address":"fe80::53"}]
        }}
    }"#).unwrap();
    let forwarding = configuration.dns.forwarding.as_ref().unwrap();
    let settings = Settings {
        listen: forwarding
            .listen
            .iter()
            .map(|entry| Binding {
                interface: 11,
                address: entry.address,
            })
            .collect(),
        upstream: Vec::new(),
    };
    assert_eq!(
        Settings::decode(&settings.encode().unwrap()).unwrap(),
        settings
    );
    let forwarding = configuration.dns.forwarding.as_mut().unwrap();
    forwarding.listen[1].interface = forwarding.upstream[0].interface.clone().unwrap();
    assert_eq!(
        configuration.validate().unwrap_err().path,
        "/dns/forwarding"
    );
}
