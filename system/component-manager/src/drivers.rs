//! 根服务持有设备目录、宿主 Job 及管理器更新决策。驱动节点状态由当前管理器监督。

use alloc::vec;
use alloc::vec::Vec;
use core::future::{Future, poll_fn};
use core::task::Poll;

use launch_protocol::{driver_host, driver_manager, driver_update};
use native_runtime::bootfs::Bootfs;
use native_runtime::executor::Executor;
use native_runtime::native_abi::{self, Status, rights, signals};
use native_runtime::sync::Once;
use native_runtime::{Channel, Event, Handle, Job, Process, Resource, Vmo, info};

// OWNER: 根服务的更新请求入口；服务路由只移交独立连接，不保存管理器实例或节点状态。
static REQUESTS: Once<Channel> = Once::new();

pub(super) fn connect(connection: Channel) -> Result<(), Status> {
    REQUESTS
        .get()
        .ok_or(native_abi::ERR_BAD_STATE)?
        .write(&[], vec![connection.into_handle()])
}

struct Instance {
    job: Job,
    process: Process,
    control: Channel,
}

pub(super) struct Drivers {
    root: Job,
    resource: Resource,
    device_tree: Handle,
    publisher: Channel,
    hosts: Job,
    host_image: Vmo,
    requests: Channel,
    current: Instance,
}

impl Drivers {
    pub(super) fn start(
        root: &Job,
        resource: &Resource,
        bootfs: &Bootfs,
        device_tree: Handle,
        publisher: Channel,
    ) -> Result<Self, Status> {
        let root = root.duplicate(rights::SAME)?;
        let resource = resource.duplicate(rights::SAME)?;
        let (client, requests) = Channel::create()?;
        let hosts = Job::create(&root)?;
        let host_image = bootfs.file_vmo(driver_host::IMAGE)?;
        let image = bootfs.image(driver_manager::IMAGE)?;
        let current = launch(
            &root,
            &image,
            vec![
                resource.duplicate(rights::SAME)?.into_handle(),
                device_tree.duplicate(rights::SAME)?,
                publisher.duplicate(rights::SAME)?.into_handle(),
                hosts.duplicate(rights::SAME)?.into_handle(),
                host_image.duplicate(rights::SAME)?.into_handle(),
            ],
        )?;
        REQUESTS.call_once(|| client);
        Ok(Self {
            root,
            resource,
            device_tree,
            publisher,
            hosts,
            host_image,
            requests,
            current,
        })
    }

    pub(super) async fn supervise(mut self, executor: &'static Executor) -> i64 {
        loop {
            let connection = {
                let process = self
                    .current
                    .process
                    .duplicate(rights::SAME)
                    .expect("driver manager supervision handle");
                let mut exited =
                    core::pin::pin!(executor.on_signals(&process, signals::TERMINATED));
                let mut request = core::pin::pin!(
                    executor.read_until(&self.requests, native_abi::DEADLINE_INFINITE)
                );
                poll_fn(|cx| {
                    if exited.as_mut().poll(cx).is_ready() {
                        return Poll::Ready(None);
                    }
                    request.as_mut().poll(cx).map(Some)
                })
                .await
            };
            let Some(Ok(connection)) = connection else {
                return super::power_off(&self.current.process, "driver-manager");
            };
            let Some(client) = connection
                .handles
                .into_iter()
                .next()
                .map(Channel::from_handle)
            else {
                continue;
            };
            let response = self
                .update(executor, &client)
                .await
                .unwrap_or_else(|status| driver_update::reply(status, native_abi::OK));
            let _ = client.write(&response, Vec::new());
        }
    }

    async fn update(
        &mut self,
        executor: &'static Executor,
        client: &Channel,
    ) -> Result<[u8; 8], Status> {
        let message = executor.read_until(client, deadline()).await?;
        let [image] =
            <[_; 1]>::try_from(message.handles).map_err(|_| native_abi::ERR_INVALID_ARGS)?;
        let address = u64::from_le_bytes(
            message
                .bytes
                .as_slice()
                .try_into()
                .map_err(|_| native_abi::ERR_INVALID_ARGS)?,
        );
        if address != 0 {
            let result = request(
                executor,
                &self.current.control,
                address,
                vec![image],
                native_abi::DEADLINE_INFINITE,
            )
            .await;
            match &result {
                Ok(bytes) => {
                    let status = i32::from_le_bytes(bytes[..4].try_into().expect("status bytes"));
                    let rollback =
                        i32::from_le_bytes(bytes[4..].try_into().expect("rollback bytes"));
                    info!(
                        "driver update {address:#x}: status {status}, rollback status {rollback}"
                    );
                }
                Err(status) => {
                    info!("driver update {address:#x}: control request failed: {status}")
                }
            }
            return result;
        }
        self.replace(executor, &Vmo::from_handle(image)).await?;
        Ok(driver_update::reply(native_abi::OK, native_abi::OK))
    }

