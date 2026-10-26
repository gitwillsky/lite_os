use wayland_wire::protocol::{
    zwlr_layer_shell_v1 as shell, zwlr_layer_surface_v1 as layer_surface,
};

use super::*;
use crate::wayland::{KeyboardInteractivity, Layer, layer};

const LAYER: u32 = 53;
const OBJECTS: [(u32, Interface); 2] = [
    (LAYER, Interface::ZwlrLayerSurfaceV1),
    (5, Interface::WlSurface),
];
const TOP: u32 = layer::ANCHOR_TOP;
const BOTTOM: u32 = layer::ANCHOR_BOTTOM;
const LEFT: u32 = layer::ANCHOR_LEFT;
const RIGHT: u32 = layer::ANCHOR_RIGHT;

fn request(harness: &mut Harness, request: layer_surface::Request) -> Result<(), Disconnected> {
    harness.send(|w| request.encode(id(LAYER), w).unwrap())
}

fn commit(harness: &mut Harness) -> Result<(), Disconnected> {
    harness.send(|w| wl_surface::Request::Commit.encode(id(5), w).unwrap())
}

/// 建立任务栏式 layer surface：surface 5、layer surface 53。
fn taskbar(harness: &mut Harness, layer: shell::Layer) {
    harness.create_surface(5);
    harness
        .send(|w| {
            shell::Request::GetLayerSurface {
                id: id(LAYER),
                surface: id(5),
                output: None,
                layer,
                namespace: "taskbar".into(),
            }
            .encode(id(51), w)
            .unwrap()
        })
        .unwrap();
}

/// 取走输出里 layer surface 的 configure，返回 `(serial, 宽, 高)`。
fn configure(harness: &mut Harness) -> (u32, u32, u32) {
    let events = harness.events(&OBJECTS);
    events
        .into_iter()
        .find_map(|event| match event.event {
            AnyEvent::ZwlrLayerSurfaceV1(layer_surface::Event::Configure {
                serial,
                width,
                height,
            }) => Some((serial, width, height)),
            _ => None,
        })
        .expect("a layer configure")
}

#[test]
fn the_initial_commit_configures_with_the_size_from_the_anchors() {
    let mut harness = Harness::privileged();
    taskbar(&mut harness, shell::Layer::Top);
    request(
        &mut harness,
        layer_surface::Request::SetAnchor {
            anchor: BOTTOM | LEFT | RIGHT,
        },
    )
    .unwrap();
    request(
        &mut harness,
        layer_surface::Request::SetSize {
            width: 0,
            height: 28,
        },
    )
    .unwrap();
    request(
        &mut harness,
        layer_surface::Request::SetExclusiveZone { zone: 28 },
    )
    .unwrap();
    commit(&mut harness).unwrap();
    // 输出 1280×800、scale 2 → 逻辑 640×400：宽填满左右锚点之间。
    let (_, width, height) = configure(&mut harness);
    assert_eq!((width, height), (640, 28));
}

#[test]
fn margins_reduce_the_filled_dimension() {
    let mut harness = Harness::privileged();
    taskbar(&mut harness, shell::Layer::Top);
    request(
        &mut harness,
        layer_surface::Request::SetAnchor {
            anchor: TOP | LEFT | RIGHT,
        },
    )
    .unwrap();
    request(
        &mut harness,
        layer_surface::Request::SetSize {
            width: 0,
            height: 20,
        },
    )
    .unwrap();
    request(
        &mut harness,
        layer_surface::Request::SetMargin {
            top: 5,
            right: 10,
            bottom: 0,
            left: 30,
        },
    )
    .unwrap();
    commit(&mut harness).unwrap();
    assert_eq!(configure(&mut harness).1, 640 - 40);
}

#[test]
fn a_zero_size_without_opposite_anchors_is_invalid_size() {
    let mut harness = Harness::privileged();
    taskbar(&mut harness, shell::Layer::Top);
    request(
        &mut harness,
        layer_surface::Request::SetAnchor { anchor: LEFT },
    )
    .unwrap();
    assert!(commit(&mut harness).is_err());
    harness.expect_error(layer_surface::Error::InvalidSize as u32);
}

