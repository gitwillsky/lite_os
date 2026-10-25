//! 登录界面的几何与绘制；输入命中与按钮绘制读取同一组区域。
use super::{Logon, Page};
use canvas::{Rect, Surface};
use classic_theme::{SystemColor as C, Theme};
use classic_ui::{Bitmap, ButtonState, Configuration, FrameKind, Ink, Painter};
use std::{io, path::Path};
use typeface::Typeface;

pub struct Art {
    banner: Bitmap,
    keyboard: Bitmap,
    font: Option<Typeface>,
    scale: i32,
}
impl Art {
    pub fn load() -> io::Result<Self> {
        Ok(Self {
            banner: Bitmap::load(Path::new("/usr/share/liteos/ui/logon-banner-1x.bmp"))?,
            keyboard: Bitmap::load(Path::new("/usr/share/liteos/ui/ctrl-alt-del-32.bmp"))?,
            font: None,
            scale: 0,
        })
    }
}
/// 登录类对话框的宽度：横幅位图 `logon-banner` 是 413 像素宽，加上左右各 3 像素的窗口框正好铺满客户区；
/// 宽于它横幅右侧会露出空白，窄于它横幅被裁掉。
const DIALOG_WIDTH: i32 = 413 + 6;

fn dialog(c: Configuration) -> Rect {
    Rect::new(
        (c.width - DIALOG_WIDTH) / 2,
        (c.height - 300) / 2,
        DIALOG_WIDTH,
        300,
    )
}

