//! 单行编辑与 Tab 焦点顺序；只消费语义按键，不拥有 socket 或绘制状态。

use crate::Key;
use std::ops::Range;

/// 单行文本、UTF-8 字节边界上的光标与选择锚点。文本内容只由此对象修改。
#[derive(Debug, Default)]
pub struct Edit {
    text: String,
    cursor: usize,
    anchor: usize,
}

impl Edit {
    /// 建立编辑内容，光标位于末尾；换行/控制字符被移除。
    pub fn new(text: &str) -> Self {
        let text: String = text
            .chars()
            .filter(|character| !character.is_control())
            .collect();
        let cursor = text.len();
        Self {
            text,
            cursor,
            anchor: cursor,
        }
    }

    /// 擦除当前分配内的文字并重置选区；用于认证结束、失败或取消时清除密码。
    pub fn clear_secret(&mut self) {
        linux_uapi::credentials::erase(&mut self.text);
        self.cursor = 0;
        self.anchor = 0;
    }

    /// 当前原文。密码字段的绘制应使用 [`Edit::masked`]，不打印此返回值。
    pub fn text(&self) -> &str {
        &self.text
    }

    /// 返回按字符数生成的掩码，不按 UTF-8 字节数泄露字符宽度。
    pub fn masked(&self) -> String {
        "*".repeat(self.text.chars().count())
    }

    /// 当前光标的 UTF-8 字节偏移；始终位于字符边界。
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// 选中范围（UTF-8 字节偏移）；空范围表示没有选择。
    pub fn selection(&self) -> Range<usize> {
        self.anchor.min(self.cursor)..self.anchor.max(self.cursor)
    }

    fn move_to(&mut self, position: usize, selecting: bool) {
        self.cursor = position;
        if !selecting {
            self.anchor = position;
        }
    }

    fn replace_selection(&mut self, text: &str) -> bool {
        let range = self.selection();
        let start = range.start;
        let changed = &self.text[range.clone()] != text;
        self.text.replace_range(range, text);
        self.cursor = start + text.len();
        self.anchor = self.cursor;
        changed
    }

    /// 消费一个按键，返回内容/光标/选择是否改变。支持左右、Home/End、Shift 选择、Ctrl+A、删除和文本插入。
    /// 释放、Alt/Super 组合以及未支持的控制快捷键不编辑文本；不包含剪贴板或 IME。
    pub fn key(&mut self, key: Key) -> bool {
        if !key.pressed || key.modifiers.alt || key.modifiers.super_key {
            return false;
        }
        let before = (self.cursor, self.anchor);
        let mut changed = false;
        if key.modifiers.control {
            if key.code == 30 {
                self.anchor = 0;
                self.cursor = self.text.len();
            }
            return before != (self.cursor, self.anchor);
        }
        match key.code {
            105 => {
                let position = if !key.modifiers.shift && !self.selection().is_empty() {
                    self.selection().start
                } else {
                    self.text[..self.cursor]
                        .char_indices()
                        .next_back()
                        .map_or(0, |(index, _)| index)
                };
                self.move_to(position, key.modifiers.shift);
            }
            106 => {
                let position = if !key.modifiers.shift && !self.selection().is_empty() {
                    self.selection().end
                } else {
                    self.text[self.cursor..]
                        .chars()
                        .next()
                        .map_or(self.cursor, |character| self.cursor + character.len_utf8())
                };
                self.move_to(position, key.modifiers.shift);
            }
            102 => self.move_to(0, key.modifiers.shift),
            107 => self.move_to(self.text.len(), key.modifiers.shift),
            14 => {
                if self.selection().is_empty() {
                    self.anchor = self.text[..self.cursor]
                        .char_indices()
                        .next_back()
                        .map_or(0, |(index, _)| index);
                }
                changed = self.replace_selection("");
            }
            111 => {
                if self.selection().is_empty() {
                    self.anchor = self.text[self.cursor..]
                        .chars()
                        .next()
                        .map_or(self.cursor, |character| self.cursor + character.len_utf8());
                }
                changed = self.replace_selection("");
            }
            _ => {
                if let Some(character) = key.text.filter(|character| !character.is_control()) {
                    changed = self.replace_selection(character.encode_utf8(&mut [0; 4]));
                }
            }
        }
        changed || before != (self.cursor, self.anchor)
    }
}

/// 一组固定控件槽位的 Tab 顺序；禁用项不参与遍历。顺序与启用状态归调用方设置，当前焦点由此对象独占。
#[derive(Debug)]
pub struct FocusChain {
    enabled: Vec<bool>,
    current: Option<usize>,
}

impl FocusChain {
    /// 按控件顺序建立焦点链，初始选中第一个启用项；全部禁用时没有焦点。
    pub fn new(enabled: &[bool]) -> Self {
        Self {
            enabled: enabled.into(),
            current: enabled.iter().position(|enabled| *enabled),
        }
    }

    /// 当前焦点的槽位，没有可用项时返回 `None`。
    pub fn current(&self) -> Option<usize> {
        self.current
    }

    /// 显式选中启用槽位；索引无效或已禁用返回 `false`，保持原焦点。
    pub fn focus(&mut self, index: usize) -> bool {
        if self.enabled.get(index) != Some(&true) {
            return false;
        }
        self.current = Some(index);
        true
    }

