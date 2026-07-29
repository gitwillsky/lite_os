use super::*;

#[test]
fn app_records_only_its_first_presented_scene() {
    let (stream, _peer) = UnixStream::pair().unwrap();
    let mut app = App {
        stream,
        id: "music-player".to_owned(),
        configure: None,
        last_revision: 0,
        pending: None,
        current: None,
        first_scene_presented: false,
    };

    assert!(!app.first_scene_presented);
    app.first_scene_presented = true;
    assert!(app.first_scene_presented);
}

#[test]
fn app_first_presented_marker_is_one_complete_line() {
    assert_eq!(
        scene::app_first_scene_presented_marker(7),
        "compositor: app 7 first scene presented\n"
    );
}

#[test]
fn empty_surface_damage_means_retained_pixels_are_unchanged() {
    assert_eq!(
        validate_surface_damage(
            Vec::new(),
            Size {
                width: 1792,
                height: 1132,
            },
        )
        .unwrap(),
        []
    );
    assert!(
        validate_surface_damage(
            vec![Rect {
                x: 1700,
                y: 0,
                width: 100,
                height: 10,
            }],
            Size {
                width: 1792,
                height: 1132,
            },
        )
        .is_err()
    );
}
