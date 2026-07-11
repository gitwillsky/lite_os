use crate::task::current_task;

/// @description 查询或设置当前进程的数据段结尾。
///
/// @param new_brk 新的数据段结尾；为零时查询当前值。
/// @return Linux `brk` 语义：成功返回新 break，失败返回未改变的旧 break。
pub(crate) fn sys_brk(new_brk: usize) -> isize {
    let task = current_task().expect("brk requires a current task");
    let current = task
        .set_program_break(0)
        .expect("user address space must own a heap area");
    task.set_program_break(new_brk).unwrap_or(current) as isize
}
