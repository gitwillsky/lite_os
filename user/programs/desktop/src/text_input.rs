//! 桌面直接拥有的原地重命名与运行框，向 Wayland 公布当前编辑目标。

use super::{Desktop, icons, paint};
use classic_ui::Display;
use std::io;

impl Desktop {
    pub(super) fn sync_text_target(
        &self,
        display: &mut Display,
        art: &mut paint::Art,
    ) -> io::Result<()> {
        let focus = display.keyboard_focus();
        let field = if focus == Some(self.background)
            && let Some(rename) = &self.rename
            && let Some(index) = self.icons.items.iter().position(
                |icon| matches!(&icon.target, icons::Target::Path(path) if path == rename.operation.path()),
            )
            && let Some(configuration) = display.configuration(self.background)
        {
            let icon = &self.icons.items[index];
            let height = art.desktop_icon_bitmap(icon, self.icons.trash_full).map_or(32, classic_ui::Bitmap::height);
            let rect = classic_ui::large_icon_edit_rect(self.icons.rect(index), height);
            let caret = art.with_probe(configuration.scale, |p| p.edit_caret(rect, &rename.edit, true))?;
            Some((self.background, index as u64 + 1, caret, classic_ui::TextPurpose::Normal))
        } else if let Some(dialog) = self.dialogs.iter().find(|dialog| Some(dialog.window) == focus)
            && let Some(configuration) = display.configuration(dialog.window)
        {
            let rect = canvas::Rect::new(82, 84, configuration.width - 105, 23);
            let caret = art.with_probe(configuration.scale, |p| p.edit_caret(rect, &dialog.edit, true))?;
            Some((dialog.window, 1, caret, classic_ui::TextPurpose::Normal))
        } else { None };
        display.set_text_input(field)
    }
}