    async fn replace(&mut self, executor: &'static Executor, image: &Vmo) -> Result<(), Status> {
        let size = image.size()?;
        if size == 0 || size > driver_update::IMAGE_LIMIT {
            return Err(native_abi::ERR_OUT_OF_RANGE);
        }
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(size as usize)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        bytes.resize(size as usize, 0);
        image.read(&mut bytes, 0)?;
        let note = driver_update::note(driver_update::MANAGER_NOTE);
        if !native_runtime::elf::has_note(&bytes, &note)? {
            return Err(native_abi::ERR_NOT_SUPPORTED);
        }
        let (transfer, receive) = Channel::create()?;
        let decision = Event::create()?;
        let old_decision = decision.duplicate(rights::SAME)?;
        // 客户端提供的 VMO 之后可能被它自己改写：先复制成映像，不能与其共享页。
        let candidate = launch(
            &self.root,
            &native_runtime::elf::Image::parse(&bytes)?,
            vec![
                self.resource.duplicate(rights::SAME)?.into_handle(),
                self.device_tree.duplicate(rights::SAME)?,
                self.publisher.duplicate(rights::SAME)?.into_handle(),
                self.hosts.duplicate(rights::SAME)?.into_handle(),
                self.host_image.duplicate(rights::SAME)?.into_handle(),
                receive.into_handle(),
                decision.duplicate(rights::SAME)?.into_handle(),
            ],
        )?;
        drop(bytes);
        let result = async {
            let limit = deadline();
            let response = request(
                executor,
                &self.current.control,
                0,
                vec![transfer.into_handle(), old_decision.into_handle()],
                limit,
            )
            .await?;
            let status = i32::from_le_bytes(response[..4].try_into().expect("status bytes"));
            if status != native_abi::OK {
                return Err(status);
            }
            let ready = executor.read_until(&candidate.control, limit).await?;
            if ready.bytes != driver_update::VERSION || !ready.handles.is_empty() {
                return Err(native_abi::ERR_INVALID_ARGS);
            }
            if !matches!(
                candidate.process.wait_one(signals::TERMINATED, 0),
                Err(native_abi::ERR_TIMED_OUT)
            ) || !matches!(
                self.current.process.wait_one(signals::TERMINATED, 0),
                Err(native_abi::ERR_TIMED_OUT)
            ) {
                return Err(native_abi::ERR_PEER_CLOSED);
            }
            decision.signal(0, signals::USER_0)
        }
        .await;
        if let Err(status) = result {
            let _ = decision.signal(0, signals::USER_1);
            let _ = native_runtime::task::kill(&candidate.job);
            let _ = executor
                .on_signals(&candidate.job, signals::TERMINATED)
                .await;
            return Err(status);
        }
        let old = core::mem::replace(&mut self.current, candidate);
        // 宿主 Job 与管理器 Job 同级，退休旧管理器不触碰设备进程。
        native_runtime::task::kill(&old.job)?;
        executor.on_signals(&old.job, signals::TERMINATED).await?;
        info!("driver-manager replaced; driver hosts kept running");
        Ok(())
    }
}

fn deadline() -> i64 {
    native_runtime::system::monotonic() + driver_update::START_TIMEOUT
}

async fn request(
    executor: &'static Executor,
    target: &Channel,
    address: u64,
    mut handles: Vec<Handle>,
    deadline: i64,
) -> Result<[u8; 8], Status> {
    let (reply, server) = Channel::create()?;
    handles.insert(0, server.into_handle());
    target.write(&address.to_le_bytes(), handles)?;
    executor
        .read_until(&reply, deadline)
        .await?
        .bytes
        .as_slice()
        .try_into()
        .map_err(|_| native_abi::ERR_INVALID_ARGS)
}

fn launch(
    root: &Job,
    image: &native_runtime::elf::Image,
    mut handles: Vec<Handle>,
) -> Result<Instance, Status> {
    let job = Job::create(root)?;
    let result = (|| {
        let (process, vmar) = Process::create(&job, b"driver-manager", 0)?;
        let runtime_vmar = vmar.duplicate(rights::SAME)?;
        let (control, server) = Channel::create()?;
        handles.insert(
            driver_manager::CONTROL - native_abi::startup::ROLE_HANDLES,
            server.into_handle(),
        );
        native_runtime::launch::launch(native_runtime::launch::Launch {
            process: &process,
            image_vmar: &vmar,
            runtime_vmar,
            name: b"driver-manager",
            image,
            arguments: &[],
            handles,
            stack_size: 256 * 1024,
        })?;
        Ok((process, control))
    })();
    match result {
        Ok((process, control)) => Ok(Instance {
            job,
            process,
            control,
        }),
        Err(status) => {
            let _ = native_runtime::task::kill(&job);
            Err(status)
        }
    }
}
