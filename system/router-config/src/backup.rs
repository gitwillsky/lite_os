//! 同服务内的备份执行器；独立线程拥有定时 I/O，不持配置锁等待文件系统，不新增服务或索引文件。

use super::*;
use router_protocol::backup::{Activity, Id, Listing, Policy, Status as BackupStatus};

mod archive;

const RETRY_NS: i64 = 30_000_000_000;

enum Pending {
    Save { id: Id, bytes: Vec<u8> },
    Prune,
}
enum Phase {
    Disabled,
    Waiting {
        at: i64,
        pending: Option<Pending>,
        error: Option<Status>,
    },
    Working,
}
enum Action {
    Wait(i64),
    Run {
        pending: Option<Pending>,
        previous_error: Option<Status>,
    },
}

/// 调度器唯一状态；锁只保护短状态转换，IO 中的工作被独占移出，Working 表示实际执行中。
pub(super) struct Scheduler {
    // 上次接纳的策略用于检测变更；缺失会在每次租约/配置通知时重置周期，永久推迟备份。
    policy: Option<Policy>,
    phase: Phase,
}
impl Default for Scheduler {
    fn default() -> Self {
        Self {
            policy: None,
            phase: Phase::Disabled,
        }
    }
}
impl Scheduler {
    fn next(&mut self, wanted: Option<Policy>, now: i64) -> (Action, Option<Phase>) {
        let mut retired = None;
        if self.policy != wanted {
            self.policy = wanted;
            // 新策略立即捕获一次；禁用只取消未来工作，不删除已存在副本。
            let phase = if wanted.is_some() {
                Phase::Waiting {
                    at: now,
                    pending: None,
                    error: None,
                }
            } else {
                Phase::Disabled
            };
            retired = Some(core::mem::replace(&mut self.phase, phase));
        }
        let action = match &self.phase {
            Phase::Disabled => Action::Wait(DEADLINE_INFINITE),
            Phase::Waiting { at, .. } if *at > now => Action::Wait(*at),
            Phase::Waiting { .. } => {
                let Phase::Waiting { pending, error, .. } =
                    core::mem::replace(&mut self.phase, Phase::Working)
                else {
                    unreachable!()
                };
                Action::Run {
                    pending,
                    previous_error: error,
                }
            }
            Phase::Working => unreachable!("only the backup worker advances its scheduler"),
        };
        (action, retired)
    }
    fn status(&self, now: i64) -> BackupStatus {
        match &self.phase {
            Phase::Disabled => BackupStatus {
                activity: Activity::Disabled,
                next_in_seconds: None,
                error: None,
            },
            Phase::Working => BackupStatus {
                activity: Activity::Working,
                next_in_seconds: None,
                error: None,
            },
            Phase::Waiting { at, error, .. } => BackupStatus {
                activity: if error.is_some() {
                    Activity::Retrying
                } else {
                    Activity::Waiting
                },
                next_in_seconds: Some(at.saturating_sub(now).max(0) as u64 / 1_000_000_000),
                error: *error,
            },
        }
    }
}

impl RouterConfig {
    fn backup_identity(&self) -> Result<Id, Status> {
        let previous = archive::list(&self.storage)?.first().map(|entry| entry.id);
        let mut random = [0; 16];
        native_runtime::system::cprng_draw(&mut random);
        Id::after(
            previous,
            native_runtime::system::utc().max(0) as u64 / 1_000_000,
            u128::from_ne_bytes(random),
        )
        .map_err(|_| native_abi::ERR_OUT_OF_RANGE)
    }
    fn publish_backup_change(&self, notification: Arc<Event>) -> Result<Revision, Status> {
        let mut inner = self.inner.lock();
        inner
            .generation
            .checked_next()
            .ok_or(native_abi::ERR_OUT_OF_RANGE)?;
        Self::publish_notification(&mut inner, notification);
        Ok(inner.generation)
    }
    /// 目录是唯一索引；调度状态只读投影，不读取每个备份的完整内容。
    pub(super) fn backup_list(&self) -> Result<Vec<u8>, Status> {
        let entries = archive::list(&self.storage)?;
        let automatic = self
            .backup
            .lock()
            .status(native_runtime::system::monotonic());
        Listing { entries, automatic }
            .encode()
            .map_err(|_| native_abi::ERR_INVALID_ARGS)
    }
    pub(super) fn backup_export(&self, id: Id) -> Result<Vec<u8>, Status> {
        archive::read(&self.storage, id)
    }
    pub(super) fn backup_delete(&self, id: Id) -> Result<Revision, Status> {
        let notification = Self::prepare_notification(&self.inner.lock())?;
        let result = archive::remove(&self.storage, id);
        // 删除错误也可能已改变命名状态；唤醒观察者重新查询，不声称删除已撤销。
        let generation = self.publish_backup_change(notification)?;
        result.map(|()| generation)
    }
    pub(super) fn backup_create(&self) -> Result<(Revision, Id), Status> {
        let (current, notification) = {
            let inner = self.inner.lock();
            (
                inner
                    .saved
                    .as_ref()
                    .ok_or(native_abi::ERR_BAD_STATE)?
                    .clone(),
                Self::prepare_notification(&inner)?,
            )
        };
        let id = self.backup_identity()?;
        let bytes = current.encode().map_err(|_| native_abi::ERR_INVALID_ARGS)?;
        let mut touched = false;
        let result = archive::save(&self.storage, id, false, &bytes, &mut touched);
        let generation = if touched {
            self.publish_backup_change(notification)?
        } else {
            self.inner.lock().generation
        };
        result.map(|()| (generation, id))
    }

