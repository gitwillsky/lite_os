use crate::protocol::{self, ArgKind};
use crate::{generate_from_xml, xml};

const MINI: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!-- header comment -->
<protocol name="mini">
  <copyright>Text with &lt;markup&gt; and a stray > sign.</copyright>
  <interface name="mini_display" version="3">
    <description summary="root"> body <b>bold</b> </description>
    <request name="get_thing">
      <arg name="id" type="new_id" interface="mini_thing"/>
      <arg name="type" type="string" allow-null="true"/>
    </request>
    <request name="release" type="destructor" since="2"/>
    <event name="error">
      <arg name="object" type="object" interface="mini_thing" allow-null="true"/>
      <arg name="code" type="uint" enum="kind"/>
      <arg name="fd" type="fd"/>
    </event>
    <enum name="kind">
      <entry name="first" value="0" summary="the first"/>
      <entry name="90" value="0x5a"/>
    </enum>
    <enum name="caps" bitfield="true">
      <entry name="pointer" value="1"/>
      <entry name="touch" value="4"/>
    </enum>
  </interface>
  <interface name="mini_thing" version="1">
    <request name="bind">
      <arg name="name" type="uint"/>
      <arg name="id" type="new_id"/>
    </request>
  </interface>
</protocol>"#;

#[test]
fn xml_handles_declarations_comments_text_and_entities() {
    let root = xml::parse(MINI).unwrap();
    assert_eq!(root.name, "protocol");
    assert_eq!(root.children_named("interface").count(), 2);
    let attribute = xml::parse(r#"<a v="x &amp; &lt;y&gt; &quot;z&quot;"/>"#).unwrap();
    assert_eq!(attribute.attribute("v"), Some("x & <y> \"z\""));
}

#[test]
fn xml_rejects_malformed_documents() {
    for bad in [
        "<a>",
        "<a></b>",
        "<a b=c/>",
        "<a/><b/>",
        "<a v=\"&nope;\"/>",
        "text",
        "<a b=\"x/>",
    ] {
        assert!(xml::parse(bad).is_err(), "{bad}");
    }
}

#[test]
fn protocol_model_reads_versions_flags_and_numbers() {
    let interfaces = protocol::parse(MINI).unwrap();
    let display = &interfaces[0];
    assert_eq!(
        (display.name.as_str(), display.version),
        ("mini_display", 3)
    );
    assert_eq!(display.requests[1].since, 2);
    assert!(display.requests[1].destructor);
    assert_eq!(display.requests[0].args[0].kind, ArgKind::NewId);
    assert!(display.requests[0].args[1].allow_null);
    assert_eq!(display.enums[0].entries[1].value, 0x5a);
    assert!(display.enums[1].bitfield);
}

#[test]
fn protocol_rejects_inconsistent_descriptions() {
    let wrap = |body: &str| {
        format!(
            "<protocol name=\"p\"><interface name=\"i\" version=\"1\">{body}</interface></protocol>"
        )
    };
    for body in [
        r#"<request name="r"><arg name="a" type="widget"/></request>"#,
        r#"<request name="r"><arg name="a" type="new_id" interface="i"/><arg name="b" type="new_id" interface="i"/></request>"#,
        r#"<enum name="e"><entry name="a" value="1"/><entry name="b" value="1"/></enum>"#,
        r#"<enum name="e"><entry name="a" value="1"/><entry name="a" value="2"/></enum>"#,
        r#"<enum name="e"><entry name="a" value="x"/></enum>"#,
        r#"<request><arg name="a" type="int"/></request>"#,
    ] {
        assert!(protocol::parse(&wrap(body)).is_err(), "{body}");
    }
    assert!(protocol::parse("<other/>").is_err());
}

#[test]
fn generator_emits_types_codecs_and_dispatch() {
    let code = generate_from_xml(&[MINI]).unwrap();
    for expected in [
        "pub mod mini_display {",
        "pub const VERSION: u32 = 3;",
        "GetThing {",
        "r#type: Option<String>,",
        "writer.string(r#type.as_deref());",
        "pub enum Kind {",
        "_90 = 90,",
        "pub mod caps {",
        "pub const POINTER: u32 = 1;",
        "Kind::from_u32(reader.uint()?).ok_or(DecodeError::InvalidEnum)?",
        "fd: std::os::fd::OwnedFd,",
        "writer.fd(std::os::fd::AsRawFd::as_raw_fd(fd));",
        "id: DynamicNewId,",
        "Interface::MiniThing => AnyRequest::MiniThing(mini_thing::Request::decode(opcode, reader)?)",
        "pub fn decode_event(",
        "pub fn from_name(name: &str) -> Option<Interface>",
    ] {
        assert!(code.contains(expected), "missing `{expected}`");
    }
}

#[test]
fn generator_rejects_unresolved_references() {
    let unknown_interface = r#"<protocol name="p"><interface name="i" version="1">
        <request name="r"><arg name="o" type="object" interface="ghost"/></request></interface></protocol>"#;
    assert!(
        generate_from_xml(&[unknown_interface])
            .unwrap_err()
            .contains("ghost")
    );
    let unknown_enum = r#"<protocol name="p"><interface name="i" version="1">
        <request name="r"><arg name="o" type="uint" enum="ghost"/></request></interface></protocol>"#;
    assert!(
        generate_from_xml(&[unknown_enum])
            .unwrap_err()
            .contains("ghost")
    );
    let duplicate = r#"<protocol name="p"><interface name="i" version="1"/><interface name="i" version="1"/></protocol>"#;
    assert!(
        generate_from_xml(&[duplicate])
            .unwrap_err()
            .contains("twice")
    );
}

#[test]
fn cross_file_references_resolve() {
    let first = r#"<protocol name="a"><interface name="a" version="1"><enum name="e"><entry name="x" value="1"/></enum></interface></protocol>"#;
    let second = r#"<protocol name="b"><interface name="b" version="1">
        <request name="r"><arg name="o" type="object" interface="a"/><arg name="v" type="uint" enum="a.e"/></request></interface></protocol>"#;
    let code = generate_from_xml(&[first, second]).unwrap();
    assert!(code.contains("v: super::a::E,"));
}
