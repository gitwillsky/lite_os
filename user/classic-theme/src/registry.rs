use std::fmt::Write;

use crate::{SystemColor, Theme};

/// 96 DPI 下 1 逻辑像素对应 15 twips；Win32 把 WindowMetrics 存成负的 twips，与 DPI 无关。
const TWIPS_PER_PIXEL: i32 = 15;

impl Theme {
    /// 生成 Wine 前缀的 `.reg` 文本，使 Win32 程序读到与桌面相同的颜色与度量。
    ///
    /// # Parameters
    ///
    /// - `scale`: 输出的整数 scale；Wine 用 `LogPixels = 96 * scale` 把 twips 换算成物理像素，
    ///   度量值本身不随 scale 变化。
    pub fn wine_registry(&self, scale: u32) -> String {
        let mut out = String::from("Windows Registry Editor Version 5.00\n");
        out.push_str("\n[HKEY_CURRENT_USER\\Control Panel\\Colors]\n");
        for color in SystemColor::ALL {
            let rgb = self.color(*color);
            let _ = writeln!(
                out,
                "\"{}\"=\"{} {} {}\"",
                color.name(),
                rgb.red,
                rgb.green,
                rgb.blue
            );
        }
        out.push_str("\n[HKEY_CURRENT_USER\\Control Panel\\Desktop\\WindowMetrics]\n");
        for (key, pixels) in self.window.entries() {
            let _ = writeln!(out, "\"{key}\"=\"{}\"", -pixels * TWIPS_PER_PIXEL);
        }
        let _ = writeln!(out, "\"AppliedDPI\"=dword:{:08x}", 96 * scale);
        out.push_str("\n[HKEY_CURRENT_CONFIG\\Software\\Fonts]\n");
        let _ = writeln!(out, "\"LogPixels\"=dword:{:08x}", 96 * scale);
        out
    }
}