    fn perform_backup(
        &self,
        pending: &mut Option<Pending>,
        touched: &mut bool,
    ) -> Result<(), Status> {
        if pending.is_none() {
            let current = self
                .inner
                .lock()
                .saved
                .as_ref()
                .ok_or(native_abi::ERR_BAD_STATE)?
                .clone();
            let id = self.backup_identity()?;
            let bytes = current.encode().map_err(|_| native_abi::ERR_INVALID_ARGS)?;
            *pending = Some(Pending::Save { id, bytes });
        }
        if let Some(Pending::Save { id, bytes }) = pending {
            archive::save(&self.storage, *id, true, bytes, touched)?;
            // 确认保存后释放大字节缓冲；清理失败只重试清理，不再创建新副本。
            *pending = Some(Pending::Prune);
        }
        let policy = self
            .inner
            .lock()
            .saved
            .as_ref()
            .and_then(|saved| saved.backup);
        if let Some(policy) = policy {
            archive::prune(&self.storage, policy.retain.get(), touched)?;
        }
        *pending = None;
        Ok(())
    }

    /// 与配置代际使用同一持久 Event 通知，等待者不清信号；禁用时无周期唤醒。
    pub(super) fn run_backups(&self) {
        loop {
            let (policy, changed) = {
                let inner = self.inner.lock();
                (
                    inner.saved.as_ref().and_then(|saved| saved.backup),
                    inner.changed.clone(),
                )
            };
            let (action, retired) = self
                .backup
                .lock()
                .next(policy, native_runtime::system::monotonic());
            // 被取消的失败快照可能很大，不能在调度状态锁中释放其字节缓冲。
            drop(retired);
            match action {
                Action::Wait(deadline) => match changed.wait_one(signals::USER_0, deadline) {
                    Ok(_) | Err(native_abi::ERR_TIMED_OUT) => {}
                    Err(error) => panic!("backup scheduler wait failed: {error}"),
                },
                Action::Run {
                    mut pending,
                    previous_error,
                } => {
                    let notification = Self::prepare_notification(&self.inner.lock());
                    let mut touched = false;
                    let result = match &notification {
                        Ok(_) => self.perform_backup(&mut pending, &mut touched),
                        Err(error) => Err(*error),
                    };
                    let now = native_runtime::system::monotonic();
                    let error = result.err();
                    {
                        let mut scheduler = self.backup.lock();
                        let interval = scheduler.policy.map_or(RETRY_NS, |policy| {
                            i64::from(policy.interval_seconds.get()) * 1_000_000_000
                        });
                        scheduler.phase = Phase::Waiting {
                            at: now.saturating_add(if error.is_some() {
                                RETRY_NS
                            } else {
                                interval
                            }),
                            pending,
                            error,
                        };
                    }
                    if error != previous_error
                        && let Some(error) = error
                    {
                        error!("router-config: backup pending: {error}");
                    }
                    // Namespace 不确定或任务失败均需要可见结果；没有新定时线程/常驻数据缓存。
                    if (touched || error != previous_error)
                        && let Ok(notification) = notification
                    {
                        let _ = self.publish_backup_change(notification);
                    }
                }
            }
        }
    }
}
