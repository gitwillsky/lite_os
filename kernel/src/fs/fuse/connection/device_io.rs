//! /dev/fuse 的消息收发与回复验证；队列状态仍由父模块的 Connection 独占。
use super::*;

impl Connection {
    /// 守护进程 read（Linux `fuse_dev_do_read`）：中断优先，其次按配额交替 FORGET 与普通请求。
    pub(in crate::fs::fuse) fn read(
        &self,
        device: u64,
        output: &mut dyn UserOutput,
        nonblocking: bool,
        wait: impl Fn() -> Result<(), DeviceError>,
    ) -> Result<(), DeviceError> {
        let max_write = self
            .negotiated
            .get()
            .map_or(0, |negotiated| negotiated.max_write as usize);
        if output.remaining() < fuse::MIN_READ_BUFFER.max(fuse::IN_HEADER_SIZE + 40 + max_write) {
            return Err(DeviceError::Errno(errno::EINVAL));
        }
        loop {
            let mut queue = self.queue.lock();
            match queue.link {
                Link::Connected => {}
                Link::Detached => return Err(DeviceError::Errno(errno::ENODEV)),
                Link::Aborted => return Err(DeviceError::Errno(errno::ECONNABORTED)),
            }
            if let Some(request) = queue.interrupts.pop_front() {
                drop(queue);
                return self.write_interrupt(&request, output);
            }
            if !queue.forgets.is_empty() {
                let take_forget = queue.pending.is_empty() || {
                    queue.forget_batch -= 1;
                    queue.forget_batch >= 0
                };
                if take_forget {
                    return self.write_forgets(queue, output);
                }
                if queue.forget_batch <= -8 {
                    queue.forget_batch = FORGET_BATCH;
                }
            }
            let Some(request) = queue.pending.pop_front() else {
                drop(queue);
                if nonblocking {
                    return Err(DeviceError::WouldBlock);
                }
                wait()?;
                continue;
            };
            if request.message.len() > output.remaining() {
                drop(queue);
                self.finish(&request, Err(FileSystemError::IoError));
                continue;
            }
            if queue.processing.try_reserve(1).is_err() {
                // 刚弹出一个元素，push_front 不会分配。
                queue.pending.push_front(request);
                return Err(DeviceError::Errno(errno::ENOMEM));
            }
            *request.phase.lock() = Phase::Sent { device };
            queue.processing.push(request.clone());
            drop(queue);
            if output.write(&request.message).is_err() {
                self.take_processing(request.unique, device);
                self.finish(&request, Err(FileSystemError::IoError));
                return Err(DeviceError::Errno(errno::EFAULT));
            }
            if request.interrupted.load(Ordering::Acquire) {
                self.queue_interrupt(&request);
            }
            return Ok(());
        }
    }

    fn write_interrupt(
        &self,
        request: &Request,
        output: &mut dyn UserOutput,
    ) -> Result<(), DeviceError> {
        const LENGTH: usize = fuse::IN_HEADER_SIZE + 8;
        let header = protocol::header(
            LENGTH as u32,
            fuse::INTERRUPT,
            request.unique | 1,
            0,
            Caller::default(),
        );
        output.write(&header).map_err(fault)?;
        output.write(&request.unique.to_ne_bytes()).map_err(fault)
    }

    fn write_forgets(
        &self,
        mut queue: spin::MutexGuard<'_, Queue>,
        output: &mut dyn UserOutput,
    ) -> Result<(), DeviceError> {
        let unique = self.next_unique.fetch_add(2, Ordering::Relaxed) + 2;
        let batch = self
            .negotiated
            .get()
            .is_some_and(|negotiated| negotiated.minor >= 16)
            && queue.forgets.len() > 1;
        if !batch {
            let (nodeid, lookups) = queue.forgets.pop_front().expect("forget pending");
            drop(queue);
            let header = protocol::header(
                (fuse::IN_HEADER_SIZE + 8) as u32,
                fuse::FORGET,
                unique,
                nodeid,
                Caller::default(),
            );
            output.write(&header).map_err(fault)?;
            return output.write(&lookups.to_ne_bytes()).map_err(fault);
        }
        let room = (output.remaining() - fuse::IN_HEADER_SIZE - 8) / 16;
        let count = queue.forgets.len().min(room);
        let mut entries = [(0u64, 0u64); 32];
        let count = count.min(entries.len());
        for entry in &mut entries[..count] {
            *entry = queue.forgets.pop_front().expect("counted forget");
        }
        drop(queue);
        let header = protocol::header(
            (fuse::IN_HEADER_SIZE + 8 + count * 16) as u32,
            fuse::BATCH_FORGET,
            unique,
            0,
            Caller::default(),
        );
        output.write(&header).map_err(fault)?;
        output
            .write(&Fields::<8>::new().u32(count as u32).finish())
            .map_err(fault)?;
        for (nodeid, lookups) in &entries[..count] {
            output
                .write(&Fields::<16>::new().u64(*nodeid).u64(*lookups).finish())
                .map_err(fault)?;
        }
        Ok(())
    }

