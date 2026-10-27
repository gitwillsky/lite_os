//! Commit transactions shared by SHM and dma-buf must preserve the same surface-tree semantics.
use super::*;
use wayland_wire::protocol::{
    wl_region, wl_subcompositor, wl_subsurface, wp_viewport, wp_viewporter,
};

impl Harness {
    fn bind_global(&mut self, name: u32, interface: &str, object: u32) {
        self.send(|w| {
            wl_registry::Request::Bind {
                name,
                id: DynamicNewId {
                    interface: interface.into(),
                    version: 1,
                    id: id(object),
                },
            }
            .encode(id(2), w)
            .unwrap();
        })
        .unwrap();
    }
    fn commit_surface(&mut self, surface: u32) {
        self.send(|w| wl_surface::Request::Commit.encode(id(surface), w).unwrap())
            .unwrap();
    }
    fn attach_buffer(&mut self, surface: u32, buffer: Option<u32>) {
        self.send(|w| {
            wl_surface::Request::Attach {
                buffer: buffer.map(id),
                x: 0,
                y: 0,
            }
            .encode(id(surface), w)
            .unwrap()
        })
        .unwrap();
    }
    fn child(&mut self, surface: u32, parent: u32, role: u32) {
        self.create_surface(surface);
        self.send(|w| {
            wl_subcompositor::Request::GetSubsurface {
                id: id(role),
                surface: id(surface),
                parent: id(parent),
            }
            .encode(id(60), w)
            .unwrap()
        })
        .unwrap();
    }
    fn tree(&self) -> Vec<(u32, i32, i32)> {
        self.client
            .subtree(id(5))
            .iter()
            .map(|view| (view.surface.get(), view.x, view.y))
            .collect()
    }
}

fn tree_fixture() -> Harness {
    let mut h = Harness::bound();
    h.bind_global(12, "wl_subcompositor", 60);
    h.create_surface(5);
    h.create_buffer(&[0xff; 16], 2, 2, 8, wl_shm::Format::Argb8888)
        .unwrap();
    h.attach_buffer(5, Some(11));
    h.commit_surface(5);
    h.events(&[(11, Interface::WlBuffer)]);
    h
}

#[test]
fn synchronized_content_position_and_stacking_publish_at_the_parent_commit() {
    let mut h = tree_fixture();
    h.child(6, 5, 70);
    h.attach_buffer(6, Some(11));
    h.send(|w| {
        wl_subsurface::Request::SetPosition { x: 7, y: 9 }
            .encode(id(70), w)
            .unwrap();
        wl_subsurface::Request::PlaceBelow { sibling: id(5) }
            .encode(id(70), w)
            .unwrap();
    })
    .unwrap();
    h.commit_surface(6);
    assert_eq!(h.tree(), [(5, 0, 0)]);
    h.commit_surface(5);
    assert_eq!(h.tree(), [(6, 7, 9), (5, 0, 0)]);
    h.attach_buffer(6, None);
    h.commit_surface(6);
    assert_eq!(h.tree(), [(6, 7, 9), (5, 0, 0)]);
    h.send(|w| wl_subsurface::Request::SetDesync.encode(id(70), w).unwrap())
        .unwrap();
    assert_eq!(
        h.tree(),
        [(5, 0, 0)],
        "desync publishes the cached unmap immediately"
    );
}

#[test]
fn a_desynchronized_descendant_still_inherits_its_synchronized_ancestor() {
    let mut h = tree_fixture();
    h.child(6, 5, 70);
    h.attach_buffer(6, Some(11));
    h.commit_surface(6);
    h.events(&[(11, Interface::WlBuffer)]);
    h.child(7, 6, 71);
    h.send(|w| wl_subsurface::Request::SetDesync.encode(id(71), w).unwrap())
        .unwrap();
    h.attach_buffer(7, Some(11));
    h.commit_surface(7);
    h.commit_surface(6);
    assert_eq!(h.tree(), [(5, 0, 0)]);
    h.commit_surface(5);
    assert_eq!(h.tree(), [(5, 0, 0), (6, 0, 0), (7, 0, 0)]);
    h.attach_buffer(7, None);
    h.commit_surface(7);
    assert_eq!(h.tree().len(), 3);
    h.send(|w| wl_subsurface::Request::SetDesync.encode(id(70), w).unwrap())
        .unwrap();
    assert_eq!(h.tree(), [(5, 0, 0), (6, 0, 0)]);
}