#[test]
fn a_buffer_before_the_first_ack_is_invalid_surface_state() {
    let mut harness = Harness::privileged();
    taskbar(&mut harness, shell::Layer::Top);
    request(
        &mut harness,
        layer_surface::Request::SetSize {
            width: 10,
            height: 10,
        },
    )
    .unwrap();
    commit(&mut harness).unwrap();
    configure(&mut harness);
    harness
        .create_buffer(&[0; 16], 2, 2, 8, wl_shm::Format::Argb8888)
        .unwrap();
    harness
        .send(|w| {
            wl_surface::Request::Attach {
                buffer: Some(id(11)),
                x: 0,
                y: 0,
            }
            .encode(id(5), w)
            .unwrap()
        })
        .unwrap();
    assert!(commit(&mut harness).is_err());
    harness.expect_error(layer_surface::Error::InvalidSurfaceState as u32);
}

#[test]
fn an_acknowledged_layer_surface_maps_and_is_not_a_window() {
    let mut harness = Harness::privileged();
    taskbar(&mut harness, shell::Layer::Overlay);
    request(
        &mut harness,
        layer_surface::Request::SetSize {
            width: 2,
            height: 2,
        },
    )
    .unwrap();
    request(
        &mut harness,
        layer_surface::Request::SetKeyboardInteractivity {
            keyboard_interactivity: layer_surface::KeyboardInteractivity::Exclusive,
        },
    )
    .unwrap();
    commit(&mut harness).unwrap();
    let (serial, ..) = configure(&mut harness);
    request(
        &mut harness,
        layer_surface::Request::AckConfigure { serial },
    )
    .unwrap();
    harness
        .create_buffer(&[0x7F; 16], 2, 2, 8, wl_shm::Format::Argb8888)
        .unwrap();
    harness
        .send(|w| {
            wl_surface::Request::Attach {
                buffer: Some(id(11)),
                x: 0,
                y: 0,
            }
            .encode(id(5), w)
            .unwrap()
        })
        .unwrap();
    commit(&mut harness).unwrap();
    let layers: Vec<_> = harness.client.layers().collect();
    assert_eq!(layers.len(), 1);
    assert_eq!(
        (
            layers[0].namespace,
            layers[0].state.layer,
            layers[0].state.keyboard
        ),
        ("taskbar", Layer::Overlay, KeyboardInteractivity::Exclusive)
    );
    assert_eq!(layers[0].content.width, 2);
    assert_eq!(
        harness.client.windows().count(),
        0,
        "a layer surface is not a toplevel window"
    );
}

#[test]
fn state_changes_apply_at_commit_and_a_new_size_is_reconfigured() {
    let mut harness = Harness::privileged();
    taskbar(&mut harness, shell::Layer::Top);
    request(
        &mut harness,
        layer_surface::Request::SetAnchor {
            anchor: TOP | BOTTOM | LEFT,
        },
    )
    .unwrap();
    request(
        &mut harness,
        layer_surface::Request::SetSize {
            width: 100,
            height: 0,
        },
    )
    .unwrap();
    commit(&mut harness).unwrap();
    assert_eq!(configure(&mut harness).1, 100);
    // commit 之前，pending 的新宽度不改变任何东西。
    request(
        &mut harness,
        layer_surface::Request::SetSize {
            width: 150,
            height: 0,
        },
    )
    .unwrap();
    assert!(harness.events(&OBJECTS).is_empty());
    commit(&mut harness).unwrap();
    assert_eq!(configure(&mut harness).1, 150);
    // 状态没变的 commit 不重复 configure。
    commit(&mut harness).unwrap();
    assert!(harness.events(&OBJECTS).is_empty());
}

#[test]
fn roles_cannot_be_combined() {
    let mut harness = Harness::privileged();
    taskbar(&mut harness, shell::Layer::Top);
    assert!(
        harness
            .send(|w| {
                shell::Request::GetLayerSurface {
                    id: id(60),
                    surface: id(5),
                    output: None,
                    layer: shell::Layer::Top,
                    namespace: "again".into(),
                }
                .encode(id(51), w)
                .unwrap()
            })
            .is_err()
    );
    harness.expect_error(shell::Error::Role as u32);

    let mut harness = Harness::privileged();
    harness.create_surface(5);
    harness
        .send(|w| {
            xdg_wm_base::Request::GetXdgSurface {
                id: id(21),
                surface: id(5),
            }
            .encode(id(20), w)
            .unwrap()
        })
        .unwrap();
    assert!(
        harness
            .send(|w| {
                shell::Request::GetLayerSurface {
                    id: id(60),
                    surface: id(5),
                    output: None,
                    layer: shell::Layer::Top,
                    namespace: "x".into(),
                }
                .encode(id(51), w)
                .unwrap()
            })
            .is_err()
    );
    harness.expect_error(shell::Error::Role as u32);
}