/// 贴在对话框右边距 10 像素处的一排按钮，`widths` 从左到右，间距 6；返回各按钮的 x（相对对话框）。
fn right_aligned(widths: &[i32]) -> Vec<i32> {
    let mut right = DIALOG_WIDTH - 10;
    let mut xs: Vec<i32> = widths
        .iter()
        .rev()
        .map(|width| {
            right -= width;
            let x = right;
            right -= 6;
            x
        })
        .collect();
    xs.reverse();
    xs
}
fn at(d: Rect, x: i32, y: i32, w: i32, h: i32) -> Rect {
    Rect::new(d.x + x, d.y + y, w, h)
}
pub fn areas(s: &Logon, c: Configuration) -> Vec<Rect> {
    let d = dialog(c);
    let row = |y: i32, widths: &[i32]| -> Vec<Rect> {
        right_aligned(widths)
            .into_iter()
            .zip(widths)
            .map(|(x, width)| at(d, x, y, *width, 23))
            .collect()
    };
    match s.page {
        Page::Credentials | Page::Unlock => {
            let mut areas = vec![at(d, 110, 132, 260, 23), at(d, 110, 166, 260, 23)];
            areas.extend(row(218, &[75, 75, 75, 94]));
            areas
        }
        Page::Password => {
            let mut areas = vec![
                at(d, 126, 124, 270, 23),
                at(d, 126, 158, 270, 23),
                at(d, 126, 192, 270, 23),
            ];
            areas.extend(row(250, &[75, 75]));
            areas
        }
        Page::Security => (0..6)
            .map(|i| at(d, 15 + (i % 3) * 132, 172 + (i / 3) * 34, 124, 25))
            .collect(),
        Page::Shutdown => {
            let mut areas = vec![
                at(d, 90, 123, 280, 23),
                at(d, 90, 151, 280, 23),
                at(d, 90, 179, 280, 23),
            ];
            areas.extend(row(250, &[75, 75]));
            areas
        }
        _ => row(250, &[75]),
    }
}
fn label(p: &mut Painter<'_>, d: Rect, x: i32, y: i32, text: &str) {
    p.text_fit(
        d.x + x,
        d.y + y,
        d.width - x - 12,
        text,
        Ink::Color(C::ButtonText),
        false,
    );
}
fn button(p: &mut Painter<'_>, s: &Logon, rect: Rect, index: usize, title: &str, default: bool) {
    p.push_button(
        rect,
        title,
        ButtonState {
            default,
            focused: s.focus == index,
            pressed: s.armed == Some(index),
            ..Default::default()
        },
    );
}
pub fn frame(
    s: &Logon,
    c: Configuration,
    art: &mut Art,
) -> Result<Vec<u32>, Box<dyn std::error::Error>> {
    if art.scale != c.scale {
        art.font = Some(Typeface::load(
            Path::new("/usr/share/liteos/fonts"),
            c.scale as u32,
        )?);
        art.scale = c.scale;
    }
    let (w, h) = c.pixel_size()?;
    let mut pixels = vec![0; w as usize * h as usize];
    let surface = Surface::new(&mut pixels, w, h, w as usize).ok_or("invalid logon pixels")?;
    let theme = Theme::classic();
    let mut p = Painter::new(surface, &theme, art.font.as_ref().unwrap(), c.scale);
    p.fill(Rect::new(0, 0, c.width, c.height), C::Background);
    if s.page == Page::Desktop {
        return Ok(pixels);
    }
    let d = dialog(c);
    p.fill(d, C::ButtonFace);
    let inner = p.window_frame(d, FrameKind::Fixed);
    let title = match s.page {
        Page::Welcome => "欢迎使用 LiteOS",
        Page::Credentials => "登录到 LiteOS",
        Page::Loading => "LiteOS",
        Page::Security => "LiteOS 安全",
        Page::Locked => "计算机已锁定",
        Page::Unlock => "解除计算机锁定",
        Page::Message => "登录消息",
        Page::Shutdown => "关闭 LiteOS",
        Page::Password => "更改密码",
        Page::Desktop => "",
    };
    p.caption_bar(
        Rect::new(inner.x, inner.y, inner.width, 19),
        title,
        None,
        true,
        &[],
        None,
    );
    art.banner.paint(&mut p, d.x + 3, d.y + 24);
    let areas = areas(s, c);
    match s.page {
        Page::Welcome => {
            art.keyboard.paint(&mut p, d.x + 28, d.y + 142);
            label(&mut p, d, 82, 143, "按 Ctrl-Alt-Delete 开始。");
            label(
                &mut p,
                d,
                82,
                170,
                "安全注意序列由显示服务直接交给登录界面。",
            );
            button(&mut p, s, areas[0], 0, "帮助", false);
        }
        Page::Credentials | Page::Unlock => {
            label(&mut p, d, 25, 137, "用户名(U):");
            label(&mut p, d, 25, 171, "密码(P):");
            p.edit(areas[0], &s.username, s.focus == 0, false);
            p.edit(areas[1], &s.password, s.focus == 1, true);
            for (i, title) in [
                "确定",
                "取消",
                "关机...",
                if s.options { "选项 <<" } else { "选项 >>" },
            ]
            .iter()
            .enumerate()
            {
                if s.page != Page::Unlock || i < 2 {
                    button(&mut p, s, areas[i + 2], i + 2, title, i == 0);
                }
            }
            if s.options {
                label(
                    &mut p,
                    d,
                    25,
                    258,
                    "登录到: LiteOS（本机）     拨号网络登录不可用",
                );
            }
        }
        Page::Password => {
            for (i, (title, edit)) in [
                ("原密码:", &s.password),
                ("新密码:", &s.new_password),
                ("确认新密码:", &s.confirmation),
            ]
            .iter()
            .enumerate()
            {
                label(&mut p, d, 25, 129 + i as i32 * 34, title);
                p.edit(areas[i], edit, s.focus == i, true);
            }
            button(&mut p, s, areas[3], 3, "确定", true);
            button(&mut p, s, areas[4], 4, "取消", false);
        }
        Page::Loading => {
            label(&mut p, d, 115, 155, "正在加载您的个人设置...");
        }
        Page::Security => {
            let user = s.session.as_ref().map_or("", |s| s.user.name.as_str());
            label(&mut p, d, 25, 124, &format!("您以 {user} 身份登录。"));
            if let Some(session) = &s.session {
                label(
                    &mut p,
                    d,
                    25,
                    145,
                    &format!("登录日期: {}", session.login_time),
                );
            }
            for (i, title) in [
                "锁定计算机",
                "注销...",
                "关机...",
                "更改密码...",
                "任务管理器",
                "取消",
            ]
            .iter()
            .enumerate()
            {
                button(&mut p, s, areas[i], i, title, i == 5);
            }
        }
        Page::Locked => {
            label(&mut p, d, 50, 142, "当前用户或本机管理员可以解除会话锁定。");
            label(&mut p, d, 50, 174, "按 Ctrl-Alt-Delete 解锁。");
        }
        Page::Message => {
            for (i, line) in typeface::wrap(&s.message, 390 * c.scale, |ch| {
                art.font.as_ref().unwrap().measure(&ch.to_string(), false)
            })
            .iter()
            .enumerate()
            {
                label(&mut p, d, 25, 130 + i as i32 * 19, &s.message[line.clone()]);
            }
            button(&mut p, s, areas[0], 0, "确定", true);
        }
        Page::Shutdown => {
            for (i, title) in ["注销当前用户", "关机", "重新启动"].iter().enumerate() {
                p.radio(
                    areas[i].x,
                    areas[i].y,
                    s.shutdown == i,
                    i == 0 && s.session.is_none(),
                );
                label(&mut p, d, 110, 123 + i as i32 * 28, title);
            }
            button(&mut p, s, areas[3], 3, "确定", true);
            button(&mut p, s, areas[4], 4, "取消", false);
        }
        Page::Desktop => {}
    }
    Ok(pixels)
}