#[test]
fn a_commit_during_an_in_flight_frame_does_not_complete_its_callback_early() {
    let mut h = tree_fixture();
    h.send(|w| {
        wl_surface::Request::Frame { callback: id(80) }
            .encode(id(5), w)
            .unwrap()
    })
    .unwrap();
    h.commit_surface(5);
    h.client.stage_surface_frame(id(5));
    h.send(|w| {
        wl_surface::Request::Frame { callback: id(81) }
            .encode(id(5), w)
            .unwrap()
    })
    .unwrap();
    h.commit_surface(5);
    h.client.finish_frame(100);
    let events = h.events(&[(80, Interface::WlCallback), (81, Interface::WlCallback)]);
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e.event, AnyEvent::WlCallback(_)))
            .map(|e| e.object)
            .collect::<Vec<_>>(),
        [80]
    );
    h.client.stage_surface_frame(id(5));
    h.client.finish_frame(200);
    let events = h.events(&[(81, Interface::WlCallback)]);
    assert!(matches!(
        events[0].event,
        AnyEvent::WlCallback(wl_callback::Event::Done { callback_data: 200 })
    ));
    assert_eq!(events[0].object, 81);
}

#[test]
fn input_region_uses_a_commit_time_snapshot_with_subtracted_holes() {
    let mut h = tree_fixture();
    h.send(|w| {
        wl_compositor::Request::CreateRegion { id: id(90) }
            .encode(id(3), w)
            .unwrap();
        wl_region::Request::Add {
            x: 0,
            y: 0,
            width: 20,
            height: 20,
        }
        .encode(id(90), w)
        .unwrap();
        wl_region::Request::Subtract {
            x: 5,
            y: 5,
            width: 10,
            height: 10,
        }
        .encode(id(90), w)
        .unwrap();
        wl_surface::Request::SetInputRegion {
            region: Some(id(90)),
        }
        .encode(id(5), w)
        .unwrap();
        wl_region::Request::Add {
            x: 5,
            y: 5,
            width: 10,
            height: 10,
        }
        .encode(id(90), w)
        .unwrap();
        wl_region::Request::Destroy.encode(id(90), w).unwrap();
    })
    .unwrap();
    assert!(h.client.accepts_input(id(5), 10., 10.));
    h.commit_surface(5);
    assert!(h.client.accepts_input(id(5), 2., 2.));
    assert!(!h.client.accepts_input(id(5), 10., 10.));
    assert!(!h.client.accepts_input(id(5), 20., 2.));
}

#[test]
fn removing_a_viewport_and_unmapping_cannot_resurrect_old_content() {
    let mut h = tree_fixture();
    h.bind_global(11, "wp_viewporter", 65);
    h.send(|w| {
        wp_viewporter::Request::GetViewport {
            id: id(70),
            surface: id(5),
        }
        .encode(id(65), w)
        .unwrap();
        wp_viewport::Request::SetDestination {
            width: 4,
            height: 6,
        }
        .encode(id(70), w)
        .unwrap();
    })
    .unwrap();
    h.commit_surface(5);
    assert_eq!(h.client.contents().next().unwrap().1.size, (4, 6));
    h.send(|w| wp_viewport::Request::Destroy.encode(id(70), w).unwrap())
        .unwrap();
    h.attach_buffer(5, None);
    h.commit_surface(5);
    assert_eq!(h.client.contents().count(), 0);
    h.commit_surface(5);
    assert_eq!(h.client.contents().count(), 0);
    h.attach_buffer(5, Some(11));
    h.commit_surface(5);
    assert_eq!(h.client.contents().next().unwrap().1.size, (2, 2));
}
