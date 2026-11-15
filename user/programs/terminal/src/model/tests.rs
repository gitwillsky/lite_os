use super::*;

fn model(columns: u16, rows: u16) -> Model {
    Model::new(Area {
        columns,
        rows,
        cell: (8, 16),
        scale: 1,
    })
}

fn line(model: &Model, row: usize) -> String {
    model
        .screen()
        .cells()
        .filter(|cell| cell.row == row && cell.width != 0)
        .flat_map(|cell| std::iter::once(cell.character).chain(cell.combining.iter().copied()))
        .collect::<String>()
        .trim_end()
        .to_owned()
}

#[test]
fn split_utf8_preserves_codex_symbols_wide_cells_and_combining_marks() {
    let mut model = model(20, 3);
    let text = "› • ⠋ 中文 e\u{301}X";
    for byte in text.bytes() {
        model.feed(&[byte]);
    }
    assert_eq!(line(&model, 0), text);
    let cells: Vec<_> = model.screen().cells().take(14).collect();
    assert_eq!(cells[6].width, 2);
    assert_eq!(cells[7].width, 0);
    assert_eq!(cells[11].combining, &['\u{301}']);
    assert_eq!(cells[12].character, 'X');
}

#[test]
fn full_screen_modes_and_queries_round_trip() {
    let mut model = model(20, 3);
    model.feed(b"shell\x1b[?1049h\x1b[?1h\x1b=\x1b[?2004h\x1b[?1004h\x1b[?1002h\x1b[?1006h");
    assert!(model.mode().contains(
        TermMode::ALT_SCREEN
            | TermMode::APP_CURSOR
            | TermMode::APP_KEYPAD
            | TermMode::BRACKETED_PASTE
            | TermMode::FOCUS_IN_OUT
            | TermMode::MOUSE_DRAG
            | TermMode::SGR_MOUSE
    ));
    let update = model.feed(b"\x1b[3;4H\x1b[6n\x1b[18t\x1b[14t\x1b]11;?\x1b\\");
    assert_eq!(
        update.reply,
        b"\x1b[3;4R\x1b[8;3;20t\x1b[4;48;160t\x1b]11;rgb:1010/1414/1818\x1b\\"
    );
    model.feed(b"tui\x1b[?1049l");
    assert_eq!(line(&model, 0), "shell");
}

#[test]
fn reflow_and_selection_preserve_soft_wrapped_text() {
    let mut model = model(6, 4);
    model.feed("ab中文abcd".as_bytes());
    model.resize(Area {
        columns: 9,
        ..model.size()
    });
    assert_eq!(line(&model, 0), "ab中文abc");
    assert_eq!(line(&model, 1), "d");
    model.select(0, 0, false);
    model.extend_selection(1, 0, true);
    assert_eq!(model.selected_text().as_deref(), Some("ab中文abcd"));
}

#[test]
fn sync_timeout_releases_buffered_output_on_an_idle_pty() {
    let mut model = model(20, 3);
    model.feed(b"before\x1b[?2026h after");
    assert!(model.sync_deadline().is_some());
    assert_eq!(line(&model, 0), "before");
    model.finish_sync();
    assert!(model.sync_deadline().is_none());
    assert_eq!(line(&model, 0), "before after");
}

#[test]
fn sgr_truecolor_palette_and_reverse_are_used_by_the_projection() {
    let mut model = model(8, 1);
    model.feed(b"\x1b[38;2;10;20;30m\x1b[48;5;196m\x1b[7mX");
    let cell = model.screen().cells().next().unwrap();
    assert_eq!(cell.foreground, 0xff0000);
    assert_eq!(cell.background, 0x0a141e);
}
