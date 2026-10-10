//! Delayed user-copy crosses the actual target installation; neither timing nor a policy stub decides it.
use super::*;

struct DelayedFault {
    memory: Memory,
    handoff: Arc<InstallHandoff>,
}
impl UserMemory for DelayedFault {
    fn pid_namespace(&self) -> Arc<namespace::PidNamespace> {
        self.memory.pid_namespace()
    }
    fn read(&self, address: usize, bytes: &mut [u8]) -> Result<(), UserFault> {
        self.memory.read(address, bytes)
    }
    fn write(&self, address: usize, bytes: &[u8]) -> Result<(), UserFault> {
        self.memory.write(address, bytes)?;
        self.handoff.enter();
        Err(UserFault)
    }
}

#[test]
fn failed_delayed_receive_cannot_reopen_an_inflight_atomic_injection() {
    initialize_pipe_scheduler();
    for fail in [false, true] {
        let install_handoff = InstallHandoff::new();
        let copy_handoff = InstallHandoff::new();
        let target = TaskControlBlock::new(53, Some(install_handoff.clone()));
        target.files.lock().unwrap().fail = fail;
        let receiver = TaskControlBlock::new(96, None);
        let injector = TaskControlBlock::new(97, None);
        injector.files.lock().unwrap().entries.insert(
            3,
            Arc::new(file::OpenFileDescription {
                identity: 3001,
                device: None,
            }),
        );
        let listener = install(&target, RET_USER_NOTIF, FILTER_FLAG_NEW_LISTENER).unwrap();
        let worker = spawn_request(&target);
        target.target.wait_until_sleeping();
        let delayed = Arc::new(DelayedFault {
            memory: Memory::new(vec![0; 80]),
            handoff: copy_handoff.clone(),
        });
        let receiving_listener = listener.clone();
        let receiving_task = receiver.clone();
        let receiving_memory = delayed.clone();
        let receiving = std::thread::spawn(move || {
            let _binding = bind(&receiving_task);
            receiving_listener.ioctl(&IoctlCall {
                fd_readable: true,
                request: IOCTL_NOTIF_RECV,
                argument: 1,
                user: receiving_memory.as_ref(),
                nonblocking: false,
            })
        });
        copy_handoff.wait_entered();
        let id = delayed.memory.id();
        let injecting_listener = listener.clone();
        let injecting_task = injector.clone();
        let injecting = std::thread::spawn(move || {
            let _binding = bind(&injecting_task);
            ioctl(
                &injecting_listener,
                IOCTL_NOTIF_ADDFD,
                &addfd(id, ADDFD_FLAG_SEND, 3, 0, false),
            )
        });
        install_handoff.wait_entered();
        copy_handoff.release();
        receiver.target.wait_until_sleeping(); // Actual recovery event, while target installation is held.
        assert_eq!(listener.poll(1 | 4), 0);
        assert_eq!(
            ioctl(&listener, IOCTL_NOTIF_SEND, &response(id, 99, 0, 0)),
            Err(DeviceError::Errno(errno::EINPROGRESS))
        );
        {
            let _binding = bind(&injector);
            assert_eq!(
                ioctl(
                    &listener,
                    IOCTL_NOTIF_ADDFD,
                    &addfd(id, ADDFD_FLAG_SEND, 3, 0, false)
                ),
                Err(DeviceError::Errno(errno::EINPROGRESS))
            );
        }
        install_handoff.release();
        assert_eq!(
            injecting.join().unwrap(),
            Ok(if fail { -errno::EMFILE } else { 7 })
        );
        assert_eq!(
            receiving.join().unwrap(),
            Err(DeviceError::Errno(errno::EFAULT))
        );
        if fail {
            assert_ne!(listener.poll(1) & 1, 0);
            let retried = Memory::new(vec![0; 80]);
            assert_eq!(ioctl(&listener, IOCTL_NOTIF_RECV, &retried), Ok(0));
            assert_eq!(retried.id(), id);
            assert_eq!(
                ioctl(&listener, IOCTL_NOTIF_SEND, &response(id, 33, 0, 0)),
                Ok(0)
            );
            assert_eq!(worker.join().unwrap(), Ok(Some(33)));
            assert!(target.files.lock().unwrap().installed.is_empty());
        } else {
            assert_eq!(worker.join().unwrap(), Ok(Some(7)));
            assert_eq!(listener.poll(1 | 4), 0);
            assert_eq!(
                target.files.lock().unwrap().installed,
                vec![(7, 3001, false)]
            );
        }
    }
}
