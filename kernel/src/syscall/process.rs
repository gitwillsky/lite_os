use alloc::sync::Arc;

use crate::{
    loader::get_app_data_by_name,
    memory::page_table::{translated_ref_mut, translated_str},
    task::{
        self, current_task, current_user_token, exit_current_and_run_next,
        suspend_current_and_run_next,
    },
};

pub fn sys_exit(exit_code: i32) -> ! {
    exit_current_and_run_next(exit_code);
    unreachable!()
}

pub fn sys_yield() -> isize {
    suspend_current_and_run_next();
    0
}

pub fn sys_fork() -> isize {
    let current_task = current_task().unwrap();
    let new_task = current_task.fork();
    let new_pid = new_task.get_pid();

    let trap_cx = new_task.inner_exclusive_access().get_trap_cx();

    // child fork return 0, so ra = 0
    trap_cx.x[10] = 0;
    task::add_task(new_task);

    new_pid as isize
}

pub fn sys_exec(path: *const u8) -> isize {
    let token = current_user_token();
    let path_str = translated_str(token, path);
    
    if let Some(elf_data) = get_app_data_by_name(&path_str) {
        let task = current_task().unwrap();
        task.exec(&elf_data);
        0
    } else {
        -1
    }
}

pub fn sys_wait_pid(pid: isize, exit_code_ptr: *mut i32) -> isize {
    let task = current_task().unwrap();

    let mut inner = task.inner_exclusive_access();

    if inner
        .children
        .iter()
        .find(|p| pid == -1 || pid as usize == p.get_pid())
        .is_none()
    {
        return -1;
    }

    let pair = inner.children.iter().enumerate().find(|(_, t)| {
        t.inner_exclusive_access().is_zombie() && (pid == -1 || t.get_pid() == pid as usize)
    });

    if let Some((idx, _)) = pair {
        let child = inner.children.remove(idx);
        assert_eq!(
            Arc::strong_count(&child),
            1,
            "Leaked Arc reference to child process!"
        );
        let found_pid = child.get_pid();
        let exit_code = child.inner_exclusive_access().exit_code;
        let parent_token = inner.get_user_token();
        *translated_ref_mut(parent_token, exit_code_ptr) = exit_code;
        found_pid as isize
    } else {
        -2
    }
}

pub fn sys_shutdown() -> ! {
    crate::arch::sbi::shutdown();
    unreachable!();
}
