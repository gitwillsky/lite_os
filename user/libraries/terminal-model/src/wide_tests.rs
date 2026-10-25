use super::*;

fn feed(model: &mut Model, bytes: impl AsRef<[u8]>) {
    model.feed(bytes.as_ref(), |_| {});
}

fn row_text(model: &Model, row: usize) -> String {
    let cells = unsafe {
        core::slice::from_raw_parts(model.primary.cells.add(row * model.columns), model.columns)
    };
    let end = cells
        .iter()
        .rposition(|cell| {
            cell.reserved & OCCUPIED != 0
                || cell.codepoint != b' ' as u32 && cell.reserved & WIDE_CONTINUATION == 0
        })
        .map_or(0, |index| index + 1);
    cells[..end]
        .iter()
        .filter(|cell| cell.reserved & WIDE_CONTINUATION == 0)
        .map(|cell| char::from_u32(cell.codepoint).expect("valid cell"))
        .collect()
}

fn assert_wide_pairs(model: &Model) {
    for row in 0..model.rows {
        let cells = unsafe {
            core::slice::from_raw_parts(model.primary.cells.add(row * model.columns), model.columns)
        };
        for (column, cell) in cells.iter().enumerate() {
            if cell.reserved & WIDE_CONTINUATION != 0 {
                assert!(column != 0);
                assert_eq!(display_width(cells[column - 1].codepoint), 2);
            } else if display_width(cell.codepoint) == 2 {
                assert!(column + 1 < model.columns);
                assert_ne!(
                    cells[column + 1].reserved & WIDE_CONTINUATION,
                    0,
                    "wide lead at row {row}, column {column} lost its continuation"
                );
            }
        }
    }
}

#[test]
fn wide_utf8_input_occupies_two_cells_when_split_across_reads() {
    let mut model = Model::new(16, 3).expect("model");
    for byte in "ab中文c".as_bytes() {
        feed(&mut model, [*byte]);
    }

    assert_eq!(model.primary.column, 7);
    assert!(!(0..model.rows).any(|row| row_text(&model, row).contains('\u{fffd}')));
}

#[test]
fn wide_character_wraps_as_one_two_cell_glyph() {
    let mut model = Model::new(4, 3).expect("model");
    feed(&mut model, "abc中x");

    assert_eq!(row_text(&model, 0), "abc");
    assert_eq!(row_text(&model, 1), "中x");
    assert_eq!(model.primary.column, 3);
    assert_wide_pairs(&model);
}

#[test]
fn pasted_chinese_plan_survives_incremental_decode_and_soft_wrap() {
    let source = concat!(
        "标准，循环直至达成。 任务转译示例： - \"加校验\" → \"为非法输入写用例 → 让其通过\" ",
        "- \"修 bug\" → \"写复现用例 → 让其通过\" - \"重构 X\" → \"改动前后行为/契约一致\" ",
        "多步任务必须先给计划： ``` 1. [步骤] → verify: [检查点] 2. [步骤] → verify: ",
        "[检查点] 3. [步骤] → verify: [检查点] ``` 强成功标准让你独立闭环；",
        "弱标准（\"让它能跑\"）会反复返工",
    );
    let mut model = Model::new(40, 20).expect("model");
    for byte in source.as_bytes() {
        feed(&mut model, [*byte]);
    }

    let visible: String = (0..model.rows).map(|row| row_text(&model, row)).collect();
    assert_eq!(visible, source);
    assert_wide_pairs(&model);
}

#[test]
fn cell_mutations_cannot_leave_orphaned_wide_halves() {
    let mut delete = Model::new(8, 2).expect("model");
    feed(&mut delete, "中x\r\x1b[P");
    assert_wide_pairs(&delete);

    let mut insert = Model::new(8, 2).expect("model");
    feed(&mut insert, "中x\x1b[2G\x1b[4hA");
    assert_wide_pairs(&insert);
}
