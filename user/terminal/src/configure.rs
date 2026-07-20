//! `CONFIGURE`/resize 事务：按桌面建议尺寸替换 surface 的 backing buffer。

use display_proto as proto;

use crate::{
    atlas::{Atlas, FontMetrics},
    client::{Session, send_commit},
    model::{Grid, Model},
    render::{self, Surface},
    session::set_window_size,
};

const MIN_COLUMNS: usize = 8;
const MAX_COLUMNS: usize = 200;
const MIN_ROWS: usize = 4;
const MAX_ROWS: usize = 100;

/// 处理 `CONFIGURE`：按桌面建议尺寸走 resize 事务替换 backing buffer。
///
/// 尺寸换算成 cell 并 clamp 到 [`MIN_COLUMNS`]..=[`MAX_COLUMNS`] ×
/// [`MIN_ROWS`]..=[`MAX_ROWS`]，与当前相同则忽略。事务顺序：`prepare_resize`
/// 只做 reflow 候选 → 新像素尺寸 `Surface::create` → `TIOCSWINSZ`（内核向
/// foreground process group 发 SIGWINCH）→ 候选整幅 `render_full` 进新 buffer →
/// `SET_BUFFER`（新 handle 所有权随之转移给桌面）+ 整幅 `COMMIT` → 全部成功才
/// `commit_resize` 并替换 surface（旧 surface 的 Drop 仅 munmap，旧 handle 由
/// 桌面切换后销毁）。任一步失败保持旧 grid 与旧 buffer 不变并静默忽略（候选
/// 与新 surface 由各自 Drop 回收）。
pub(crate) fn handle_configure(
    payload: &[u8],
    session: &Session,
    surface: &mut Surface,
    atlas: &Atlas,
    model: &mut Model,
    metrics: FontMetrics,
    focused: bool,
) {
    let Some(configure) = proto::Configure::parse(payload) else {
        return;
    };
    if configure.surface_id != session.surface_id {
        return;
    }
    let columns = (configure.width as usize / metrics.width()).clamp(MIN_COLUMNS, MAX_COLUMNS);
    let rows = (configure.height as usize / metrics.height()).clamp(MIN_ROWS, MAX_ROWS);
    if columns == model.columns() && rows == model.rows() {
        return;
    }
    let pixel_width = columns * metrics.width();
    let pixel_height = rows * metrics.height();
    let Some(candidate) = model.prepare_resize(columns, rows) else {
        return;
    };
    let created = Surface::create(session.drm_fd, pixel_width as u32, pixel_height as u32);
    let Some((mut next, gem_handle)) = created else {
        return;
    };
    // pixel 分量最大 200×16 / 100×32 = 3200，u16 不溢出。
    if set_window_size(session.master, columns, rows, pixel_width as u16, pixel_height as u16)
        .is_err()
    {
        return;
    }
    render::render_full(&mut next, &candidate, atlas, metrics, focused);
    let set_buffer = proto::SetBuffer {
        surface_id: session.surface_id,
        gem_handle,
        width: pixel_width as u32,
        height: pixel_height as u32,
    };
    if send_set_buffer(session.socket, set_buffer).is_err()
        || send_commit(session.socket, session.surface_id, &[]).is_err()
    {
        return;
    }
    model.commit_resize(candidate);
    // 新 buffer 已是整幅最新画面；清掉 commit_resize 标的全脏，避免重复 COMMIT。
    model.clear_all_dirty();
    *surface = next;
}

/// 发送一条 `SET_BUFFER`；成功时 `set_buffer.gem_handle` 所有权转移给桌面。
fn send_set_buffer(socket: i32, set_buffer: proto::SetBuffer) -> Result<(), ()> {
    let mut buf = [0u8; 24];
    let length = set_buffer.encode(&mut buf).ok_or(())?;
    proto::send_message(socket, &buf[..length]).map_err(|_| ())
}