#[test]
fn invalid_anchor_and_exclusive_edge_are_rejected() {
    let mut harness = Harness::privileged();
    taskbar(&mut harness, shell::Layer::Top);
    assert!(
        request(
            &mut harness,
            layer_surface::Request::SetAnchor { anchor: 16 }
        )
        .is_err()
    );
    harness.expect_error(layer_surface::Error::InvalidAnchor as u32);

    let mut harness = Harness::privileged();
    taskbar(&mut harness, shell::Layer::Top);
    assert!(
        request(
            &mut harness,
            layer_surface::Request::SetExclusiveEdge { edge: TOP | LEFT }
        )
        .is_err()
    );
    harness.expect_error(layer_surface::Error::InvalidExclusiveEdge as u32);
}

#[test]
fn destroying_the_layer_surface_unmaps_and_frees_the_wl_surface_for_reuse() {
    let mut harness = Harness::privileged();
    taskbar(&mut harness, shell::Layer::Top);
    request(
        &mut harness,
        layer_surface::Request::SetSize {
            width: 2,
            height: 2,
        },
    )
    .unwrap();
    commit(&mut harness).unwrap();
    let (serial, ..) = configure(&mut harness);
    request(
        &mut harness,
        layer_surface::Request::AckConfigure { serial },
    )
    .unwrap();
    request(&mut harness, layer_surface::Request::Destroy).unwrap();
    assert_eq!(harness.client.layers().count(), 0);
    harness
        .send(|w| {
            shell::Request::GetLayerSurface {
                id: id(LAYER),
                surface: id(5),
                output: None,
                layer: shell::Layer::Bottom,
                namespace: "again".into(),
            }
            .encode(id(51), w)
            .unwrap()
        })
        .unwrap();
}

#[test]
fn popups_on_layer_surfaces_are_refused_clearly() {
    let mut harness = Harness::privileged();
    taskbar(&mut harness, shell::Layer::Top);
    assert!(
        request(
            &mut harness,
            layer_surface::Request::GetPopup { popup: id(70) }
        )
        .is_err()
    );
    let message = harness.expect_error(wl_display::Error::Implementation as u32);
    assert!(message.contains("popup"), "{message}");
}

fn state(anchor: u32, width: u32, height: u32) -> layer::LayerState {
    layer::LayerState {
        layer: Layer::Top,
        anchor,
        width,
        height,
        margin: [0; 4],
        exclusive_zone: 0,
        exclusive_edge: 0,
        keyboard: KeyboardInteractivity::None,
    }
}

#[test]
fn exclusive_side_follows_the_anchor_rules() {
    assert_eq!(layer::exclusive_side(&state(BOTTOM, 0, 0)), Some(BOTTOM));
    assert_eq!(
        layer::exclusive_side(&state(BOTTOM | LEFT | RIGHT, 0, 28)),
        Some(BOTTOM)
    );
    assert_eq!(
        layer::exclusive_side(&state(TOP | LEFT | RIGHT, 0, 28)),
        Some(TOP)
    );
    assert_eq!(
        layer::exclusive_side(&state(TOP | BOTTOM | LEFT, 40, 0)),
        Some(LEFT)
    );
    assert_eq!(
        layer::exclusive_side(&state(LEFT | RIGHT, 0, 20)),
        None,
        "two opposite edges are ambiguous"
    );
    assert_eq!(layer::exclusive_side(&state(0, 10, 10)), None);
    let mut explicit = state(TOP | BOTTOM | LEFT | RIGHT, 0, 0);
    explicit.exclusive_edge = RIGHT;
    assert_eq!(layer::exclusive_side(&explicit), Some(RIGHT));
}

#[test]
fn configure_size_prefers_the_request_and_never_goes_negative() {
    assert_eq!(
        layer::configure_size(&state(LEFT | RIGHT, 0, 10), (640, 400)),
        (640, 10)
    );
    assert_eq!(
        layer::configure_size(&state(0, 50, 60), (640, 400)),
        (50, 60)
    );
    let mut tiny = state(LEFT | RIGHT | TOP | BOTTOM, 0, 0);
    tiny.margin = [300, 400, 300, 400];
    assert_eq!(layer::configure_size(&tiny, (640, 400)), (0, 0));
}