    /// 修改启用状态；当前项被禁用时自动前进。索引越界返回 `false`。
    pub fn set_enabled(&mut self, index: usize, enabled: bool) -> bool {
        let Some(slot) = self.enabled.get_mut(index) else {
            return false;
        };
        *slot = enabled;
        if !enabled && self.current == Some(index) {
            self.advance(false);
        } else if enabled && self.current.is_none() {
            self.current = Some(index);
        }
        true
    }

    /// Tab 前进或 Shift+Tab 后退，循环跳过禁用项；返回新的焦点槽位。
    pub fn advance(&mut self, reverse: bool) -> Option<usize> {
        let count = self.enabled.len();
        if count == 0 {
            self.current = None;
            return None;
        }
        let start = self.current.unwrap_or(if reverse { 0 } else { count - 1 });
        self.current = (1..=count)
            .map(|step| {
                if reverse {
                    (start + count - step) % count
                } else {
                    (start + step) % count
                }
            })
            .find(|index| self.enabled[*index]);
        self.current
    }
}

impl crate::Painter<'_> {
    /// 绘制带选择区、水平滚动和光标的单行编辑框；密码用等字符数的 `*` 替代原文。
    /// `focused=false` 时隐藏选择与光标；返回值为编辑框内部的逻辑矩形。
    pub fn edit(
        &mut self,
        rect: canvas::Rect,
        edit: &Edit,
        focused: bool,
        password: bool,
    ) -> canvas::Rect {
        self.edit_with_leading(rect, edit, focused, password, 0)
    }

    /// 同 [`Painter::edit`]，文字区左侧让出 `leading` 逻辑像素给调用者自己画的图标（如地址栏文件夹图标）。
    pub fn edit_with_leading(
        &mut self,
        rect: canvas::Rect,
        edit: &Edit,
        focused: bool,
        password: bool,
        leading: i32,
    ) -> canvas::Rect {
        use crate::Ink;
        use classic_theme::SystemColor;
        let mut inner = self.edit_frame(rect, true).inset(2);
        inner.x += leading;
        inner.width -= leading;
        let masked;
        let (text, cursor, selection) = if password {
            masked = edit.masked();
            let selection = edit.selection();
            (
                masked.as_str(),
                edit.text()[..edit.cursor()].chars().count(),
                edit.text()[..selection.start].chars().count()
                    ..edit.text()[..selection.end].chars().count(),
            )
        } else {
            (edit.text(), edit.cursor(), edit.selection())
        };
        let caret = self.text_width(&text[..cursor], false);
        let scroll = (caret + self.scale - self.px(inner.width)).max(0);
        let shift = self.logical_ceil(scroll);
        let x = inner.x - shift;
        let y = inner.y + (inner.height - self.line_height()) / 2;
        let previous = self.surface.clip();
        self.surface
            .set_clip(previous.intersect(self.physical(inner)));
        self.text(x, y, text, Ink::Color(SystemColor::WindowText), false);
        if focused && !selection.is_empty() {
            let left = self.text_width(&text[..selection.start], false);
            let right = self.text_width(&text[..selection.end], false);
            let highlight = canvas::Rect::new(
                self.px(x) + left,
                self.px(y),
                right - left,
                self.px(self.line_height()),
            );
            self.surface
                .fill_rect(highlight, self.color(SystemColor::Hilight));
            self.surface.set_clip(
                previous
                    .intersect(self.physical(inner))
                    .intersect(highlight),
            );
            self.text(x, y, text, Ink::Color(SystemColor::HilightText), false);
            self.surface
                .set_clip(previous.intersect(self.physical(inner)));
        }
        if focused {
            self.surface.fill_rect(
                canvas::Rect::new(
                    self.px(x) + caret,
                    self.px(y),
                    self.scale,
                    self.px(self.line_height()),
                ),
                self.color(SystemColor::WindowText),
            );
        }
        self.surface.set_clip(previous);
        inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keyboard::Modifiers;
    fn key(code: u32, shift: bool, control: bool, text: Option<char>) -> Key {
        Key {
            code,
            pressed: true,
            repeat: false,
            modifiers: Modifiers {
                shift,
                control,
                ..Modifiers::default()
            },
            text,
        }
    }
    #[test]
    fn utf8_selection_replacement_and_delete_keep_character_boundaries() {
        let mut edit = Edit::new("中a文");
        edit.key(key(105, true, false, None));
        assert_eq!(edit.selection(), 4..7);
        edit.key(key(30, false, false, Some('x')));
        assert_eq!(edit.text(), "中ax");
        edit.key(key(102, false, false, None));
        edit.key(key(111, false, false, None));
        assert_eq!(edit.text(), "ax");
        edit.key(key(30, false, true, None));
        edit.key(key(30, false, false, Some('汉')));
        assert_eq!(edit.text(), "汉");
        assert_eq!(edit.masked(), "*");
        edit.key(key(14, false, false, None));
        assert!(edit.text().is_empty());
        assert!(!edit.key(key(14, false, false, None)));
    }
    #[test]
    fn tab_skips_disabled_controls_and_handles_empty_chains() {
        let mut chain = FocusChain::new(&[true, false, true]);
        assert_eq!(chain.advance(false), Some(2));
        assert_eq!(chain.advance(false), Some(0));
        assert_eq!(chain.advance(true), Some(2));
        chain.set_enabled(2, false);
        assert_eq!(chain.current(), Some(0));
        chain.set_enabled(0, false);
        assert_eq!(chain.current(), None);
        assert_eq!(FocusChain::new(&[]).advance(true), None);
        assert!(!chain.focus(5));
    }
}
