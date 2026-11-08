//! 新的区域合成不能仅靠编译验证：与整帧参考逐像素对账，覆盖真实的双缓冲旧内容和透明混合。
use super::*;

#[test]
fn incremental_scanout_matches_full_frames_across_exposure_reordering_and_alpha() {
    let theme = Theme::classic();
    let screen = Rect::new(0, 0, 16, 12);
    let color = theme.color(SystemColor::Background);
    let mut history = damage::History::default();
    let mut buffers = [vec![0x12345678; 192], vec![0xabcdef12; 192]];
    let mut reference = vec![0; 192];
    // 设备端每个 framebuffer 有自己的历史副本；仅上传相对前台的损伤会漏掉隔帧变化。
    let mut resident = [vec![0; 192], vec![0; 192]];
    let mut front = Content::new(5, 5, vec![0xff40a060; 25]);
    let lower = Content::new(16, 12, vec![0xff206080; 192]);
    let glass = Content::new(6, 5, vec![0x80400040; 30]);
    for step in 0..12 {
        if step == 3 {
            front = Content::new(5, 5, vec![0xff60a040; 25]);
        }
        let mut base = Drawable::surface(0, 0, &lower, 1);
        base.identity = Some((1, 1));
        base.opaque = true;
        let mut top = Drawable::surface(if step < 5 { 3 } else { 7 }, 2, &front, 1);
        top.identity = Some((1, 2));
        top.opaque = true;
        let change = [Rect::new(0, 0, 3, 5), Rect::new(2, 0, 3, 5)];
        if step == 3 {
            // 两块重叠损伤不能让透明混合重复执行。
            top.damage = &change;
        }
        let mut transparent = Drawable::surface(5, 4, &glass, 1);
        transparent.identity = Some((1, 3));
        let mut windows = vec![base, top];
        if step < 8 {
            windows.push(transparent);
        }
        if step == 7 {
            windows.swap(1, 2);
        }
        let back = step % 2;
        let frame = history.prepare(back, screen, color, &windows, None);
        paint(
            Surface::new(&mut buffers[back], 16, 12, 16).unwrap(),
            &theme,
            &windows,
            &frame,
            |_, _| {},
        )
        .unwrap();
        let mut whole = Surface::new(&mut reference, 16, 12, 16).unwrap();
        whole.fill_rect(screen, Color::rgb(color.red, color.green, color.blue));
        for window in &windows {
            whole.blit_sampled(
                Image {
                    pixels: window.content.pixels().unwrap(),
                    width: window.content.width as i32,
                    height: window.content.height as i32,
                    stride: window.content.width,
                },
                window.rect,
                window.content.source,
            );
        }
        assert_eq!(buffers[back], reference, "stale output at frame {step}");
        for rect in frame.repair_rects() {
            for y in rect.y as usize..rect.bottom() as usize {
                let start = y * 16 + rect.x as usize;
                let end = start + rect.width as usize;
                resident[back][start..end].copy_from_slice(&buffers[back][start..end]);
            }
        }
        assert_eq!(
            resident[back], reference,
            "stale device replica at frame {step}"
        );
        if step == 2 || step == 11 {
            assert!(
                frame.windows.iter().all(|region| region.is_empty()),
                "unchanged repaired buffers do no raster work"
            );
        }
        history.record(back, frame);
    }
}
