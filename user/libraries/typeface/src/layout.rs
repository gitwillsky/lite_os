//! 中西文换行：西文只在空格处断开，CJK 字符之间任意断开，但遵守标点禁则。

use std::ops::Range;

/// 不能出现在行首的标点（闭合与句读）。
fn forbidden_at_line_start(character: char) -> bool {
    "，。、；：！？）］｝〕〉》」』】”’…—,.;:!?)]}%".contains(character)
}

/// 不能出现在行尾的标点（开启）。
fn forbidden_at_line_end(character: char) -> bool {
    "（［｛〔〈《「『【“‘([{".contains(character)
}

fn is_cjk(character: char) -> bool {
    !character.is_ascii() && !character.is_whitespace() && !matches!(character as u32, 0xA0..=0x24F)
}

/// 两个相邻字符之间能否断行。
fn can_break_between(before: char, after: char) -> bool {
    if before == ' ' {
        return after != ' ';
    }
    (is_cjk(before) || is_cjk(after))
        && !forbidden_at_line_start(after)
        && !forbidden_at_line_end(before)
}

/// 把 `text` 按 `max_width` 像素分行，返回每一行在 `text` 中的字节范围。
///
/// # Parameters
///
/// - `advance`: 返回字符前进量（像素）的函数，通常是 `|c| typeface.glyph(c, bold).advance`。
/// - `max_width`: 行宽上限；比它还长的单词在字符处硬断（同 `DT_WORDBREAK`），单个字符比它还宽时独占一行，所以每行至少前进一个字符。
///
/// `\n` 强制换行且不属于任何行。空格允许悬挂在行宽之外：它不会触发换行，行尾的空格也不进入返回的范围。
pub fn wrap(text: &str, max_width: i32, advance: impl Fn(char) -> i32) -> Vec<Range<usize>> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut width = 0;
    let mut break_at: Option<usize> = None;
    let mut previous: Option<char> = None;
    for (index, character) in text.char_indices() {
        if character == '\n' {
            lines.push(start..index);
            start = index + 1;
            width = 0;
            break_at = None;
            previous = None;
            continue;
        }
        if previous.is_some_and(|before| can_break_between(before, character)) {
            break_at = Some(index);
        }
        let step = advance(character);
        if width + step > max_width && index > start && character != ' ' {
            let end = break_at
                .filter(|position| *position > start)
                .unwrap_or(index);
            lines.push(start..start + text[start..end].trim_end_matches(' ').len());
            start = end;
            width = text[start..index].chars().map(&advance).sum();
            break_at = None;
        }
        width += step;
        previous = Some(character);
    }
    lines.push(start..text.len());
    lines
}
