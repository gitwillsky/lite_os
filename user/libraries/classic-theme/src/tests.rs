use std::fs;

use crate::parse::ParseErrorKind;
use crate::{DEFAULT_THEME, Edge, LoadError, Rgb, SystemColor, Theme};

const fn rgb(red: u8, green: u8, blue: u8) -> Rgb {
    Rgb { red, green, blue }
}

#[test]
fn default_file_defines_every_key() {
    for color in SystemColor::ALL {
        assert!(
            DEFAULT_THEME.contains(&format!("\n{}=", color.name())),
            "{}",
            color.name()
        );
    }
    let theme = Theme::classic();
    for (key, _) in theme
        .window
        .entries()
        .into_iter()
        .chain(theme.shell.entries())
    {
        assert!(DEFAULT_THEME.contains(&format!("\n{key}=")), "{key}");
    }
}

#[test]
fn classic_matches_the_specification() {
    let theme = Theme::classic();
    assert_eq!(theme.color(SystemColor::ActiveTitle), rgb(0x0A, 0x24, 0x6A));
    assert_eq!(
        theme.color(SystemColor::GradientActiveTitle),
        rgb(0xA6, 0xCA, 0xF0)
    );
    assert_eq!(theme.color(SystemColor::Background), rgb(0x3A, 0x6E, 0xA5));
    assert_eq!(theme.color(SystemColor::InfoWindow), rgb(0xFF, 0xFF, 0xE1));
    assert_eq!(theme.window.size_frame(), 4);
    assert_eq!(theme.window.caption_total_height(), 19);
    assert_eq!(theme.window.menu_total_height(), 19);
    assert_eq!(theme.window.scroll_width, 16);
    assert_eq!(theme.shell.taskbar_height, 28);
}

#[test]
fn raised_edge_uses_the_documented_layers() {
    let theme = Theme::classic();
    let edge = theme.edge(Edge::Raised);
    assert_eq!(
        edge.outer,
        Some((
            theme.color(SystemColor::ButtonLight),
            theme.color(SystemColor::ButtonDkShadow)
        ))
    );
    assert_eq!(
        edge.inner,
        Some((
            theme.color(SystemColor::ButtonHilight),
            theme.color(SystemColor::ButtonShadow)
        ))
    );
    assert_eq!(theme.edge(Edge::RaisedInner).outer, None);
    assert_eq!(theme.edge(Edge::SunkenOuter).inner, None);
    // 凹陷是凸起的镜像：内外层颜色互换方向。
    let sunken = theme.edge(Edge::Sunken);
    assert_eq!(
        sunken.outer,
        Some((
            theme.color(SystemColor::ButtonShadow),
            theme.color(SystemColor::ButtonHilight)
        ))
    );
}

#[test]
fn overlay_changes_only_listed_keys() {
    let base = Theme::classic();
    let theme = base
        .overlay("[Colors]\nButtonFace=1 2 3\n; comment\n\n[Shell]\nTaskbarHeight=40\n")
        .unwrap();
    assert_eq!(theme.color(SystemColor::ButtonFace), rgb(1, 2, 3));
    assert_eq!(theme.shell.taskbar_height, 40);
    assert_eq!(
        theme.color(SystemColor::Window),
        base.color(SystemColor::Window)
    );
    assert_eq!(theme.window, base.window);
}

#[test]
fn overlay_reports_the_failing_line() {
    let base = Theme::classic();
    let cases = [
        (
            "[Colors]\nButtonFace=1 2\n",
            2,
            ParseErrorKind::InvalidColor("1 2".into()),
        ),
        (
            "[Colors]\nButtonFace=1 2 3 4\n",
            2,
            ParseErrorKind::InvalidColor("1 2 3 4".into()),
        ),
        (
            "[Colors]\nButtonFace=1 2 256\n",
            2,
            ParseErrorKind::InvalidColor("1 2 256".into()),
        ),
        (
            "[Colors]\nNoSuchColor=1 2 3\n",
            2,
            ParseErrorKind::UnknownKey("NoSuchColor".into()),
        ),
        (
            "\n[Nope]\n",
            2,
            ParseErrorKind::UnknownSection("Nope".into()),
        ),
        ("Window=0 0 0\n", 1, ParseErrorKind::KeyOutsideSection),
        ("[Shell]\nTaskbarHeight\n", 2, ParseErrorKind::MissingEquals),
        (
            "[Shell]\nTaskbarHeight=-3\n",
            2,
            ParseErrorKind::InvalidMetric("-3".into()),
        ),
        (
            "[WindowMetrics]\nScrollWidth=abc\n",
            2,
            ParseErrorKind::InvalidMetric("abc".into()),
        ),
        (
            "[Shell]\nScrollWidth=3\n",
            2,
            ParseErrorKind::UnknownKey("ScrollWidth".into()),
        ),
    ];
    for (text, line, kind) in cases {
        let error = base.overlay(text).unwrap_err();
        assert_eq!((error.line, error.kind), (line, kind), "{text:?}");
    }
}

#[test]
fn load_uses_the_first_existing_file_over_the_default() {
    let directory = std::env::temp_dir().join(format!("classic-theme-load-{}", std::process::id()));
    fs::create_dir_all(&directory).unwrap();
    let missing = directory.join("missing.theme");
    let user = directory.join("user.theme");
    let local = directory.join("local.theme");
    fs::write(&user, "[Colors]\nWindow=9 9 9\n").unwrap();
    fs::write(&local, "[Colors]\nWindow=7 7 7\n").unwrap();

    let theme = Theme::load(&[&missing, &user, &local]).unwrap();
    assert_eq!(theme.color(SystemColor::Window), rgb(9, 9, 9));
    assert_eq!(
        theme.color(SystemColor::Menu),
        Theme::classic().color(SystemColor::Menu)
    );
    assert_eq!(Theme::load(&[&missing]).unwrap(), Theme::classic());

    fs::write(&user, "[Colors]\nWindow=oops\n").unwrap();
    // 损坏的用户主题不能悄悄退回到下一个候选。
    assert!(matches!(
        Theme::load(&[&user, &local]),
        Err(LoadError::Parse { .. })
    ));
    fs::remove_dir_all(&directory).unwrap();
}
