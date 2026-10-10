#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/vfs.h>
#include <sys/wait.h>
#include <unistd.h>

#ifndef SYS_pidfd_open
#define SYS_pidfd_open 434
#endif
#ifndef SYS_pidfd_send_signal
#define SYS_pidfd_send_signal 424
#endif
#ifndef SYS_pidfd_getfd
#define SYS_pidfd_getfd 438
#endif
#ifndef SYS_clone3
#define SYS_clone3 435
#endif
#ifndef CLONE_PIDFD
#define CLONE_PIDFD 0x1000
#endif
/* 与现有 raw-clone fixture 一样，仅用 musl 的汇编 trampoline 进入标准 clone ABI。 */
extern int __clone(int (*)(void *), void *, int, void *, int *, void *, int *);

#define PIDFD_THREAD 0x80
#define PIDFD_GET_USER_NAMESPACE 0xff09
#define PIDFD_GET_INFO _IOWR(0xff, 11, struct pidfd_information)

struct pidfd_information {
    uint64_t mask, cgroupid;
    uint32_t pid, tgid, ppid, ruid, rgid, euid, egid, suid, sgid, fsuid, fsgid;
    int32_t exit_code;
    uint32_t coredump_mask, coredump_signal, coredump_code, coredump_pad;
    uint64_t supported_mask;
};
struct pidfd_clone_arguments {
    uint64_t flags, pidfd, child_tid, parent_tid, exit_signal;
    uint64_t stack, stack_size, tls, set_tid, set_tid_size, cgroup;
};
static int pidfd_send(int fd, int signal, siginfo_t *information, unsigned int flags)
{
    return syscall(SYS_pidfd_send_signal, fd, signal, information, flags);
}
static int pidfd_ready(int fd, int timeout, int expected)
{
    struct pollfd item = { .fd = fd, .events = POLLIN | POLLOUT | POLLRDNORM };
    int result = poll(&item, 1, timeout);
    return result < 0 || (expected ? result != 1 : result != 0)
        || item.revents != expected;
}
static int pidfd_wait(int fd, unsigned int options, siginfo_t *information)
{
    return syscall(SYS_waitid, 3, fd, information, options, 0);
}
static int pidfd_information_file(const char *proc, int descriptor, long expected)
{
    char path[80], bytes[512], field[64];
    snprintf(path, sizeof path, "%s/self/fdinfo/%d", proc, descriptor);
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd < 0) return 1;
    ssize_t length = read(fd, bytes, sizeof bytes - 1);
    close(fd);
    if (length <= 0) return 1;
    bytes[length] = 0;
    snprintf(field, sizeof field, "Pid:\t%ld\nNSpid:\t%ld", expected, expected);
    return !strstr(bytes, "pos:\t0\nflags:\t") || !strstr(bytes, "\nmnt_id:\t")
        || !strstr(bytes, "\nino:\t") || !strstr(bytes, field);
}
struct pidfd_thread_probe { int ready, release; };
static void *pidfd_thread(void *opaque)
{
    struct pidfd_thread_probe *probe = opaque;
    pid_t tid = syscall(SYS_gettid);
    char byte;
    if (write(probe->ready, &tid, sizeof tid) != sizeof tid
        || read(probe->release, &byte, 1) != 1) return (void *)1;
    return NULL;
}
static int pidfd_clone_thread(void *opaque)
{
    return pidfd_thread(opaque) != NULL;
}

