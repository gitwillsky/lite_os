/// @description 清除 line discipline 已接收但 userspace 尚未读取的全部状态。
/// @param input_head cooked ring 当前 head。
/// @param input_len cooked ring 当前 byte 数。
/// @param line_len canonical 模式尚未提交的当前行长度。
/// @param eof_pending 尚未由 read 消费的 canonical EOF。
/// @return 清理前是否存在任何 pending cooked/line/EOF state。
pub(super) fn clear_pending(
    input_head: &mut usize,
    input_len: &mut usize,
    line_len: &mut usize,
    eof_pending: &mut bool,
) -> bool {
    let changed = *input_len != 0 || *line_len != 0 || *eof_pending;
    *input_head = 0;
    *input_len = 0;
    *line_len = 0;
    *eof_pending = false;
    changed
}

/// @description 清除固定 raw ring 的 cursor 与全部未消费 bytes。
/// @param head raw ring 当前 head。
/// @param length raw ring 当前 byte 数。
/// @return 被丢弃的 byte 数。
pub(crate) fn clear_raw(head: &mut usize, length: &mut usize) -> usize {
    let discarded = *length;
    *head = 0;
    *length = 0;
    discarded
}

#[cfg(test)]
mod tests {
    use super::{clear_pending, clear_raw};

    #[test]
    fn flush_clears_cooked_partial_line_and_eof_state() {
        let mut head = 17;
        let mut input_len = 23;
        let mut line_len = 5;
        let mut eof = true;

        assert!(clear_pending(
            &mut head,
            &mut input_len,
            &mut line_len,
            &mut eof,
        ));
        assert_eq!((head, input_len, line_len, eof), (0, 0, 0, false));
        assert!(!clear_pending(
            &mut head,
            &mut input_len,
            &mut line_len,
            &mut eof,
        ));
    }

    #[test]
    fn flush_discards_the_complete_raw_ring() {
        let mut head = 4090;
        let mut length = 37;

        assert_eq!(clear_raw(&mut head, &mut length), 37);
        assert_eq!((head, length), (0, 0));
        assert_eq!(clear_raw(&mut head, &mut length), 0);
    }
}
