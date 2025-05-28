use crate::timer;

pub fn sys_get_time_msec() -> isize {
    timer::get_time_msec() as isize
}
