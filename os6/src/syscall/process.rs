//! Process management syscalls

use crate::mm::{translated_refmut, translated_ref, translated_str, VirtAddr, MapPermission, PageTable};
use crate::task::{add_task, current_add_area, current_delete_page, current_task, current_user_token, exit_current_and_run_next, suspend_current_and_run_next, TaskStatus};
use crate::fs::{open_file, OpenFlags};
use crate::timer::get_time_us;
use alloc::sync::Arc;
use alloc::vec::Vec;
use crate::config::MAX_SYSCALL_NUM;
use alloc::string::String;

#[repr(C)]
#[derive(Debug)]
pub struct TimeVal {
    pub sec: usize,
    pub usec: usize,
}

#[derive(Clone, Copy)]
pub struct TaskInfo {
    pub status: TaskStatus,
    pub syscall_times: [u32; MAX_SYSCALL_NUM],
    pub time: usize,
}

pub fn sys_exit(exit_code: i32) -> ! {
    debug!("[kernel] Application exited with code {}", exit_code);
    exit_current_and_run_next(exit_code);
    panic!("Unreachable in sys_exit!");
}

/// current task gives up resources for other tasks
pub fn sys_yield() -> isize {
    suspend_current_and_run_next();
    0
}

pub fn sys_getpid() -> isize {
    current_task().unwrap().pid.0 as isize
}

/// Syscall Fork which returns 0 for child process and child_pid for parent process
pub fn sys_fork() -> isize {
    let current_task = current_task().unwrap();
    let new_task = current_task.fork();
    let new_pid = new_task.pid.0;
    // modify trap context of new_task, because it returns immediately after switching
    let trap_cx = new_task.inner_exclusive_access().get_trap_cx();
    // we do not have to move to next instruction since we have done it before
    // for child process, fork returns 0
    trap_cx.x[10] = 0;
    // add new task to scheduler
    add_task(new_task);
    new_pid as isize
}

/// Syscall Exec which accepts the elf path
pub fn sys_exec(path: *const u8) -> isize {
    let token = current_user_token();
    let path = translated_str(token, path);
    if let Some(app_inode) = open_file(path.as_str(), OpenFlags::RDONLY) {
        let all_data = app_inode.read_all();
        let task = current_task().unwrap();
        task.exec(all_data.as_slice());
        0
    } else {
        -1
    }
}


/// If there is not a child process whose pid is same as given, return -1.
/// Else if there is a child process but it is still running, return -2.
pub fn sys_waitpid(pid: isize, exit_code_ptr: *mut i32) -> isize {
    let task = current_task().unwrap();
    // find a child process

    // ---- access current TCB exclusively
    let mut inner = task.inner_exclusive_access();
    if !inner
        .children
        .iter()
        .any(|p| pid == -1 || pid as usize == p.getpid())
    {
        return -1;
        // ---- release current PCB
    }
    let pair = inner.children.iter().enumerate().find(|(_, p)| {
        // ++++ temporarily access child PCB lock exclusively
        p.inner_exclusive_access().is_zombie() && (pid == -1 || pid as usize == p.getpid())
        // ++++ release child PCB
    });
    if let Some((idx, _)) = pair {
        let child = inner.children.remove(idx);
        // confirm that child will be deallocated after removing from children list
        assert_eq!(Arc::strong_count(&child), 1);
        let found_pid = child.getpid();
        // ++++ temporarily access child TCB exclusively
        let exit_code = child.inner_exclusive_access().exit_code;
        // ++++ release child PCB
        *translated_refmut(inner.memory_set.token(), exit_code_ptr) = exit_code;
        found_pid as isize
    } else {
        -2
    }
    // ---- release current PCB lock automatically
}

// YOUR JOB: 引入虚地址后重写 sys_get_time
pub fn sys_get_time(ts: *mut TimeVal, _tz: usize) -> isize {
    let us = get_time_us();
    // 先找到当前任务的token
    // 通过构造pagetable来进行转换得到物理地址
    // 再通过物理地址获设置对应时间
    let token = current_user_token();
    let ptr = translated_refmut(token,ts);
    // debug!("[kernel] us: {},ptr: {:#x}",us,ptr as *mut TimeVal as usize);
    *ptr = TimeVal {
        sec: us / 1_000_000,
        usec: us % 1_000_000,
    };
    0
}

// YOUR JOB: 引入虚地址后重写 sys_task_info
pub fn sys_task_info(ti: *mut TaskInfo) -> isize {
    0
}

// YOUR JOB: 实现sys_set_priority，为任务添加优先级
pub fn sys_set_priority(_prio: isize) -> isize {
    -1
}

// YOUR JOB: 扩展内核以实现 sys_mmap 和 sys_munmap
pub fn sys_mmap(start: usize, len: usize, port: usize) -> isize {
    let start_vir: VirtAddr = start.into(); //与页大小对齐
    //除了低8位其它位必须为0;
    //低8位不能全部为0
    if start_vir.aligned() != true || (port & !0x7 != 0) || (port & 0x7 == 0) {
        return -1;
    }
    //判断是否已经存在某个页被映射
    let new_port: u8 = (port & 0x7) as u8;
    let permission = MapPermission::U;
    let map_permission = MapPermission::from_bits(new_port << 1).unwrap() | permission;

    let start_vpn = start_vir.floor(); //起始页
    let end_vpn = VirtAddr::from(start + len).ceil(); //向上取整结束页

    //申请到一个map_area后判断其每个页是否出现在map_area中过
    let current_user_token = current_user_token(); //获取当前用户程序的satp
    let temp_page_table = PageTable::from_token(current_user_token);
    for vpn in start_vpn.0 ..end_vpn.0 {
        if let Some(_val) = temp_page_table.find_pte(vpn.into()) {
            error!("[kernel] mmap failed, page {:#x} already exists",vpn);
            return -1;
        } //提前返回错误值
    }
    current_add_area(start_vir, (start + len).into(), map_permission);
    0
}

pub fn sys_munmap(start: usize, len: usize) -> isize {
    let start_vir: VirtAddr = start.into(); //与页大小对齐
    if !start_vir.aligned() {
        return -1;
    }
    let start_vpn = start_vir.floor(); //起始页
    let end_vpn = VirtAddr::from(start + len).ceil(); //向上取整结束页
    let current_user_token = current_user_token(); //获取当前用户程序的satp
    let temp_page_table = PageTable::from_token(current_user_token);
    for vpn in start_vpn.0..end_vpn.0 {
        if temp_page_table.find_pte(vpn.into()).is_none() {
            return -1;
        } //提前返回错误值,如果这些页存在不位于内存的则错误返回
    }
    current_delete_page(start_vir);
    0
}

//
// YOUR JOB: 实现 sys_spawn 系统调用
// ALERT: 注意在实现 SPAWN 时不需要复制父进程地址空间，SPAWN != FORK + EXEC 
pub fn sys_spawn(path: *const u8) -> isize {
    //完成新建子进程并执行应用程序的功能，即将exec与fork合并的功能
    //这里的实现是spawn不必像fork一样复制父进程地址空间和内容
    let token = current_user_token();
    let name = translated_str(token,path);//查找是否存在此应用程序
    let task = current_task().unwrap();
    if let Some(app_inode) = open_file(name.as_str(), OpenFlags::RDONLY) {
        let all_data = app_inode.read_all();
        let task = current_task().unwrap();
        task.spawn(all_data.as_slice())
    } else {
        -1
    }

}
