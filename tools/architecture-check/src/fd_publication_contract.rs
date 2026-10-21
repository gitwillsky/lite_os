//! 普通 open 与 SCM_RIGHTS 共享 fd owner；副作用前 admission、原表 rollback、无分配 publication。
use super::SourceFile;
fn source<'a>(sources: &'a [SourceFile], path: &str) -> &'a str {
    &sources
        .iter()
        .find(|s| s.relative == path)
        .expect("fd publication source")
        .text
}
fn before(body: &str, first: &str, second: &str) -> bool {
    matches!((body.find(first), body.find(second)), (Some(a), Some(b)) if a < b)
}
pub(super) fn check(sources: &[SourceFile], errors: &mut Vec<String>) {
    let open = source(sources, "kernel/src/syscall/fs/open.rs");
    let open = open.split("pub(crate) fn sys_openat").nth(1).unwrap_or("");
    if ![
        "base(task",
        "vfs().open_or_create_file_at",
        "inode.open_backend",
        "crate::fs::truncate",
    ]
    .iter()
    .all(|side_effect| before(open, "task.fd_prepare_open", side_effect))
        || !open.contains("reservation.publish(ofd)")
        || open.contains("task.fd_allocate(")
    {
        errors.push("fd-publication: open must reserve fd before every filesystem/device side effect and consume it on success".into());
    }
    let prepared = source(sources, "kernel/src/file/description_preparation.rs");
    let ofd = source(sources, "kernel/src/file/mod.rs");
    if !before(open, "PreparedFileDescription::try_new", "base(task")
        || !before(
            open,
            "ofd.notify(crate::fs::IN_OPEN)",
            "crate::fs::truncate",
        )
        || !prepared.contains("allocation: Arc<MaybeUninit<OpenFileDescription>>")
        || !before(
            prepared,
            ".write(OpenFileDescription",
            "self.allocation.assume_init()",
        )
        || ofd.contains("Arc::try_new(Self")
        || ofd.contains("pub(crate) fn inode(")
        || ofd.contains("pub(crate) fn device(")
    {
        errors.push("fd-publication: OFD must be allocated before side effects and initialized before open/truncate/close lifecycle".into());
    }
    if !before(open, "prepared.prepare_named", "inode.open_backend")
        || !prepared.contains("pub(crate) struct PreparedNamedFile")
        || !prepared.contains("InodeType::File | InodeType::BlockDevice")
        || !prepared.contains("self.write_open")
        || prepared.contains("pub(crate) fn device(")
        || prepared.contains("pub(crate) fn inode(")
    {
        errors.push("fd-publication: named storage/device backends must share pre-open writer admission and a single no-fail lifecycle commit".into());
    }
    let owner = source(sources, "kernel/src/task/model/file_table.rs");
    let reservation = owner
        .split("pub(crate) struct OpenFdReservation")
        .nth(1)
        .unwrap_or("")
        .split("impl core::fmt::Debug")
        .next()
        .unwrap();
    if !reservation.contains("owner: Arc<FileTableOwner>")
        || !reservation.contains("descriptors: [Option<usize>; N]")
        || !reservation.contains("files.publish_open(fd, file);")
        || !reservation.contains("self.owner.with_table(|files| files.cancel_reserved(fd))")
        || reservation.contains("self.task")
        || reservation.contains("file_table()")
    {
        errors.push("fd-publication: reservation publication and rollback must pin the original FileTable owner".into());
    }
    let table = source(sources, "kernel/src/file/descriptor_table.rs");
    let reserve = table
        .split("pub(crate) fn reserve_open")
        .nth(1)
        .unwrap_or("")
        .split("pub(crate) fn publish_open")
        .next()
        .unwrap();
    let publish = table
        .split("pub(crate) fn publish_open")
        .nth(1)
        .unwrap_or("")
        .split("pub(crate) fn cancel_reserved")
        .next()
        .unwrap();
    if !table.contains("ofd: Option<Arc<OpenFileDescription>>")
        || !reserve.contains("FileDescriptor::reserved(None, cloexec)")
        || !publish.contains("!entry.published && entry.ofd.is_none()")
        || !before(publish, "entry.ofd = Some(file)", "entry.published = true")
        || !before(
            publish,
            "descriptor_refs.fetch_add",
            "entry.published = true",
        )
        || ["try_reserve", "try_new", "insert_with"]
            .iter()
            .any(|allocation| publish.contains(allocation))
    {
        errors.push("fd-publication: reserved slot must have no fake OFD and publication must be allocation-free in the existing owner".into());
    }
    if !table.contains("try_clone_where(|entry| entry.published)")
        || !table.contains("take_if(fd, |entry| !entry.published)")
        || !table.contains("self.slots.get(new).is_some_and(|entry| !entry.published)")
        || !table.contains("return Err(FileDescriptorError::Busy)")
        || table.matches("filter(|entry| entry.published)").count() < 3
    {
        errors.push("fd-publication: unpublished reservations must remain invisible and exclusive through fork/proc/close/dup".into());
    }
    let control = source(sources, "kernel/src/syscall/socket/control.rs");
    let rights = control
        .split("fn write_rights(")
        .nth(1)
        .unwrap_or("")
        .split("fn write_control(")
        .next()
        .unwrap();
    let output = control
        .split("pub(super) fn write_receive(")
        .nth(1)
        .unwrap_or("");
    if !before(rights, "task.fd_prepare_open(cloexec)", ".copy_to_user(")
        || !before(rights, ".copy_to_user(", "reservation.publish(")
        || !before(rights, "reservation.publish(", "installed += 1")
        || !before(rights, "installed += 1", "let header =")
        || !rights.contains("installed < total")
        || !rights.contains("if header.is_ok()")
        || !rights.contains("pointer.wrapping_add(8)")
        || !before(
            rights,
            "pointer.wrapping_add(8)",
            "pointer.wrapping_add(12)",
        )
        || !before(
            rights,
            "pointer.wrapping_add(12)",
            "copy_to_user(pointer, &length",
        )
        || rights.contains("Vec::")
        || rights.contains("-errno::EFAULT")
        || control.contains("ReceivedFdTransaction")
        || control.contains("after_copyout")
        || !before(output, "write_rights(", "if name != 0")
        || !before(
            output,
            "message.wrapping_add(8), &(actual",
            "copy_to_user(name,",
        )
        || !before(output, "copy_to_user(name,", "message.wrapping_add(48)")
        || !before(
            output,
            "message.wrapping_add(48)",
            "message.wrapping_add(40)",
        )
    {
        errors.push("fd-publication: native SCM must copy and publish each FD before its header, retain the installed prefix on later faults, and output name/flags/length afterward".into());
    }
    let socket = source(sources, "kernel/src/syscall/socket.rs");
    let accept = socket
        .split("pub(in crate::syscall) fn accept_once")
        .nth(1)
        .unwrap_or("")
        .split("pub(crate) fn sys_accept4")
        .next()
        .unwrap();
    let socket_prepared = source(sources, "kernel/src/socket/preparation.rs");
    let socket_ofd = source(sources, "kernel/src/file/socket_preparation.rs");
    if !before(accept, "task.fd_prepare_open", "prepared.accept(")
        || !before(
            accept,
            "PreparedSocketDescription::try_new",
            "prepared.accept(",
        )
        || !before(
            accept,
            "write_address(task, peer",
            "reservation.publish(accepted)",
        )
        || !socket_prepared.contains("allocation: Arc<MaybeUninit<Socket>>")
        || !before(
            socket_prepared,
            ".write(Socket",
            "self.allocation.assume_init()",
        )
        || !before(
            socket_ofd,
            "crate::fs::socket_opened",
            "pub(crate) fn accept(",
        )
        || socket.contains("accept_with_notify")
        || socket.contains("OpenFileDescription::socket(")
    {
        errors.push("fd-publication: accept must prepare facade/OFD/inode/fd before consuming connection and copy address before publication".into());
    }
    let pipe = source(sources, "kernel/src/syscall/fs.rs");
    let pipe = pipe
        .split("pub(crate) fn sys_pipe2")
        .nth(1)
        .unwrap_or("")
        .split("/// OFD offset")
        .next()
        .unwrap();
    let pair = socket
        .split("pub(crate) fn sys_socketpair")
        .nth(1)
        .unwrap_or("")
        .split("pub(crate) fn sys_bind")
        .next()
        .unwrap();
    if !before(pair, "copy_to_user(output", "let create = ||")
        || !before(pair, "copy_to_user(output", "decode_domain(domain)")
    {
        errors.push("fd-publication: socketpair must reserve and copy fd numbers before protocol creation as native Linux".into());
    }
    for (name, body) in [("pipe2", pipe), ("socketpair", pair)] {
        if !body.contains("fd_prepare_opens::<2>")
            || !before(body, "copy_to_user", "reservation.publish_many")
            || body.contains("fd_allocate_pair")
            || body.contains("fd_close(")
        {
            errors.push(format!("fd-publication: {name} must copy out reserved numbers before atomic paired publication"));
        }
    }
}
#[cfg(test)]
#[path = "fd_publication_contract_tests.rs"]
mod tests;
