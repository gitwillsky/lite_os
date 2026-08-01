//! Client mapping ownership and compositor-driven buffer lifecycle.

use std::collections::VecDeque;
use std::io;

use display_proto::{Rect, Size};
use linux_uapi::drm::SharedDumbBuffer;

use super::{Display, invalid};

/// Damage 历史容量(单位:revision 条数)。
///
/// 只保留最近若干次 commit 的 damage;acquire 到的 buffer 若欠账跨越已被
/// 丢弃的更旧历史,无法精确重建,回退为全屏 damage(安全但保守)。
pub(super) const DAMAGE_HISTORY_CAP: usize = 8;

pub(super) struct Buffer {
    pub(super) id: u32,
    pub(super) pixels: SharedDumbBuffer,
    pub(super) free: bool,
    /// 本 buffer 像素最后一次对应的场景 revision;0 表示从未 commit(全新
    /// 映射,内容不可信)。acquire 据此计算 buffer-age 欠账:back buffer 里
    /// 缺的是 `(last_revision, 最新]` 之间每次 commit 上报的 damage。
    pub(super) last_revision: u64,
}

impl Buffer {
    /// Returns whether this mapping belongs to the active physical geometry.
    pub(super) fn matches(&self, physical: Size) -> bool {
        self.pixels.width() == physical.width as usize
            && self.pixels.height() == physical.height as usize
    }
}

/// Computes the damage a buffer with `last_revision` is missing.
///
/// The owed set is the union of every committed damage newer than
/// `last_revision`. A fresh mapping (`last_revision == 0`) and a debt spanning
/// already-dropped history (the oldest retained entry is not contiguous with
/// `last_revision`) both fall back to the full surface: the buffer contents
/// cannot be reconstructed exactly without the discarded revisions.
pub(super) fn owed_damage(
    history: &VecDeque<(u64, Vec<Rect>)>,
    last_revision: u64,
    full: Rect,
) -> Vec<Rect> {
    if last_revision == 0 {
        return vec![full];
    }
    let Some((oldest, _)) = history.front() else {
        // 几何代际切换会清空历史;幸存 buffer 的旧坐标系 damage 已无意义。
        return vec![full];
    };
    if *oldest > last_revision + 1 {
        return vec![full];
    }
    let mut owed = Vec::new();
    for (revision, damage) in history {
        if *revision > last_revision {
            owed.extend_from_slice(damage);
        }
    }
    owed
}

impl Display {
    /// Records one commit's damage and stamps its buffer with the revision.
    ///
    /// The renderer restores an acquired back buffer by repainting exactly the
    /// rects committed since that buffer's `last_revision`; without this record
    /// the next acquire would either over-restore (full frame) or under-restore
    /// (stale pixels). The move-underlay scratch buffer never commits a scene
    /// revision, so it stays at `last_revision == 0` and out of the debt model.
    pub(super) fn record_damage(&mut self, buffer_id: u32, revision: u64, damage: &[Rect]) {
        if let Some(buffer) = self
            .buffers
            .iter_mut()
            .find(|buffer| buffer.id == buffer_id)
        {
            buffer.last_revision = revision;
        }
        self.history.push_back((revision, damage.to_vec()));
        while self.history.len() > DAMAGE_HISTORY_CAP {
            self.history.pop_front();
        }
    }

    /// Makes one current-generation mapping writable again.
    pub(super) fn release(&mut self, id: u32) -> io::Result<()> {
        let Some(index) = self.buffers.iter().position(|buffer| buffer.id == id) else {
            return Err(invalid("unknown buffer released"));
        };
        let buffer = &mut self.buffers[index];
        if buffer.free {
            return Err(invalid("buffer released twice"));
        }
        buffer.free = true;
        Ok(())
    }

    /// Permanently removes one mapping from an obsolete geometry generation.
    pub(super) fn retire(&mut self, id: u32) -> io::Result<()> {
        let index = self
            .buffers
            .iter()
            .position(|buffer| buffer.id == id)
            .ok_or_else(|| invalid("unknown buffer retired"))?;
        self.buffers.remove(index);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use display_proto::Rect;

    use super::{DAMAGE_HISTORY_CAP, owed_damage};

    const FULL: Rect = Rect {
        x: 0,
        y: 0,
        width: 3008,
        height: 1692,
    };

    fn rect(x: i32) -> Rect {
        Rect {
            x,
            y: 0,
            width: 10,
            height: 10,
        }
    }

    fn history(entries: &[(u64, i32)]) -> VecDeque<(u64, Vec<Rect>)> {
        entries
            .iter()
            .map(|(revision, x)| (*revision, vec![rect(*x)]))
            .collect()
    }

    #[test]
    fn fresh_buffer_owes_the_full_surface() {
        assert_eq!(owed_damage(&history(&[(1, 10)]), 0, FULL), [FULL]);
        assert_eq!(owed_damage(&VecDeque::new(), 0, FULL), [FULL]);
    }

    #[test]
    fn debt_is_the_union_of_newer_committed_damage() {
        let history = history(&[(1, 10), (2, 20), (3, 30)]);
        // 双缓冲稳态:back buffer 落后两个 revision,欠两条 damage。
        assert_eq!(owed_damage(&history, 1, FULL), [rect(20), rect(30)]);
        assert_eq!(owed_damage(&history, 2, FULL), [rect(30)]);
        // 已是最新的 buffer 不欠任何内容。
        assert_eq!(owed_damage(&history, 3, FULL), []);
    }

    #[test]
    fn debt_spanning_dropped_history_falls_back_to_full() {
        // cap 8 条:只保留 revision 3..=10。last_revision = 1 时欠账跨越已被
        // 丢弃的 revision 2,无法精确重建,回退全屏。
        let entries = (3..=(DAMAGE_HISTORY_CAP as u64 + 2))
            .map(|revision| (revision, revision as i32 * 10))
            .collect::<Vec<_>>();
        let history = history(&entries);
        assert_eq!(history.len(), DAMAGE_HISTORY_CAP);
        assert_eq!(owed_damage(&history, 1, FULL), [FULL]);
        // 最旧保留条目与 last_revision 相接时仍能精确计算。
        assert_eq!(
            owed_damage(&history, 2, FULL),
            entries.iter().map(|(_, x)| rect(*x)).collect::<Vec<_>>()
        );
    }

    #[test]
    fn cleared_history_forces_full_damage() {
        // reconfigure 清空历史后,幸存 buffer 的旧 revision 不再有可对照的坐标系。
        assert_eq!(owed_damage(&VecDeque::new(), 7, FULL), [FULL]);
    }
}