#[test]
fn explicit_exclusive_edge_must_be_anchored_at_commit() {
    let mut harness = Harness::privileged();
    taskbar(&mut harness, shell::Layer::Top);
    request(
        &mut harness,
        layer_surface::Request::SetSize {
            width: 2,
            height: 2,
        },
    )
    .unwrap();
    request(
        &mut harness,
        layer_surface::Request::SetAnchor { anchor: TOP },
    )
    .unwrap();
    request(
        &mut harness,
        layer_surface::Request::SetExclusiveEdge { edge: BOTTOM },
    )
    .unwrap();
    assert!(commit(&mut harness).is_err());
    harness.expect_error(layer_surface::Error::InvalidExclusiveEdge as u32);
}

#[test]
fn unmapping_requires_a_fresh_configure_handshake() {
    let mut harness = Harness::privileged();
    taskbar(&mut harness, shell::Layer::Top);
    request(
        &mut harness,
        layer_surface::Request::SetSize {
            width: 2,
            height: 2,
        },
    )
    .unwrap();
    commit(&mut harness).unwrap();
    let (first, ..) = configure(&mut harness);
    request(
        &mut harness,
        layer_surface::Request::AckConfigure { serial: first },
    )
    .unwrap();
    harness
        .send(|w| {
            wl_surface::Request::Attach {
                buffer: None,
                x: 0,
                y: 0,
            }
            .encode(id(5), w)
            .unwrap()
        })
        .unwrap();
    commit(&mut harness).unwrap();
    assert!(
        harness.events(&OBJECTS).is_empty(),
        "unmap itself does not configure"
    );
    request(
        &mut harness,
        layer_surface::Request::SetSize {
            width: 2,
            height: 2,
        },
    )
    .unwrap();
    commit(&mut harness).unwrap();
    let (second, ..) = configure(&mut harness);
    assert_ne!(first, second);
    harness
        .create_buffer(&[0; 16], 2, 2, 8, wl_shm::Format::Argb8888)
        .unwrap();
    harness
        .send(|w| {
            wl_surface::Request::Attach {
                buffer: Some(id(11)),
                x: 0,
                y: 0,
            }
            .encode(id(5), w)
            .unwrap()
        })
        .unwrap();
    assert!(
        commit(&mut harness).is_err(),
        "the old ack cannot authorize the new mapping"
    );
    harness.expect_error(layer_surface::Error::InvalidSurfaceState as u32);
}

#[test]
fn layer_and_cursor_roles_cannot_become_xdg_windows() {
    use wayland_wire::protocol::{wl_pointer, wl_seat};
    let mut harness = Harness::privileged();
    taskbar(&mut harness, shell::Layer::Top);
    assert!(
        harness
            .send(|w| xdg_wm_base::Request::GetXdgSurface {
                id: id(60),
                surface: id(5)
            }
            .encode(id(20), w)
            .unwrap())
            .is_err()
    );
    harness.expect_error(xdg_wm_base::Error::Role as u32);
    let mut harness = Harness::privileged();
    harness.create_surface(5);
    harness
        .send(|w| {
            wl_registry::Request::Bind {
                name: 5,
                id: DynamicNewId {
                    interface: "wl_seat".into(),
                    version: 7,
                    id: id(40),
                },
            }
            .encode(id(2), w)
            .unwrap()
        })
        .unwrap();
    harness.events(&[(40, Interface::WlSeat)]);
    harness
        .send(|w| {
            wl_seat::Request::GetPointer { id: id(61) }
                .encode(id(40), w)
                .unwrap()
        })
        .unwrap();
    // set_cursor 只有携带当前 enter 序号才生效；先给连接真实的指针焦点，再验证角色排斥。
    harness.create_surface(6);
    harness.client.pointer_enter(
        1,
        id(6),
        wayland_wire::wire::Fixed::from_int(0),
        wayland_wire::wire::Fixed::from_int(0),
    );
    harness.events(&[(61, Interface::WlPointer)]);
    harness
        .send(|w| {
            wl_pointer::Request::SetCursor {
                serial: 1,
                surface: Some(id(5)),
                hotspot_x: 0,
                hotspot_y: 0,
            }
            .encode(id(61), w)
            .unwrap()
        })
        .unwrap();
    assert!(
        harness
            .send(|w| xdg_wm_base::Request::GetXdgSurface {
                id: id(60),
                surface: id(5)
            }
            .encode(id(20), w)
            .unwrap())
            .is_err()
    );
    harness.expect_error(xdg_wm_base::Error::Role as u32);
}