/* 直接标准 syscall/实际 pipe 屏障；不依赖 sleep 猜测 zombie、共享 OFD 或 thread retirement。 */
int verify_pidfds(const char *proc)
{
    int self = syscall(SYS_pidfd_open, getpid(), 0);
    if (self < 0 || fcntl(self, F_GETFD) != FD_CLOEXEC
        || (fcntl(self, F_GETFL) & O_ACCMODE) != O_RDWR
        || syscall(SYS_pidfd_open, 0, 0) != -1 || errno != EINVAL
        || syscall(SYS_pidfd_open, getpid(), 1) != -1 || errno != EINVAL
        || pidfd_send(-1, 0, NULL, 3) != -1 || errno != EINVAL
        || pidfd_send(self, 0, NULL, 0) || pidfd_send(-10000, 0, NULL, 0)
        || pidfd_send(-10001, 0, NULL, 0) || pidfd_ready(self, 0, 0)) return 1;
    struct stat first, second;
    struct statfs filesystem;
    int again = syscall(SYS_pidfd_open, getpid(), PIDFD_THREAD | O_NONBLOCK);
    if (again < 0 || fstat(self, &first) || fstat(again, &second) || first.st_ino != second.st_ino
        || first.st_dev != second.st_dev || (first.st_mode & S_IFMT) != 0
        || (first.st_mode & 0777) != 0700 || fstatfs(self, &filesystem)
        || filesystem.f_type != 0x50494446 || read(self, NULL, 0) != -1 || errno != EINVAL
        || write(self, NULL, 0) != -1 || errno != EINVAL) return 2;
    close(again);
    if (pidfd_information_file(proc, self, getpid())) return 25;
    char path[80];
    snprintf(path, sizeof path, "/proc/self/fd/%d", self);
    int pathfd = open(path, O_PATH | O_RDWR | O_TRUNC | O_CLOEXEC);
    if (pathfd < 0 || fcntl(pathfd, F_GETFL) != O_PATH || fstat(pathfd, &second)
        || first.st_ino != second.st_ino || read(pathfd, NULL, 0) != -1 || errno != EBADF
        || ioctl(pathfd, PIDFD_GET_USER_NAMESPACE, 0) != -1 || errno != EBADF
        || pidfd_send(pathfd, 0, NULL, 0) != -1 || errno != EBADF
        || fcntl(pathfd, F_SETFL, O_NONBLOCK) != -1 || errno != EBADF) return 26;
    int pathcopy = syscall(SYS_pidfd_getfd, self, pathfd, 0);
    if (pathcopy < 0 || fcntl(pathcopy, F_GETFL) != O_PATH || fstatat(pathcopy, "", &second, AT_EMPTY_PATH)
        || second.st_ino != first.st_ino || pidfd_send(pathcopy, 0, NULL, 0) != -1 || errno != EBADF) return 27;
    close(pathcopy); close(pathfd);
    struct pidfd_information detail = { .mask = 32 };
    if (ioctl(self, PIDFD_GET_INFO, &detail) || (detail.mask & 3) != 3
        || detail.pid != (uint32_t)getpid() || detail.tgid != (uint32_t)getpid()
        || detail.ppid != (uint32_t)getppid() || detail.ruid != getuid()
        || detail.euid != geteuid() || detail.fsuid != geteuid()
        || !(detail.mask & 32) || (detail.supported_mask & 3) != 3) return 3;
    int user = ioctl(self, PIDFD_GET_USER_NAMESPACE, 0);
    int original = open("/proc/self/ns/user", O_RDONLY);
    if (user < 0 || original < 0 || fstat(user, &first) || fstat(original, &second)
        || first.st_ino != second.st_ino || first.st_dev != second.st_dev
        || ioctl(self, PIDFD_GET_USER_NAMESPACE, 1) != -1 || errno != EINVAL) return 4;
    close(user); close(original);
    int process_directory = open("/proc/self", O_RDONLY | O_DIRECTORY);
    if (process_directory < 0 || pidfd_send(process_directory, 0, NULL, 0)
        || syscall(SYS_pidfd_getfd, process_directory, self, 0) != -1 || errno != EBADF) return 24;
    close(process_directory);
    process_directory = open("/proc/self", O_PATH | O_DIRECTORY);
    if (process_directory < 0 || pidfd_send(process_directory, 0, NULL, 0) != -1 || errno != EBADF) return 28;
    close(process_directory);
    int file = open("/tmp/pidfd-ofd", O_CREAT | O_TRUNC | O_RDWR | O_CLOEXEC, 0600);
    if (file < 0 || unlink("/tmp/pidfd-ofd") || write(file, "ABCD", 4) != 4 || lseek(file, 1, SEEK_SET) != 1) return 5;
    int received = syscall(SYS_pidfd_getfd, self, file, 0);
    char byte;
    if (received < 0 || fcntl(received, F_GETFD) != FD_CLOEXEC || read(received, &byte, 1) != 1
        || byte != 'B' || lseek(file, 0, SEEK_CUR) != 2
        || syscall(SYS_pidfd_getfd, self, -1, 0) != -1 || errno != EBADF
        || syscall(SYS_pidfd_getfd, -1, file, 1) != -1 || errno != EINVAL
        || syscall(SYS_pidfd_getfd, file, file, 0) != -1 || errno != EBADF) return 6;
    close(received); close(file);
    int release[2];
    if (pipe(release)) return 7;
    int descriptor = -77;
    if (syscall(SYS_clone, CLONE_PIDFD | SIGCHLD, 0, 1, 0, 0) != -1 || errno != EFAULT
        || syscall(SYS_clone, CLONE_PIDFD | CLONE_PARENT_SETTID | SIGCHLD, 0, &descriptor, 0, 0) != -1 || errno != EINVAL
        || waitpid(-1, NULL, WNOHANG) != -1 || errno != ECHILD) return 8;
    pid_t child = syscall(SYS_clone, CLONE_PIDFD | SIGCHLD, 0, &descriptor, 0, 0);
    if (!child) { if (descriptor != -77 || read(release[0], &byte, 1) != 1) _exit(99); _exit(43); }
    if (child < 0 || descriptor < 0 || fcntl(descriptor, F_GETFD) != FD_CLOEXEC
        || pidfd_ready(descriptor, 0, 0) || pidfd_send(descriptor, 0, NULL, 0)) return 9;
    int nonblocking = syscall(SYS_pidfd_open, child, O_NONBLOCK);
    siginfo_t information;
    memset(&information, 0xa5, sizeof information);
    if (nonblocking < 0 || pidfd_wait(nonblocking, WEXITED, &information) != -1 || errno != EAGAIN
        || information.si_signo || information.si_pid || information.si_status
        || pidfd_wait(nonblocking, WEXITED | WNOHANG, &information)) return 10;
    information = (siginfo_t){ .si_signo = SIGUSR1, .si_code = SI_USER, .si_pid = getpid(), .si_uid = getuid() };
    if (pidfd_send(descriptor, SIGUSR2, &information, 0) != -1 || errno != EINVAL
        || pidfd_send(descriptor, SIGUSR1, &information, 0) != -1 || errno != EPERM
        || write(release[1], "X", 1) != 1
        || pidfd_ready(descriptor, 2000, POLLIN | POLLRDNORM)) return 11;
    if (pidfd_wait(descriptor, WEXITED | WNOWAIT, &information) || information.si_pid != child
        || information.si_code != CLD_EXITED || information.si_status != 43
        || pidfd_ready(descriptor, 0, POLLIN | POLLRDNORM)
        || syscall(SYS_pidfd_getfd, descriptor, release[0], 0) != -1 || errno != ESRCH
        || pidfd_wait(descriptor, WEXITED, &information)
        || pidfd_ready(descriptor, 0, POLLIN | POLLRDNORM | POLLHUP)
        || pidfd_send(descriptor, 0, NULL, 0) != -1 || errno != ESRCH) return 12;
    detail = (struct pidfd_information){ .mask = 8 | 32 };
    if (ioctl(descriptor, PIDFD_GET_INFO, &detail) || !(detail.mask & 8)
        || detail.exit_code != (43 << 8) || detail.mask & 3
        || ioctl(descriptor, PIDFD_GET_USER_NAMESPACE, 1) != -1 || errno != ESRCH
        || pidfd_information_file(proc, descriptor, -1)) return 13;
    int32_t number = child;
    struct pidfd_clone_arguments args = { .exit_signal = SIGCHLD, .set_tid = (uintptr_t)&number, .set_tid_size = 1 };
    child = syscall(SYS_clone3, &args, sizeof args);
    if (!child) _exit(read(release[0], &byte, 1) == 1 ? 44 : 99);
    int replacement = syscall(SYS_pidfd_open, child, 0);
    if (child != number || replacement < 0 || pidfd_send(descriptor, 0, NULL, 0) != -1 || errno != ESRCH
        || pidfd_send(replacement, 0, NULL, 0) || write(release[1], "R", 1) != 1) return 14;
    int status;
    if (waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status) != 44) return 15;
    close(replacement); close(descriptor); close(nonblocking);
    int parent_tid = -1;
    args = (struct pidfd_clone_arguments){ .flags = CLONE_PIDFD | CLONE_PARENT_SETTID,
        .pidfd = (uintptr_t)&descriptor, .parent_tid = (uintptr_t)&descriptor, .exit_signal = SIGCHLD };
    if (syscall(SYS_clone3, &args, sizeof args) != -1 || errno != EINVAL) return 16;
    args.parent_tid = (uintptr_t)&parent_tid;
    child = syscall(SYS_clone3, &args, sizeof args);
    if (!child) _exit(45);
    if (child < 0 || parent_tid != child || pidfd_wait(descriptor, WEXITED | WNOWAIT, &information)
        || information.si_status != 45 || syscall(SYS_wait4, child, 1, 0, 0) != -1 || errno != EFAULT
        || waitpid(child, NULL, WNOHANG) != -1 || errno != ECHILD) return 17;
    close(descriptor);
    int ready[2]; pthread_t thread;
    if (pipe(ready)) return 18;
    struct pidfd_thread_probe probe = { .ready = ready[1], .release = release[0] };
    if (pthread_create(&thread, NULL, pidfd_thread, &probe) || read(ready[0], &child, sizeof child) != sizeof child
        || syscall(SYS_pidfd_open, child, 0) != -1 || errno != ENOENT) return 19;
    descriptor = syscall(SYS_pidfd_open, child, PIDFD_THREAD);
    detail = (struct pidfd_information){0};
    if (descriptor < 0 || !(fcntl(descriptor, F_GETFL) & PIDFD_THREAD)
        || ioctl(descriptor, PIDFD_GET_INFO, &detail) || detail.pid != (uint32_t)child || detail.tgid != (uint32_t)getpid()
        || pidfd_send(descriptor, 0, NULL, 2)
        || pidfd_send(descriptor, 0, NULL, 0) || pidfd_wait(descriptor, WEXITED | WNOHANG, &information) != -1 || errno != ECHILD
        || write(release[1], "T", 1) != 1 || pthread_join(thread, NULL)
        || pidfd_ready(descriptor, 2000, POLLIN | POLLRDNORM | POLLHUP)) return 20;
    close(descriptor);
    /* CLONE_THREAD 的 PIDFD 必须从实际 nonleader association 打开，不能误指 TGID。 */
    void *stack = mmap(NULL, 65536, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (stack == MAP_FAILED) return 21;
    /* musl 的公共 clone 包装器拒绝 CLONE_THREAD；callback 仅执行 pipe syscall，不注册 pthread。 */
    child = __clone(pidfd_clone_thread, (char *)stack + 65536,
        CLONE_VM | CLONE_FILES | CLONE_SIGHAND | CLONE_THREAD | CLONE_PIDFD | CLONE_SETTLS,
        &probe, &descriptor, __builtin_thread_pointer(), NULL);
    pid_t actual;
    if (child < 0 || descriptor < 0 || read(ready[0], &actual, sizeof actual) != sizeof actual || actual != child
        || pidfd_send(descriptor, 0, NULL, 0) || pidfd_send(descriptor, 0, NULL, 2)
        || write(release[1], "C", 1) != 1 || pidfd_ready(descriptor, 2000, POLLIN | POLLRDNORM | POLLHUP)) return 22;
    if (munmap(stack, 65536)) return 23;
    close(descriptor); close(self);
    close(ready[0]); close(ready[1]); close(release[0]); close(release[1]);
    return 0;
}
