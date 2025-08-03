//! 作业控制模块

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use user_lib::{kill, wait_pid_nb};

/// 作业状态
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum JobStatus {
    Running,     // 运行中
    Stopped,     // 暂停
    Done,        // 完成
    Terminated,  // 被终止
}

/// 作业信息
#[derive(Debug, Clone)]
pub struct Job {
    pub id: usize,           // 作业编号
    pub pid: isize,          // 进程 ID
    pub command: String,     // 命令行
    pub status: JobStatus,   // 状态
    pub background: bool,    // 是否为后台作业
}

/// 作业管理器
pub struct JobManager {
    jobs: Vec<Job>,
    next_job_id: usize,
    foreground_job: Option<usize>, // 当前前台作业 ID
}

impl JobManager {
    pub fn new() -> Self {
        Self {
            jobs: Vec::new(),
            next_job_id: 1,
            foreground_job: None,
        }
    }

    /// 添加新作业
    pub fn add_job(&mut self, pid: isize, command: String, background: bool) -> usize {
        let job_id = self.next_job_id;
        self.next_job_id += 1;

        let job = Job {
            id: job_id,
            pid,
            command,
            status: JobStatus::Running,
            background,
        };

        if !background {
            self.foreground_job = Some(job_id);
        }

        self.jobs.push(job);
        job_id
    }

    /// 获取作业
    pub fn get_job(&self, job_id: usize) -> Option<&Job> {
        self.jobs.iter().find(|job| job.id == job_id)
    }

    /// 获取可变作业
    pub fn get_job_mut(&mut self, job_id: usize) -> Option<&mut Job> {
        self.jobs.iter_mut().find(|job| job.id == job_id)
    }

    /// 根据PID获取作业
    pub fn get_job_by_pid(&mut self, pid: isize) -> Option<&mut Job> {
        self.jobs.iter_mut().find(|job| job.pid == pid)
    }

    /// 将作业移到前台
    pub fn bring_to_foreground(&mut self, job_id: usize) -> Result<(), String> {
        // 先检查作业是否存在
        let job_exists = self.jobs.iter().any(|job| job.id == job_id);
        if !job_exists {
            return Err(format!("作业 {} 不存在", job_id));
        }

        // 找到作业并获取需要的信息
        for job in &mut self.jobs {
            if job.id == job_id {
                if job.status == JobStatus::Stopped {
                    // 发送SIGCONT信号继续执行
                    if kill(job.pid as usize, 18) != 0 { // SIGCONT = 18
                        return Err(format!("无法继续作业 {}", job_id));
                    }
                    job.status = JobStatus::Running;
                }
                job.background = false;
                break;
            }
        }

        // 设置前台作业
        self.foreground_job = Some(job_id);
        Ok(())
    }

    /// 将作业移到后台
    pub fn send_to_background(&mut self, job_id: usize) -> Result<(), String> {
        // 先检查作业是否存在
        let job_exists = self.jobs.iter().any(|job| job.id == job_id);
        if !job_exists {
            return Err(format!("作业 {} 不存在", job_id));
        }

        // 找到作业并获取需要的信息
        let mut job_command = String::new();
        for job in &mut self.jobs {
            if job.id == job_id {
                if job.status == JobStatus::Stopped {
                    // 发送SIGCONT信号继续执行
                    if kill(job.pid as usize, 18) != 0 { // SIGCONT = 18
                        return Err(format!("无法继续作业 {}", job_id));
                    }
                    job.status = JobStatus::Running;
                }
                job.background = true;
                job_command = job.command.clone();
                break;
            }
        }

        // 更新前台作业状态
        if self.foreground_job == Some(job_id) {
            self.foreground_job = None;
        }
        println!("[{}] {} &", job_id, job_command);
        Ok(())
    }

    /// 列出所有作业
    pub fn list_jobs(&self) {
        for job in &self.jobs {
            if job.status != JobStatus::Done {
                let status_str = match job.status {
                    JobStatus::Running => if job.background { "Running" } else { "Foreground" },
                    JobStatus::Stopped => "Stopped",
                    JobStatus::Done => "Done",
                    JobStatus::Terminated => "Terminated",
                };

                let bg_indicator = if job.background { " &" } else { "" };
                println!("[{}] {} ({}) {}{}",
                    job.id,
                    job.pid,
                    status_str,
                    job.command,
                    bg_indicator
                );
            }
        }
    }

    /// 清理已完成的作业
    pub fn cleanup_finished_jobs(&mut self) {
        self.jobs.retain(|job| job.status != JobStatus::Done && job.status != JobStatus::Terminated);
    }

    /// 检查并更新作业状态（非阻塞式）
    /// 返回是否有前台作业完成
    pub fn check_job_status(&mut self) -> bool {
        let mut foreground_job_completed = false;

        for job in &mut self.jobs {
            if job.status == JobStatus::Running {
                let mut exit_code = 0i32;
                // 使用非阻塞的wait_pid_nb
                let result = wait_pid_nb(job.pid as usize, &mut exit_code);

                if result == job.pid {
                    // 作业已终止
                    job.status = if exit_code == 0 { JobStatus::Done } else { JobStatus::Terminated };
                    if job.background {
                        println!("[{}] {} {}",
                            job.id,
                            if exit_code == 0 { "Done" } else { "Terminated" },
                            job.command
                        );
                    }
                    if self.foreground_job == Some(job.id) {
                        self.foreground_job = None;
                        foreground_job_completed = true;
                    }
                }
                // 如果result == -2，表示作业还在运行，不做任何操作
                // 如果result == -1，表示进程不存在（已经被回收）
            }
        }

        foreground_job_completed
    }

    /// 获取当前前台作业
    pub fn get_foreground_job(&self) -> Option<&Job> {
        if let Some(job_id) = self.foreground_job {
            self.get_job(job_id)
        } else {
            None
        }
    }

    /// 停止前台作业（Ctrl+Z）
    pub fn suspend_foreground_job(&mut self) -> Result<(), String> {
        if let Some(job_id) = self.foreground_job {
            // 先获取作业信息
            let mut job_command = String::new();
            let mut job_pid = 0;
            for job in &self.jobs {
                if job.id == job_id {
                    job_command = job.command.clone();
                    job_pid = job.pid;
                    break;
                }
            }

            // 发送SIGTSTP信号
            if kill(job_pid as usize, 20) == 0 { // SIGTSTP = 20
                // 更新作业状态
                for job in &mut self.jobs {
                    if job.id == job_id {
                        job.status = JobStatus::Stopped;
                        job.background = true;
                        break;
                    }
                }
                self.foreground_job = None;
                println!("[{}] Stopped {}", job_id, job_command);
                Ok(())
            } else {
                Err("无法停止前台作业".to_string())
            }
        } else {
            Err("没有前台作业".to_string())
        }
    }

    /// 终止前台作业（Ctrl+C）
    pub fn terminate_foreground_job(&mut self) -> Result<(), String> {
        if let Some(job_id) = self.foreground_job {
            if let Some(job) = self.get_job_mut(job_id) {
                // 发送SIGINT信号
                if kill(job.pid as usize, 2) == 0 { // SIGINT = 2
                    job.status = JobStatus::Terminated;
                    self.foreground_job = None;
                    Ok(())
                } else {
                    Err("无法终止前台作业".to_string())
                }
            } else {
                Err("没有有效的前台作业".to_string())
            }
        } else {
            Err("没有前台作业".to_string())
        }
    }
}