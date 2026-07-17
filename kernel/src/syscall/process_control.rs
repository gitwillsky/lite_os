use crate::{
    syscall::errno,
    task::{current_task, parent_death_signal},
};

const PR_SET_PDEATHSIG: usize = 1;
const PR_GET_PDEATHSIG: usize = 2;

/// @description 实现 Linux `prctl` 当前开放的 parent-death signal operations。
/// @param option 标准 `PR_SET_PDEATHSIG` 或 `PR_GET_PDEATHSIG` selector。
/// @param argument SET 的 signal value，或 GET 的 `int *` userspace pointer。
/// @return 成功返回零。
/// @errors selector/signal 非法返回 `EINVAL`；GET copyout 失败返回 `EFAULT`。
pub(crate) fn sys_prctl(option: usize, argument: usize) -> isize {
    match option {
        PR_SET_PDEATHSIG => parent_death_signal(Some(argument)).map_or(-errno::EINVAL, |_| 0),
        PR_GET_PDEATHSIG => {
            let signal = match parent_death_signal(None) {
                Ok(signal) => signal as i32,
                Err(()) => return -errno::EINVAL,
            };
            current_task()
                .expect("prctl requires current task")
                .copy_to_user(argument, &signal.to_ne_bytes())
                .map_or(-errno::EFAULT, |()| 0)
        }
        _ => -errno::EINVAL,
    }
}
