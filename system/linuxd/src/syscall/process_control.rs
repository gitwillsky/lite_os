use crate::{
    execution::task::{current_task, parent_death_signal},
    syscall::errno,
};

const PR_SET_PDEATHSIG: u32 = 1;
const PR_GET_PDEATHSIG: u32 = 2;
const PR_GET_DUMPABLE: u32 = 3;
const PR_SET_NAME: u32 = 15;
const PR_GET_NAME: u32 = 16;
const PR_SET_DUMPABLE: u32 = 4;
const PR_GET_SECCOMP: u32 = 21;
const PR_SET_SECCOMP: u32 = 22;
const PR_SET_NO_NEW_PRIVS: u32 = 38;
const PR_GET_NO_NEW_PRIVS: u32 = 39;

/// 实现 Linux `prctl` 的 PDEATH、MM dumpability、单向 no_new_privs 与 seccomp operations。
///
/// # Parameters
///
/// - `option`: Linux `int` 的低 32 位 selector。
/// - `argument`: PDEATHSIG SET 的 signal 或 GET 的 `int *`；DUMPABLE SET 为零/一；NO_NEW_PRIVS SET 为一、GET 为零。
/// - `remaining`: Linux arg3/arg4/arg5；NO_NEW_PRIVS 要求它们全为零，SET_SECCOMP 从 arg3 取 filter。
///
/// # Returns
///
/// SET/PDEATHSIG 成功返回零；GET_DUMPABLE/GET_NO_NEW_PRIVS 返回零或一，GET_SECCOMP 返回 mode。
///
/// # Errors
///
/// selector/signal 非法返回 `EINVAL`；GET copyout 失败返回 `EFAULT`。
pub(crate) fn sys_prctl(option: usize, argument: usize, remaining: [usize; 3]) -> isize {
    match option as u32 {
        PR_GET_DUMPABLE => isize::from(
            current_task()
                .expect("dumpability prctl requires current Thread")
                .user_dumpable(),
        ),
        PR_SET_DUMPABLE => {
            if argument > 1 {
                return -errno::EINVAL;
            }
            current_task()
                .expect("dumpability prctl requires current Thread")
                .set_user_dumpable(argument != 0);
            0
        }
        PR_SET_NAME => {
            let task = current_task().expect("prctl requires current task");
            let mut name = [0u8; 16];
            if task.copy_from_user(argument, &mut name).is_err() {
                return -errno::EFAULT;
            }
            task.set_comm(&name);
            0
        }
        PR_GET_NAME => {
            let task = current_task().expect("prctl requires current task");
            task.copy_to_user(argument, &task.comm_buffer())
                .map_or(-errno::EFAULT, |()| 0)
        }
        PR_GET_SECCOMP => super::seccomp::get_mode(),
        option @ (7 | 8 | 23 | 24 | 27 | 28 | 47) => current_task()
            .expect("capability prctl requires current Thread")
            .capability_prctl(option, argument, remaining)
            .map_or_else(super::capabilities::capability_error, |value| {
                value as isize
            }),
        PR_SET_SECCOMP => super::seccomp::set_mode(argument, remaining[0]),
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
        PR_SET_NO_NEW_PRIVS => {
            if argument != 1 || remaining != [0; 3] {
                return -errno::EINVAL;
            }
            current_task()
                .expect("prctl requires current task")
                .enable_no_new_privileges();
            0
        }
        PR_GET_NO_NEW_PRIVS => {
            if argument != 0 || remaining != [0; 3] {
                return -errno::EINVAL;
            }
            isize::from(
                current_task()
                    .expect("prctl requires current task")
                    .no_new_privileges(),
            )
        }
        _ => -errno::EINVAL,
    }
}