    fn take_processing(&self, unique: u64, device: u64) -> Option<Arc<Request>> {
        let mut queue = self.queue.lock();
        let index = queue.processing.iter().position(|request| {
            request.unique == unique
                && matches!(*request.phase.lock(), Phase::Sent { device: owner } if owner == device)
        })?;
        let request = queue.processing.swap_remove(index);
        if request.interrupted.load(Ordering::Acquire)
            && let Some(index) = queue
                .interrupts
                .iter()
                .position(|queued| Arc::ptr_eq(queued, &request))
        {
            queue.interrupts.remove(index);
        }
        Some(request)
    }

    /// 守护进程 write（Linux `fuse_dev_do_write`）：回复、中断回复或通知。
    ///
    /// `notify` 处理 unique 为零的通知消息（错误字段是通知码，负载已复制）。
    pub(in crate::fs::fuse) fn write(
        &self,
        device: u64,
        input: &mut dyn UserInput,
        notify: impl FnOnce(i32, &[u8]) -> Result<(), FileSystemError>,
    ) -> Result<(), DeviceError> {
        let length = input.remaining();
        if length < fuse::OUT_HEADER_SIZE {
            return Err(DeviceError::Errno(errno::EINVAL));
        }
        let mut header = [0u8; fuse::OUT_HEADER_SIZE];
        input.copy(&mut header).map_err(fault)?;
        let mut reader = Reader::new(&header);
        let declared = reader.u32().unwrap() as usize;
        let error = reader.u32().unwrap() as i32;
        let unique = reader.u64().unwrap();
        if declared != length {
            return Err(DeviceError::Errno(errno::EINVAL));
        }
        let payload_length = length - fuse::OUT_HEADER_SIZE;
        if unique == 0 {
            let payload = copy_payload(input, payload_length)?;
            notify(error, &payload[fuse::OUT_HEADER_SIZE..]).map_err(device_error)?;
            input.consume(length);
            return Ok(());
        }
        if !(-511..=0).contains(&error) {
            return Err(DeviceError::Errno(errno::EINVAL));
        }
        if unique & 1 != 0 {
            let request = {
                let queue = self.queue.lock();
                queue
                    .processing
                    .iter()
                    .find(|request| {
                        request.unique == unique & !1
                            && matches!(*request.phase.lock(), Phase::Sent { device: owner } if owner == device)
                    })
                    .cloned()
            }
            .ok_or(DeviceError::Errno(errno::ENOENT))?;
            if payload_length != 0 {
                return Err(DeviceError::Errno(errno::EINVAL));
            }
            match -(error as isize) {
                errno::ENOSYS => self.queue.lock().no_interrupt = true,
                errno::EAGAIN => self.queue_interrupt(&request),
                _ => {}
            }
            input.consume(length);
            return Ok(());
        }
        let request = self
            .take_processing(unique, device)
            .ok_or(DeviceError::Errno(errno::ENOENT))?;
        let result = if error != 0 {
            if payload_length != 0 {
                Err(errno::EINVAL)
            } else {
                Ok(Err(protocol::reply_error((-error) as u16)))
            }
        } else if payload_length > request.reply.size
            || payload_length < request.reply.size && !request.reply.variable
        {
            Err(errno::EINVAL)
        } else {
            match copy_payload(input, payload_length) {
                Ok(mut payload) => {
                    payload.drain(..fuse::OUT_HEADER_SIZE);
                    Ok(Ok(payload))
                }
                Err(DeviceError::Errno(errno::ENOMEM)) => Ok(Err(FileSystemError::OutOfMemory)),
                Err(error) => {
                    self.finish(&request, Err(FileSystemError::IoError));
                    return Err(error);
                }
            }
        };
        match result {
            Ok(reply) => {
                self.finish(&request, reply);
                input.consume(length);
                Ok(())
            }
            Err(code) => {
                self.finish(&request, Err(FileSystemError::IoError));
                Err(DeviceError::Errno(code))
            }
        }
    }
}
