//! Image 资源选择与调色板的稳定 ABI 入口；应用只提供颜色，不接触解码或绘制。
use super::{Context, abi, invalid, invoke};
use classic_ui::Bitmap;
use std::{ffi::c_void, io, rc::Rc, slice};

/// 创建和替换 Image 共用同一资源选择，避免整张位图被替换入口误当作图标。
pub(super) fn resource(context: &mut Context, name: &str, width: i32) -> Option<Rc<Bitmap>> {
    match width {
        16 | 32 => context.icon_asset(name, 0, width),
        -1 if !name.ends_with("-1x") => context.icon_asset(name, 0, 32),
        _ => context.ui_bitmap(name),
    }
}

pub(super) unsafe extern "C" fn palette(
    pointer: *mut c_void,
    window: u64,
    id: u64,
    colors: *const u32,
    count: usize,
) -> i32 {
    if count != 256 || colors.is_null() {
        return abi::INVALID;
    }
    // SAFETY: Api 输入契约保证 colors 在调用中为有效的 u32 数组；先校验数量/非空。
    // invoke 独占 context；新像素完整生成后才替换 Node.image，输入指针不跨越本次调用。
    unsafe {
        invoke(pointer, |context| {
            let colors: &[u32; 256] = slice::from_raw_parts(colors, count).try_into().unwrap();
            if colors.iter().any(|color| color & 0xff000000 != 0) {
                return Err(invalid("palette colors must be RGB"));
            }
            let host = context.host_mut(window)?;
            let node = host
                .controls_mut()?
                .nodes
                .get_mut(&id)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown widget"))?;
            if node.kind != abi::widget::IMAGE {
                return Err(invalid("palette requires an Image widget"));
            }
            if let Some(image) = &node.image {
                node.image = Some(Rc::new(image.with_palette(colors)?));
                host.frame.invalidate();
            }
            Ok(())
        })
    }
}
