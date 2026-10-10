#define _GNU_SOURCE
#include <errno.h>
#include <dirent.h>
#include <fcntl.h>
#include <grp.h>
#include <limits.h>
#include <linux/futex.h>
#include <netinet/in.h>
#include <net/if.h>
#include <net/route.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/msg.h>
#include <sys/sem.h>
#include <sys/shm.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <sys/timerfd.h>
#include <sys/mount.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/uio.h>
#include <sys/resource.h>
#include <sys/prctl.h>
#include <sys/sysinfo.h>
#include <poll.h>
#include <pthread.h>
#include <time.h>
#include <unistd.h>

#define NS_GET_USERNS 0xb701
#define NS_GET_PARENT 0xb702
#define NS_GET_NSTYPE 0xb703
#define NS_GET_OWNER_UID 0xb704
#ifndef CLONE_NEWTIME
#define CLONE_NEWTIME 0x80
#endif
#ifndef SYS_pidfd_open
#define SYS_pidfd_open 434
#endif
static int verify_time_namespaces(void);
static int verify_pid_namespaces(void);
static int verify_proc_mount_user_owner(void);
static int verify_live_task_statistics(void);
static int verify_kernel_proc_tasks(void);
int verify_pidfds(const char *proc);

static int map_write(pid_t pid, const char *name, const char *bytes)
{
    char path[96];
    snprintf(path, sizeof path, "/proc/%d/%s", pid, name);
    int fd = open(path, O_WRONLY | O_CLOEXEC);
    if (fd < 0) return -1;
    size_t length = strlen(bytes);
    ssize_t written = write(fd, bytes, length);
    int saved_errno = errno;
    close(fd);
    errno = saved_errno;
    return written == (ssize_t)length ? 0 : -1;
}
static int first_lower(int fd, uint32_t expected)
{
    char bytes[192];
    if (lseek(fd, 0, SEEK_SET) != 0) return -1;
    ssize_t count = read(fd, bytes, sizeof bytes - 1);
    if (count <= 0) return -1;
    bytes[count] = 0;
    unsigned int local, lower, length;
    if (sscanf(bytes, "%u %u %u", &local, &lower, &length) != 3) return -1;
    return local == 0 && lower == expected && length > 0 ? 0 : -1;
}
static int namespace_child(int ready, int proceed, int inherited_map, int parent_user, pid_t parent)
{
    if (unshare(CLONE_NEWUSER | CLONE_NEWUTS) < 0) return 10;
    if (getuid() != 65534 || getgid() != 65534) return 11;
    if (write(ready, "R", 1) != 1) return 12;
    char byte;
    if (read(proceed, &byte, 1) != 1) return 13;
    if (setresgid(0, 0, 0) < 0 || setresuid(0, 0, 0) < 0) return 14;
    if (getuid() != 0 || geteuid() != 0 || getgid() != 0 || getegid() != 0) return 15;
    if (setgroups(0, 0) != -1 || errno != EPERM) return 16;
    if (first_lower(inherited_map, 0) < 0) return 17;
    char path[96];
    snprintf(path, sizeof path, "/proc/%d/uid_map", parent);
    int fd = open(path, O_RDONLY);
    if (fd < 0 || first_lower(fd, UINT32_MAX) < 0) return 18;
    close(fd);
    fd = open("/proc/self/uid_map", O_RDONLY);
    if (fd < 0 || first_lower(fd, 1000) < 0) return 19;
    close(fd);
    int own_user = open("/proc/self/ns/user", O_RDONLY);
    int own_uts = open("/proc/self/ns/uts", O_RDONLY);
    if (own_user < 0 || own_uts < 0) return 20;
    if (ioctl(own_user, NS_GET_NSTYPE) != CLONE_NEWUSER || ioctl(own_uts, NS_GET_NSTYPE) != CLONE_NEWUTS) return 21;
    if (setns(own_user, CLONE_NEWUSER) != -1 || errno != EINVAL) return 22;
    if (setns(parent_user, CLONE_NEWUSER) != -1 || errno != EPERM) return 23;
    if (ioctl(own_user, NS_GET_PARENT) != -1 || errno != EPERM) return 24;
    if (lseek(own_user, 0, SEEK_SET) != -1 || errno != ESPIPE) return 25;
    int owner = ioctl(own_uts, NS_GET_USERNS);
    struct stat own_stat, owner_stat;
    if (owner < 0 || fstat(owner, &owner_stat) < 0 || fstat(own_user, &own_stat) < 0
        || owner_stat.st_ino != own_stat.st_ino || owner_stat.st_dev != own_stat.st_dev) return 26;
    close(owner);
    if (sethostname("ns-child", 8) < 0 || setdomainname("ns-domain", 9) < 0) return 27;
    struct utsname names;
    if (uname(&names) < 0 || strcmp(names.nodename, "ns-child") || strcmp(names.domainname, "ns-domain")) return 28;
    /* 全局特权资源必须仍授权 initial user namespace，不能借 child UID0 绕过。 */
    if (syscall(SYS_reboot, 0, 0, 0, 0) != -1 || errno != EPERM) return 29;
    if (syscall(SYS_swapon, 0, 0) != -1 || errno != EPERM) return 30;
    int quota = open("/proc/sys/user/max_user_namespaces", O_RDWR | O_TRUNC);
    struct stat quota_stat;
    if (quota < 0 || fstat(quota, &quota_stat) < 0 || (quota_stat.st_mode & 0777) != 0644) return 40;
    if (ftruncate(quota, 137) < 0 || fstat(quota, &quota_stat) < 0 || quota_stat.st_size != 0) return 48;
    if (write(quota, "0\n", 2) != 2) return 41;
    if (unshare(CLONE_NEWUSER) != -1 || errno != ENOSPC) return 42;
    /* sysctl_writes_strict=1: 非零位置成功消费，但不恢复配额。 */
    if (pwrite(quota, "1\n", 2, 2) != 2 || unshare(CLONE_NEWUSER) != -1 || errno != ENOSPC) return 43;
    if (pwrite(quota, "2147483647\n", 11, 0) != 11) return 44;
    close(quota);
    quota = open("/proc/sys/user/max_uts_namespaces", O_RDWR | O_TRUNC);
    if (quota < 0 || write(quota, "0\n", 2) != 2) return 45;
    if (unshare(CLONE_NEWUTS) != -1 || errno != ENOSPC) return 46;
    if (pwrite(quota, "2147483647\n", 11, 0) != 11) return 47;
    close(quota);
    if (verify_proc_mount_user_owner()) return 51;
    if (write(ready, "U", 1) != 1 || read(proceed, &byte, 1) != 1) return 31;
    close(own_user);
    close(own_uts);
    return 0;
}
int verify_user_namespaces(void)
{
    int kernel_proc = verify_kernel_proc_tasks();
    if (kernel_proc) {
        dprintf(2, "kernel proc failed step=%d errno=%d\n", kernel_proc, errno);
        return 190 + kernel_proc;
    }
    int statistics = verify_live_task_statistics();
    if (statistics) {
        dprintf(2, "task statistics failed step=%d errno=%d\n", statistics, errno);
        return 180 + statistics;
    }
    int ready[2], proceed[2];
    if (pipe(ready) < 0) return 1;
    if (pipe(proceed) < 0) { close(ready[0]); close(ready[1]); return 2; }
    int initial_map = open("/proc/self/uid_map", O_RDONLY);
    int initial_user = open("/proc/self/ns/user", O_RDONLY);
    int initial_uts = open("/proc/self/ns/uts", O_RDONLY);
    int result = 0, switched = 0, child_uts = -1, child_map = -1;
    pid_t child = -1;
    struct utsname initial_names, names;
    if (initial_map < 0 || initial_user < 0 || initial_uts < 0 || uname(&initial_names) < 0) { result = 3; goto cleanup; }
    if (ioctl(initial_user, NS_GET_PARENT) != -1 || errno != EPERM) { result = 4; goto cleanup; }
    child = fork();
    if (child < 0) { result = 5; goto cleanup; }
    if (child == 0) {
        close(ready[0]); close(proceed[1]);
        int step = namespace_child(ready[1], proceed[0], initial_map, initial_user, getppid());
        if (step) dprintf(2, "namespace child failed step=%d errno=%d\n", step, errno);
        _exit(step);
    }
    close(ready[1]); ready[1] = -1;
    close(proceed[0]); proceed[0] = -1;
    char byte;
    if (read(ready[0], &byte, 1) != 1 || byte != 'R') { result = 6; goto cleanup; }
    if (map_write(child, "uid_map", "0 1000 1\n") < 0
        || map_write(child, "setgroups", "deny\n") < 0
        || map_write(child, "gid_map", "0 1001 1\n") < 0) { result = 7; goto cleanup; }
    if (map_write(child, "uid_map", "0 1000 1\n") != -1 || errno != EPERM) { result = 8; goto cleanup; }
    if (write(proceed[1], "M", 1) != 1 || read(ready[0], &byte, 1) != 1 || byte != 'U') { result = 9; goto cleanup; }
    if (uname(&names) < 0 || strcmp(names.nodename, initial_names.nodename)
        || strcmp(names.domainname, initial_names.domainname)) { result = 32; goto cleanup; }
    char path[96];
    snprintf(path, sizeof path, "/proc/%d/ns/uts", child);
    child_uts = open(path, O_RDONLY);
    if (child_uts < 0 || setns(child_uts, CLONE_NEWUTS) < 0) { result = 33; goto cleanup; }
    switched = 1;
    if (uname(&names) < 0 || strcmp(names.nodename, "ns-child") || strcmp(names.domainname, "ns-domain")) { result = 34; goto cleanup; }
    if (setns(initial_uts, CLONE_NEWUTS) < 0) { result = 35; goto cleanup; }
    switched = 0;
    int owner = ioctl(child_uts, NS_GET_USERNS);
    int ancestor = owner < 0 ? -1 : ioctl(owner, NS_GET_PARENT);
    struct stat ancestor_stat, initial_stat;
    if (ancestor < 0 || fstat(ancestor, &ancestor_stat) < 0 || fstat(initial_user, &initial_stat) < 0
        || ancestor_stat.st_ino != initial_stat.st_ino) result = 36;
    if (owner >= 0) close(owner);
    if (ancestor >= 0) close(ancestor);
    if (result) goto cleanup;
    snprintf(path, sizeof path, "/proc/%d/uid_map", child);
    child_map = open(path, O_RDONLY);
    if (child_map < 0) { result = 49; goto cleanup; }
    if (write(proceed[1], "D", 1) != 1) { result = 37; goto cleanup; }
cleanup:
    if (switched && setns(initial_uts, CLONE_NEWUTS) < 0) result = 38;
    if (child > 0) {
        if (result) kill(child, SIGKILL);
        int status;
        pid_t waited;
        do { waited = waitpid(child, &status, 0); } while (waited < 0 && errno == EINTR);
        if (!result && (waited != child || !WIFEXITED(status) || WEXITSTATUS(status))) result = 39;
        if (!result && (fstat(child_map, &initial_stat) < 0 || initial_stat.st_uid != 0 || initial_stat.st_gid != 0
                        || first_lower(child_map, 1000) < 0)) result = 50;
    }
    if (initial_map >= 0) close(initial_map);
    if (initial_user >= 0) close(initial_user);
    if (initial_uts >= 0) close(initial_uts);
    if (child_uts >= 0) close(child_uts);
    if (child_map >= 0) close(child_map);
    for (int i = 0; i < 2; ++i) { if (ready[i] >= 0) close(ready[i]); if (proceed[i] >= 0) close(proceed[i]); }
    if (result) dprintf(2, "namespace parent failed step=%d errno=%d\n", result, errno);
    if (result) return result;
    result = verify_time_namespaces();
    return result ? result : verify_pid_namespaces();
}

int verify_time_namespace_image(void)
{
    struct timespec monotonic, boottime;
    return clock_gettime(CLOCK_MONOTONIC, &monotonic) < 0
        || clock_gettime(CLOCK_BOOTTIME, &boottime) < 0
        || monotonic.tv_sec < 3600 || boottime.tv_sec < 7200;
}
static int time_namespace_timers(clockid_t clock)
{
    int fd = timerfd_create(clock, TFD_CLOEXEC | TFD_NONBLOCK);
    if (fd < 0) return 1;
    struct timespec now;
    if (clock_gettime(clock, &now) < 0) return 2;
    struct itimerspec setting = { .it_value = now };
    setting.it_value.tv_nsec += 20000000;
    if (setting.it_value.tv_nsec >= 1000000000) { ++setting.it_value.tv_sec; setting.it_value.tv_nsec -= 1000000000; }
    if (timerfd_settime(fd, TFD_TIMER_ABSTIME, &setting, 0) < 0) return 3;
    struct pollfd pollfd = { .fd = fd, .events = POLLIN };
    uint64_t count;
    if (poll(&pollfd, 1, 1000) != 1 || read(fd, &count, 8) != 8 || count != 1) return 4;
    /* 非零 namespace timestamp 转换为 host 0：应立即到期，不能 disarm。 */
    setting.it_value.tv_sec = 0; setting.it_value.tv_nsec = 1;
    if (timerfd_settime(fd, TFD_TIMER_ABSTIME, &setting, 0) < 0
        || poll(&pollfd, 1, 1000) != 1 || read(fd, &count, 8) != 8) return 5;
    setting.it_value.tv_nsec = 20000000;
    if (timerfd_settime(fd, 0, &setting, 0) < 0
        || poll(&pollfd, 1, 1000) != 1 || read(fd, &count, 8) != 8) return 6;
    setting.it_value.tv_sec = INT64_MAX; setting.it_value.tv_nsec = 999999999;
    if (timerfd_settime(fd, TFD_TIMER_ABSTIME, &setting, 0) < 0
        || timerfd_gettime(fd, &setting) < 0 || setting.it_value.tv_sec < 1000000000) return 12;
    memset(&setting, 0, sizeof setting);
    if (timerfd_settime(fd, 0, &setting, 0) < 0) return 13;
    close(fd);
    struct sigevent notification = { .sigev_notify = SIGEV_NONE };
    timer_t timer;
    if (timer_create(clock, &notification, &timer) < 0 || clock_gettime(clock, &now) < 0) return 7;
    setting.it_value = now; setting.it_value.tv_sec += 5;
    if (timer_settime(timer, TIMER_ABSTIME, &setting, 0) < 0
        || timer_gettime(timer, &setting) < 0 || setting.it_value.tv_sec < 3 || setting.it_value.tv_sec > 5
        || timer_delete(timer) < 0) return 8;
    if (clock_gettime(clock, &now) < 0) return 9;
    now.tv_nsec += 2000000;
    if (now.tv_nsec >= 1000000000) { ++now.tv_sec; now.tv_nsec -= 1000000000; }
    if (clock_nanosleep(clock, TIMER_ABSTIME, &now, 0) != 0) return 10;
    struct timespec relative = { .tv_nsec = 2000000 };
    if (clock_nanosleep(clock, 0, &relative, 0) != 0) return 11;
    return 0;
}
static int time_namespace_child(void)
{
    if (verify_time_namespace_image()) return 20;
    struct timespec raw, coarse, boottime;
    if (clock_gettime(CLOCK_MONOTONIC_RAW, &raw) < 0 || raw.tv_sec < 3600
        || clock_gettime(CLOCK_MONOTONIC_COARSE, &coarse) < 0 || coarse.tv_sec < 3600
        || clock_gettime(CLOCK_BOOTTIME, &boottime) < 0) return 21;
    struct sysinfo info;
    if (sysinfo(&info) < 0 || info.uptime < boottime.tv_sec || info.uptime > boottime.tv_sec + 2) return 22;
    int uptime = open("/proc/uptime", O_RDONLY);
    char bytes[128];
    ssize_t length = uptime < 0 ? -1 : read(uptime, bytes, sizeof bytes - 1);
    if (length <= 0) return 23;
    bytes[length] = 0; close(uptime);
    unsigned long long seconds;
    if (sscanf(bytes, "%llu", &seconds) != 1 || seconds < 7200) return 24;
    int result = time_namespace_timers(CLOCK_MONOTONIC);
    if (result) return 30 + result;
    result = time_namespace_timers(CLOCK_BOOTTIME);
    return result ? 50 + result : 0;
}
static int time_namespace_wait(pid_t pid)
{
    int status;
    if (pid < 0 || waitpid(pid, &status, 0) != pid || !WIFEXITED(status)) return 1;
    return WEXITSTATUS(status);
}
static int time_namespace_parent(void)
{
    int initial = open("/proc/self/ns/time", O_RDONLY);
    /* 已打开 proc fd 必须跟随 target 的新 for_children，权限保留 opener f_cred。 */
    int offsets = open("/proc/self/timens_offsets", O_RDWR);
    struct stat initial_stat, active_stat, child_stat;
    struct timespec before, after, realtime;
    if (initial < 0 || offsets < 0 || fstat(initial, &initial_stat) < 0
        || clock_gettime(CLOCK_MONOTONIC, &before) < 0 || clock_gettime(CLOCK_REALTIME, &realtime) < 0) return 1;
    if (unshare(CLONE_NEWTIME) < 0) return 2;
    int active = open("/proc/self/ns/time", O_RDONLY);
    int pending = open("/proc/self/ns/time_for_children", O_RDONLY);
    if (active < 0 || pending < 0 || fstat(active, &active_stat) < 0 || fstat(pending, &child_stat) < 0
        || active_stat.st_ino != initial_stat.st_ino || child_stat.st_ino == initial_stat.st_ino
        || ioctl(pending, NS_GET_NSTYPE) != CLONE_NEWTIME) return 3;
    const char replacement[] = "monotonic 3600 0\nboottime 7200 0\n";
    if (write(offsets, replacement, sizeof replacement - 1) != (ssize_t)(sizeof replacement - 1)) return 4;
    if (clock_gettime(CLOCK_MONOTONIC, &after) < 0 || after.tv_sec - before.tv_sec >= 3600) return 5;
    if (write(offsets, "1 1 0", 5) != -1 || errno != EINVAL || lseek(offsets, 0, SEEK_SET) != 0) return 6;
    const char invalid_range[] = "1 1 0\n7 -9223372036854775808 0\n";
    if (write(offsets, invalid_range, sizeof invalid_range - 1) != -1 || errno != ERANGE) return 7;
    if (setns(pending, CLONE_NEWUTS) != -1 || errno != EINVAL) return 8;
    pid_t child = fork();
    if (child == 0) { int result = time_namespace_child(); if (result) dprintf(2, "time namespace child step=%d errno=%d\n", result, errno); _exit(result); }
    int result = time_namespace_wait(child);
    if (result) return result;
    if (write(offsets, "1 1 0", 5) != -1 || errno != EACCES) return 9;
    /* CLONE_VM 的 vfork child 仍使用旧 active time；exec 必须进入 pending time。 */
    child = vfork();
    if (child == 0) { execl("/bin/init", "init", "time-namespace-image", (char *)0); _exit(127); }
    if (time_namespace_wait(child)) return 10;
    if (setns(pending, 0) < 0 || verify_time_namespace_image()) return 11;
    if (setns(initial, CLONE_NEWTIME) < 0 || clock_gettime(CLOCK_MONOTONIC, &after) < 0) return 12;
    struct timespec realtime_after;
    if (clock_gettime(CLOCK_REALTIME, &realtime_after) < 0
        || after.tv_sec - before.tv_sec > realtime_after.tv_sec - realtime.tv_sec + 1) return 13;
    close(initial); close(offsets); close(active); close(pending);
    return 0;
}
static int verify_time_namespaces(void)
{
    pid_t child = fork();
    if (child == 0) { int result = time_namespace_parent(); if (result) dprintf(2, "time namespace parent step=%d errno=%d\n", result, errno); _exit(result); }
    return time_namespace_wait(child);
}

/* 真正的 syscall/owner 集成；同一 guest 最后执行，不另启动 VM。 */
static int pid_proc_read(const char *path, char *bytes, size_t capacity)
{
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd < 0) return -1;
    ssize_t count = read(fd, bytes, capacity - 1);
    int saved = errno;
    close(fd);
    errno = saved;
    if (count < 0) return -1;
    bytes[count] = 0;
    return (int)count;
}

/* boot worker 在真实 PID index/proc 可见；PF_KTHREAD 不能从退出用户 mm 的缺失推断。 */
static int verify_kernel_proc_tasks(void)
{
    DIR *directory = opendir("/proc");
    if (!directory) return 1;
    unsigned seen = 0;
    struct dirent *entry;
    while ((entry = readdir(directory))) {
        if (entry->d_name[0] < '0' || entry->d_name[0] > '9') continue;
        char path[512], text[4096];
        snprintf(path, sizeof path, "/proc/%s/comm", entry->d_name);
        if (pid_proc_read(path, text, sizeof text) < 0) { closedir(directory); return 2; }
        unsigned worker = !strcmp(text, "memory-reclaim\n") ? 1 : !strcmp(text, "unix-rights-gc\n") ? 2 : 0;
        if (!worker) continue;
        if (seen & worker) { closedir(directory); return 3; }
        seen |= worker;
        pid_t pid = (pid_t)strtol(entry->d_name, 0, 10);
        if (kill(pid, 0) || kill(pid, SIGUSR1) || kill(pid, SIGSTOP) || kill(pid, SIGKILL)
            || syscall(SYS_tkill, pid, 0) || syscall(SYS_tgkill, pid, pid, SIGUSR2)) {
            closedir(directory); return 13;
        }
        if (syscall(SYS_tgkill, getpid(), pid, 0) != -1 || errno != ESRCH) { closedir(directory); return 14; }
        int status;
        if (waitpid(pid, &status, WNOHANG) != -1 || errno != ECHILD) { closedir(directory); return 15; }
        pid_t unprivileged = fork();
        if (unprivileged < 0) { closedir(directory); return 16; }
        if (!unprivileged) {
            if (setresgid(1000, 1000, 1000) || setresuid(1000, 1000, 1000)) _exit(1);
            if (kill(pid, 0) != -1 || errno != EPERM) _exit(2);
            if (syscall(SYS_tkill, pid, SIGUSR1) != -1 || errno != EPERM) _exit(3);
            _exit(0);
        }
        if (waitpid(unprivileged, &status, 0) != unprivileged || !WIFEXITED(status) || WEXITSTATUS(status)) {
            closedir(directory); return 17;
        }
        snprintf(path, sizeof path, "/proc/%s/status", entry->d_name);
        if (pid_proc_read(path, text, sizeof text) <= 0 || !strstr(text, "Kthread:\t1\n")
            || !strstr(text, "Uid:\t0\t0\t0\t0\n") || !strstr(text, "FDSize:\t0\n") || strstr(text, "VmSize:")) {
            closedir(directory); return 4;
        }
        snprintf(path, sizeof path, "/proc/%s/statm", entry->d_name);
        if (pid_proc_read(path, text, sizeof text) < 0 || strcmp(text, "0 0 0 0 0 0 0\n")) { closedir(directory); return 5; }
        snprintf(path, sizeof path, "/proc/%s/cmdline", entry->d_name);
        if (pid_proc_read(path, text, sizeof text) != 0) { closedir(directory); return 6; }
        snprintf(path, sizeof path, "/proc/%s/stat", entry->d_name);
        if (pid_proc_read(path, text, sizeof text) <= 0) { closedir(directory); return 7; }
        char *field = strrchr(text, ')');
        if (!field || !(field = strchr(field + 2, ' '))) { closedir(directory); return 8; }
        unsigned long flags = 0;
        if (sscanf(field + 1, "%*d %*d %*d %*d %*d %lu", &flags) != 1 || !(flags & 0x00200000UL)) {
            closedir(directory); return 9;
        }
        snprintf(path, sizeof path, "/proc/%s/fd", entry->d_name);
        DIR *descriptors = opendir(path);
        if (!descriptors) { closedir(directory); return 10; }
        struct dirent *descriptor;
        int empty = 1;
        while ((descriptor = readdir(descriptors))) {
            if (strcmp(descriptor->d_name, ".") && strcmp(descriptor->d_name, "..")) empty = 0;
        }
        closedir(descriptors);
        if (!empty) { closedir(directory); return 11; }
    }
    closedir(directory);
    if (seen != 3) dprintf(2, "kernel proc missing workers seen=%u\n", seen);
    return seen == 3 ? 0 : 12;
}

/* total_forks 的 thread publication 与系统 live task count 使用实际不同事实。 */
struct task_count_rendezvous { int ready; int release; };
static void *task_count_worker(void *argument)
{
    struct task_count_rendezvous *gate = argument;
    pid_t tid = syscall(SYS_gettid);
    char byte;
    if (write(gate->ready, &tid, sizeof tid) != sizeof tid
        || read(gate->release, &byte, 1) != 1) return (void *)1;
    return NULL;
}
static int proc_forks(unsigned long long *count)
{
    char bytes[4096];
    if (pid_proc_read("/proc/stat", bytes, sizeof bytes) < 0) return -1;
    char *line = strstr(bytes, "\nprocesses ");
    return line && sscanf(line, "\nprocesses %llu", count) == 1 ? 0 : -1;
}
static int verify_live_task_statistics(void)
{
    int ready[2], release[2];
    if (pipe(ready) < 0 || pipe(release) < 0) return 1;
    struct sysinfo before, live, after;
    unsigned long long previous_forks, current_forks;
    if (sysinfo(&before) < 0 || proc_forks(&previous_forks) < 0) return 2;
    struct task_count_rendezvous gate = { ready[1], release[0] };
    pthread_t worker;
    if (pthread_create(&worker, NULL, task_count_worker, &gate)) return 3;
    pid_t tid;
    if (read(ready[0], &tid, sizeof tid) != sizeof tid || sysinfo(&live) < 0 || live.procs != before.procs + 1
        || proc_forks(&current_forks) < 0 || current_forks != previous_forks + 1) return 4;
    int pidfd = syscall(SYS_pidfd_open, tid, O_EXCL);
    if (pidfd < 0) return 7;
    if (write(release[1], "R", 1) != 1) return 5;
    void *result;
    /* clear_child_tid 唤醒 join 早于 PID association retirement；用实际 pidfd 等待该提交。 */
    struct pollfd completion = { .fd = pidfd, .events = POLLIN | POLLRDNORM };
    if (pthread_join(worker, &result) || result || poll(&completion, 1, 2000) != 1
        || (completion.revents & (POLLIN | POLLRDNORM | POLLHUP)) != (POLLIN | POLLRDNORM | POLLHUP)
        || sysinfo(&after) < 0 || after.procs != before.procs
        || proc_forks(&current_forks) < 0 || current_forks != previous_forks + 1) return 6;
    close(pidfd);
    close(ready[0]); close(ready[1]); close(release[0]); close(release[1]);
    return 0;
}

static int verify_proc_mount_user_owner(void)
{
    /* child UserNS 可创建自己的 MountNS，但不能因此挂载祖先 PIDNS 的 proc。 */
    if (unshare(CLONE_NEWNS) < 0 || mkdir("/tmp/user-proc", 0700) < 0) return 1;
    if (mount("proc", "/tmp/user-proc", "proc", 0, 0) != -1 || errno != EPERM) return 2;
    if (unshare(CLONE_NEWPID) < 0) return 3;
    pid_t child = fork();
    if (child == 0) {
        char status[4096];
        int result = getpid() != 1
            || mount("proc", "/tmp/user-proc", "proc", 0, 0) < 0
            || pid_proc_read("/tmp/user-proc/self/status", status, sizeof status) < 0
            || !strstr(status, "NSpid:\t1\n");
        _exit(result);
    }
    int status;
    return child < 0 || waitpid(child, &status, 0) != child
        || !WIFEXITED(status) || WEXITSTATUS(status);
}

#ifndef SYS_clone3
#define SYS_clone3 435
#endif
static int verify_clone3_namespace(void)
{
    struct clone_arguments {
        uint64_t flags, pidfd, child_tid, parent_tid, exit_signal;
        uint64_t stack, stack_size, tls, set_tid, set_tid_size, cgroup;
    };
    struct clone_arguments args = { .exit_signal = SIGCHLD };
    if (syscall(SYS_clone3, &args, 63) != -1 || errno != EINVAL
        || syscall(SYS_clone3, &args, 4097) != -1 || errno != E2BIG
        || syscall(SYS_clone3, 0, 64) != -1 || errno != EFAULT) return 1;
    args.flags = CLONE_PARENT;
    if (syscall(SYS_clone3, &args, sizeof args) != -1 || errno != EINVAL) return 6;
    args.flags = 0; args.exit_signal = 65;
    if (syscall(SYS_clone3, &args, sizeof args) != -1 || errno != EINVAL) return 7;
    args.exit_signal = SIGCHLD;
    unsigned char future[96] = {0};
    memcpy(future, &args, sizeof args);
    future[88] = 1;
    if (syscall(SYS_clone3, future, sizeof future) != -1 || errno != E2BIG) return 2;
    /* 独立 MM 的 child_tid 只写 child；checkpoint 编号只选择最内层，祖先正常分配。 */
    int32_t requested = 7, parent_tid = -1, child_tid = -123;
    args.flags = CLONE_PARENT_SETTID | CLONE_CHILD_SETTID | CLONE_CHILD_CLEARTID | CLONE_SETTLS;
    args.parent_tid = (uintptr_t)&parent_tid;
    args.child_tid = (uintptr_t)&child_tid;
    args.tls = (uintptr_t)__builtin_thread_pointer();
    args.set_tid = (uintptr_t)&requested;
    args.set_tid_size = 1;
    pid_t child = syscall(SYS_clone3, &args, sizeof args);
    if (child == 0) _exit(getpid() != 7 || syscall(SYS_gettid) != 7 || child_tid != 7
                         || syscall(SYS_set_tid_address, &child_tid) != 7);
    int status;
    if (child != 7 || parent_tid != 7 || child_tid != -123
        || waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status)
        || child_tid != -123) return 3;
    requested = 1;
    if (syscall(SYS_clone3, &args, sizeof args) != -1 || errno != EEXIST) return 4;
    requested = 0;
    if (syscall(SYS_clone3, &args, sizeof args) != -1 || errno != EINVAL) return 5;
    args = (struct clone_arguments){ .exit_signal = SIGCHLD };
    child = syscall(SYS_clone3, &args, 64);
    if (child == 0) _exit(0);
    return child <= 1 || waitpid(child, &status, 0) != child
        || !WIFEXITED(status) || WEXITSTATUS(status);
}

/* proc_fill_cache：目录枚举建立的 inode identity 必须由随后 lstat 看到。 */
static int proc_directory_identity(const char *path, const char *parent)
{
    DIR *directory = opendir(path);
    struct stat expected;
    if (!directory || stat(parent, &expected) < 0) return 1;
    int found_parent = 0;
    struct dirent *entry;
    while ((entry = readdir(directory))) {
        char child[256];
        struct stat actual;
        snprintf(child, sizeof child, "%s/%s", path, entry->d_name);
        if (lstat(child, &actual) < 0 || actual.st_ino != entry->d_ino) return 2;
        if (!strcmp(entry->d_name, "..")) {
            if (entry->d_ino != expected.st_ino) return 3;
            found_parent = 1;
        }
    }
    return closedir(directory) < 0 || !found_parent;
}

struct proc_thread_probe { int marker, ready, release; };
static void *proc_thread_probe(void *opaque)
{
    struct proc_thread_probe *probe = opaque;
    int32_t tid = unshare(CLONE_FILES) < 0 || close(probe->marker) < 0 ? -1 : syscall(SYS_gettid);
    if (write(probe->ready, &tid, sizeof tid) != sizeof tid) return (void *)2;
    if (tid < 0) return (void *)1;
    char byte;
    return (void *)(uintptr_t)(read(probe->release, &byte, 1) != 1);
}
static int verify_proc_thread_entries(void)
{
    int ready[2], release[2];
    struct proc_thread_probe probe;
    if (pipe(ready) < 0 || pipe(release) < 0
        || (probe.marker = open("/dev/null", O_RDONLY | O_CLOEXEC)) < 0) return 1;
    probe.ready = ready[1]; probe.release = release[0];
    pthread_t thread;
    if (pthread_create(&thread, NULL, proc_thread_probe, &probe)) return 2;
    int32_t tid;
    if (read(ready[0], &tid, sizeof tid) != sizeof tid || tid <= 1) return 3;
    char path[128], contents[4096];
    snprintf(path, sizeof path, "/tmp/pid-proc/%d/status", tid);
    if (pid_proc_read(path, contents, sizeof contents) < 0
        || !strstr(contents, "Tgid:\t1\n")) return 4;
    snprintf(path, sizeof path, "/tmp/pid-proc/1/task/%d/status", tid);
    if (pid_proc_read(path, contents, sizeof contents) < 0) return 5;
    snprintf(path, sizeof path, "/tmp/pid-proc/%d/fd/%d", tid, probe.marker);
    if (readlink(path, contents, sizeof contents) != -1 || errno != ENOENT) return 6;
    snprintf(path, sizeof path, "/tmp/pid-proc/1/fd/%d", probe.marker);
    if (readlink(path, contents, sizeof contents) <= 0) return 7;
    snprintf(path, sizeof path, "/tmp/pid-proc/%d/exe", tid);
    if (readlink(path, contents, sizeof contents) <= 0) return 8;
    snprintf(path, sizeof path, "/tmp/pid-proc/%d", tid);
    DIR *directory = opendir(path);
    struct stat root;
    if (!directory || stat("/tmp/pid-proc", &root) < 0) return 9;
    int found_parent = 0;
    struct dirent *entry;
    while ((entry = readdir(directory))) {
        if (!strcmp(entry->d_name, "..")) {
            if (entry->d_ino != root.st_ino) return 10;
            found_parent = 1;
        }
    }
    closedir(directory);
    if (!found_parent) return 11;
    char parent[128];
    snprintf(parent, sizeof parent, "/tmp/pid-proc/%d", tid);
    snprintf(path, sizeof path, "%s/fd", parent);
    if (proc_directory_identity(path, parent)) return 14;
    snprintf(path, sizeof path, "%s/ns", parent);
    if (proc_directory_identity(path, parent)) return 15;
    snprintf(parent, sizeof parent, "/tmp/pid-proc/1/task/%d", tid);
    snprintf(path, sizeof path, "%s/fd", parent);
    if (proc_directory_identity(path, parent)) return 16;
    snprintf(path, sizeof path, "%s/ns", parent);
    if (proc_directory_identity(path, parent)) return 17;
    if (proc_directory_identity("/tmp/pid-proc/1/task/1/ns", "/tmp/pid-proc/1/task/1")) return 18;
    snprintf(path, sizeof path, "/tmp/pid-proc/%d/cmdline", tid);
    if (pid_proc_read(path, contents, sizeof contents) <= 0) return 19;
    if (write(release[1], "R", 1) != 1) return 11;
    void *result;
    if (pthread_join(thread, &result) || result) return 12;
    snprintf(path, sizeof path, "/tmp/pid-proc/%d/status", tid);
    if (open(path, O_RDONLY) != -1 || errno != ENOENT) return 13;
    close(probe.marker);
    close(ready[0]); close(ready[1]); close(release[0]); close(release[1]);
    return 0;
}
#ifndef MSG_COPY
#define MSG_COPY 040000
#endif
static volatile sig_atomic_t ipc_signal_count;
static void ipc_signal_handler(int signal)
{
    (void)signal;
    ++ipc_signal_count;
}
static int ipc_wait_blocked(pid_t child)
{
    char path[96], text[256];
    snprintf(path, sizeof path, "/tmp/pid-proc/%d/stat", child);
    for (int attempt = 0; attempt < 512; ++attempt) {
        int fd = open(path, O_RDONLY | O_CLOEXEC);
        if (fd < 0) return -1;
        ssize_t count = read(fd, text, sizeof text - 1);
        close(fd);
        if (count <= 0) return -1;
        text[count] = 0;
        char *comm_end = strrchr(text, ')');
        if (comm_end && comm_end[1] == ' ' && comm_end[2] == 'S') return 0;
        if (comm_end && comm_end[2] == 'Z') return -1;
        struct timespec delay = { .tv_nsec = 2000000 };
        if (nanosleep(&delay, NULL)) return -1;
    }
    return -1;
}
/* 真实 sleep waiter，stop 后修改用户相对请求，验证不重导入或重计时。 */
static volatile sig_atomic_t sleep_signal_seen;
static void sleep_signal_handler(int signal_number)
{
    (void)signal_number;
    sleep_signal_seen++;
}
static int sleep_restart_case(int method, int caught_after_stop)
{
    struct timespec *request = mmap(NULL, 4096, PROT_READ | PROT_WRITE,
        MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (request == MAP_FAILED) return 1;
    *request = (struct timespec){ .tv_sec = 1 };
    int completion[2];
    if (pipe(completion)) { munmap(request, 4096); return 2; }
    pid_t child = fork();
    if (child == 0) {
        close(completion[0]);
        struct sigaction action = { .sa_handler = sleep_signal_handler, .sa_flags = SA_RESTART };
        if (sigaction(SIGUSR1, &action, NULL) || write(completion[1], "R", 1) != 1) _exit(1);
        int result = method == 0 ? nanosleep(request, NULL)
            : clock_nanosleep(CLOCK_MONOTONIC, 0, request, NULL);
        int failed = caught_after_stop
            ? (method == 0 ? result != -1 || errno != EINTR : result != EINTR)
            : result != 0;
        if (failed || sleep_signal_seen != caught_after_stop
            || syscall(SYS_restart_syscall) != -1 || errno != EINTR) _exit(2);
        if (write(completion[1], "D", 1) != 1) _exit(3);
        _exit(0);
    }
    close(completion[1]);
    char byte;
    int status, failed = child <= 0 || read(completion[0], &byte, 1) != 1 || byte != 'R'
        || ipc_wait_blocked(child) || kill(child, SIGSTOP)
        || waitpid(child, &status, WUNTRACED) != child || !WIFSTOPPED(status);
    if (!failed) {
        request->tv_sec = 1000;
        if (caught_after_stop) failed = kill(child, SIGUSR1);
        else {
            struct timespec delay = { .tv_sec = 1, .tv_nsec = 100000000 };
            failed = nanosleep(&delay, NULL);
        }
        if (!failed) failed = kill(child, SIGCONT);
    }
    struct pollfd ready = { .fd = completion[0], .events = POLLIN };
    if (!failed) failed = poll(&ready, 1, 2000) <= 0 || read(completion[0], &byte, 1) != 1 || byte != 'D';
    if (failed && child > 0) kill(child, SIGKILL);
    int waited = child > 0 && waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0;
    close(completion[0]);
    munmap(request, 4096);
    return failed || !waited;
}
static int verify_sleep_restart(void)
{
    for (int method = 0; method < 2; ++method)
        for (int caught = 0; caught < 2; ++caught)
            if (sleep_restart_case(method, caught)) return 1;
    return 0;
}

/* 0=receive EINTR，1=send EINTR，2=RMID，3=直接交付；先观察真实阻塞再触发，不依赖猜测延时。 */
static int ipc_waiter_case(int queue, int mode)
{
    int ready[2];
    if (pipe2(ready, O_CLOEXEC)) return -1;
    pid_t child = fork();
    if (child == 0) {
        close(ready[0]);
        struct sigaction action = { .sa_handler = ipc_signal_handler, .sa_flags = SA_RESTART };
        sigemptyset(&action.sa_mask);
        ipc_signal_count = 0;
        if (sigaction(SIGUSR1, &action, NULL) || write(ready[1], "R", 1) != 1) _exit(1);
        close(ready[1]);
        struct { long type; char text[16]; } message = { .type = 99 };
        ssize_t result = mode == 1 ? msgsnd(queue, &message, 0, 0)
            : msgrcv(queue, &message, sizeof message.text, 99, 0);
        int failed = mode == 3 ? result != 5 || message.type != 99 || memcmp(message.text, "event", 5)
            : result != -1 || errno != (mode == 2 ? EIDRM : EINTR)
                || ipc_signal_count != (mode == 2 ? 0 : 1);
        _exit(failed);
    }
    close(ready[1]);
    char byte;
    int failed = child < 0 || read(ready[0], &byte, 1) != 1 || ipc_wait_blocked(child);
    close(ready[0]);
    if (!failed) {
        if (mode == 2) failed = msgctl(queue, IPC_RMID, NULL) < 0;
        else if (mode == 3) {
            struct { long type; char text[5]; } message = { 99, { 'e', 'v', 'e', 'n', 't' } };
            struct msqid_ds status;
            failed = msgsnd(queue, &message, sizeof message.text, 0) < 0
                || msgctl(queue, IPC_STAT, &status) < 0 || status.msg_qnum || status.msg_cbytes;
        } else failed = kill(child, SIGUSR1) < 0;
    }
    if (failed && child > 0) kill(child, SIGKILL);
    if (child > 0 && time_namespace_wait(child)) failed = 1;
    return failed;
}
union ipc_semun { int val; struct semid_ds *buf; unsigned short *array; void *info; };
static int ipc_sem_waiter_case(int set, int mode)
{
    int ready[2];
    if (pipe2(ready, O_CLOEXEC)) return -1;
    pid_t child = fork();
    if (child == 0) {
        close(ready[0]);
        struct sigaction action = { .sa_handler = ipc_signal_handler, .sa_flags = SA_RESTART };
        sigemptyset(&action.sa_mask); ipc_signal_count = 0;
        if (sigaction(SIGUSR1, &action, NULL) || write(ready[1], "R", 1) != 1) _exit(1);
        close(ready[1]);
        struct sembuf operation = { .sem_num = 0, .sem_op = -1 };
        struct timespec timeout = { .tv_sec = 5 };
        int result = semtimedop(set, &operation, 1, &timeout);
        int failed = mode == 1 ? result != 0 : result != -1 || errno != (mode == 2 ? EIDRM : EINTR)
            || ipc_signal_count != (mode == 2 ? 0 : 1);
        _exit(failed);
    }
    close(ready[1]); char byte;
    int failed = child < 0 || read(ready[0], &byte, 1) != 1 || ipc_wait_blocked(child);
    close(ready[0]);
    if (!failed && semctl(set, 0, GETNCNT) != 1) failed = 1;
    if (!failed) {
        if (mode == 2) failed = semctl(set, 0, IPC_RMID) < 0;
        else if (mode == 1) failed = semctl(set, 0, SETVAL, (union ipc_semun){ .val = 1 }) < 0;
        else failed = kill(child, SIGUSR1) < 0;
    }
    if (failed && child > 0) kill(child, SIGKILL);
    if (child > 0 && time_namespace_wait(child)) failed = 1;
    return failed;
}
static int ipc_sem_shared_child(void *argument)
{
    struct sembuf operation = { .sem_num = 0, .sem_op = -1, .sem_flg = SEM_UNDO };
    return semop((int)(intptr_t)argument, &operation, 1) < 0;
}
static int ipc_sem_undo_case(int set, int reset)
{
    int ready[2], release[2];
    if (pipe2(ready, O_CLOEXEC) || pipe2(release, O_CLOEXEC)) return -1;
    if (semctl(set, 0, SETVAL, (union ipc_semun){ .val = 0 }) < 0) return -1;
    pid_t child = fork();
    if (child == 0) {
        close(ready[0]); close(release[1]);
        struct sembuf operation = { .sem_num = 0, .sem_op = 1, .sem_flg = SEM_UNDO };
        char byte;
        if (semop(set, &operation, 1) < 0 || write(ready[1], "R", 1) != 1 || read(release[0], &byte, 1) != 1) _exit(1);
        _exit(0);
    }
    close(ready[1]); close(release[0]); char byte;
    int failed = child < 0 || read(ready[0], &byte, 1) != 1 || semctl(set, 0, GETVAL) != 1;
    if (!failed && reset) failed = semctl(set, 0, SETVAL, (union ipc_semun){ .val = 7 }) < 0;
    if (write(release[1], "R", 1) != 1) failed = 1;
    close(ready[0]); close(release[1]);
    if (failed && child > 0) kill(child, SIGKILL);
    if (child > 0 && time_namespace_wait(child)) failed = 1;
    return failed || semctl(set, 0, GETVAL) != (reset ? 7 : 0);
}
static int ipc_shm_lock_child(void)
{
    struct rlimit limit;
    if (getrlimit(RLIMIT_MEMLOCK, &limit) < 0) return 1;
    limit.rlim_cur = 4096;
    if (setrlimit(RLIMIT_MEMLOCK, &limit) < 0 || setuid(49053) < 0) return 2;
    int first = shmget(IPC_PRIVATE, 4096, 0600), second = shmget(IPC_PRIVATE, 4096, 0600);
    if (first < 0 || second < 0 || shmctl(first, SHM_LOCK, NULL) < 0
        || shmctl(first, SHM_LOCK, NULL) < 0) return 3;
    if (shmctl(second, SHM_LOCK, NULL) != -1 || errno != ENOMEM) return 4;
    struct shmid_ds status;
    if (shmctl(first, IPC_STAT, &status) < 0 || !(status.shm_perm.mode & SHM_LOCKED)) return 5;
    limit.rlim_cur = 0;
    if (setrlimit(RLIMIT_MEMLOCK, &limit) < 0 || shmctl(first, SHM_LOCK, NULL) != -1 || errno != EPERM) return 6;
    if (shmctl(first, SHM_UNLOCK, NULL) < 0 || shmctl(first, IPC_STAT, &status) < 0
        || status.shm_perm.mode & SHM_LOCKED) return 7;
    limit.rlim_cur = 4096;
    if (setrlimit(RLIMIT_MEMLOCK, &limit) < 0 || shmctl(second, SHM_LOCK, NULL) < 0
        || shmctl(second, IPC_RMID, NULL) < 0 || shmctl(first, SHM_LOCK, NULL) < 0
        || shmctl(first, IPC_RMID, NULL) < 0) return 8;
    return 0;
}
static int ipc_shm_creator_exit(int forced, int flag)
{
    if (pwrite(flag, forced ? "1\n" : "0\n", 2, 0) != 2) return 1;
    int ready[2], resume[2];
    if (pipe(ready) < 0 || pipe(resume) < 0) return 2;
    pid_t child = fork();
    if (child < 0) return 3;
    if (!child) {
        close(ready[0]); close(resume[1]);
        int segment = shmget(IPC_PRIVATE, 4096, 0600);
        char token;
        if (segment < 0 || write(ready[1], &segment, sizeof segment) != sizeof segment
            || read(resume[0], &token, 1) != 1) _exit(1);
        _exit(0);
    }
    close(ready[1]); close(resume[0]);
    int segment;
    struct shmid_ds status;
    if (read(ready[0], &segment, sizeof segment) != sizeof segment
        || shmctl(segment, IPC_STAT, &status) < 0 || status.shm_nattch) return 4;
    if (write(resume[1], "x", 1) != 1) return 5;
    int child_status;
    if (waitpid(child, &child_status, 0) != child || !WIFEXITED(child_status) || WEXITSTATUS(child_status)) return 6;
    if (!forced && (shmctl(segment, IPC_STAT, &status) < 0 || pwrite(flag, "1\n", 2, 0) != 2)) return 7;
    if (shmctl(segment, IPC_STAT, &status) != -1 || errno != EINVAL) return 8;
    close(ready[0]); close(resume[1]);
    return pwrite(flag, "0\n", 2, 0) == 2 ? 0 : 9;
}
static int ipc_proc_has_id(int fd, int wanted)
{
    char bytes[8192];
    if (lseek(fd, 0, SEEK_SET) != 0) return -1;
    ssize_t count = read(fd, bytes, sizeof bytes - 1);
    if (count <= 0) return -1;
    bytes[count] = 0;
    char *line = strchr(bytes, '\n');
    while (line && *++line) {
        int key, id;
        if (sscanf(line, "%d %d", &key, &id) != 2) return -1;
        if (id == wanted) return 1;
        line = strchr(line, '\n');
    }
    return 0;
}
static int verify_ipc_shared_memory(int original)
{
    struct shmid_ds status;
    const size_t page = 4096;
    key_t key = 0x67451239;
    int segment = shmget(key, page * 3, IPC_CREAT | IPC_EXCL | 0600);
    if (segment < 0 || shmget(key, page * 3 + 1, 0) != -1 || errno != EINVAL) return 1;
    if (shmctl(segment, IPC_STAT, &status) < 0 || status.shm_segsz != page * 3
        || status.shm_nattch || status.shm_cpid != getpid()) return 2;
    unsigned char *mapping = shmat(segment, NULL, 0);
    if (mapping == (void *)-1 || mapping[0] || mapping[page] || mapping[page * 2]) return 3;
    mapping[0] = 71; mapping[page] = 72; mapping[page * 2] = 73;
    unsigned char *readonly = shmat(segment, NULL, SHM_RDONLY);
    if (readonly == (void *)-1 || readonly[0] != 71 || readonly[page * 2] != 73) return 4;
    if (mprotect(readonly, page, PROT_READ | PROT_WRITE) != -1 || errno != EACCES) return 5;
    if (shmctl(segment, IPC_STAT, &status) < 0 || status.shm_nattch != 2 || status.shm_lpid != getpid()) return 6;
    int ready[2], resume[2];
    if (pipe(ready) < 0 || pipe(resume) < 0) return 7;
    pid_t child = fork();
    if (child < 0) return 8;
    if (!child) {
        close(ready[0]); close(resume[1]);
        mapping[page] = 81;
        char token = 'r';
        if (write(ready[1], &token, 1) != 1 || read(resume[0], &token, 1) != 1) _exit(1);
        _exit(0);
    }
    close(ready[1]); close(resume[0]);
    char token;
    if (read(ready[0], &token, 1) != 1 || mapping[page] != 81
        || shmctl(segment, IPC_STAT, &status) < 0 || status.shm_nattch != 4) return 9;
    if (write(resume[1], "x", 1) != 1) return 10;
    int child_status;
    if (waitpid(child, &child_status, 0) != child || !WIFEXITED(child_status) || WEXITSTATUS(child_status)
        || shmctl(segment, IPC_STAT, &status) < 0 || status.shm_nattch != 2 || status.shm_lpid != child) return 11;
    close(ready[0]); close(resume[1]);
    if (munmap(mapping + page, page) < 0 || shmctl(segment, IPC_STAT, &status) < 0
        || status.shm_nattch != 3) return 12;
    /* shmdt 必须跳过已替换为普通匿名页的中间区间。 */
    void *replacement = mmap(mapping + page, page, PROT_READ | PROT_WRITE,
        MAP_FIXED | MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (replacement != mapping + page) return 13;
    memset(replacement, 0xa9, page);
    if (shmdt(mapping) < 0 || *(unsigned char *)replacement != 0xa9
        || shmctl(segment, IPC_STAT, &status) < 0 || status.shm_nattch != 1) return 14;
    if (munmap(replacement, page) < 0 || shmctl(segment, IPC_RMID, NULL) < 0
        || shmget(key, 0, 0) != -1 || errno != ENOENT) return 15;
    int next = shmget(key, page, IPC_CREAT | IPC_EXCL | 0600);
    unsigned char *late = shmat(segment, NULL, 0);
    if (next < 0 || next == segment || late == (void *)-1 || late[0] != 71) return 16;
    int description = open("/tmp/pid-proc/sysvipc/shm", O_RDONLY | O_CLOEXEC);
    if (description < 0 || ipc_proc_has_id(description, segment) != 1 || ipc_proc_has_id(description, next) != 1) return 25;
    if (unshare(CLONE_NEWIPC) < 0 || late[page] != 81 || shmdt(readonly) < 0) return 17;
    int empty = open("/tmp/pid-proc/sysvipc/shm", O_RDONLY | O_CLOEXEC);
    if (empty < 0 || ipc_proc_has_id(empty, segment) != 0 || ipc_proc_has_id(description, segment) != 1) return 26;
    close(empty);
    /* 挂接仍拥有旧 IPCNS，切换后 shmdt 不通过新 namespace 查找旧 ID。 */
    if (shmdt(late) < 0 || setns(original, CLONE_NEWIPC) < 0
        || shmctl(segment, IPC_STAT, &status) != -1 || errno != EINVAL) return 18;
    if (ipc_proc_has_id(description, segment) != 0 || ipc_proc_has_id(description, next) != 1) return 27;
    close(description);
    if (shmctl(next, IPC_RMID, NULL) < 0) return 19;
    int remapped = shmget(IPC_PRIVATE, page, 0600);
    void *target = mmap(NULL, page, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (remapped < 0 || target == MAP_FAILED || shmat(remapped, target, 0) != (void *)-1 || errno != EINVAL) return 20;
    if (shmat(remapped, target, SHM_REMAP) != target || shmdt(target) < 0 || shmctl(remapped, IPC_RMID, NULL) < 0) return 21;
    int forced = open("/tmp/pid-proc/sys/kernel/shm_rmid_forced", O_RDWR | O_CLOEXEC);
    if (forced < 0 || ipc_shm_creator_exit(0, forced) || ipc_shm_creator_exit(1, forced)) return 22;
    close(forced);
    child = fork();
    if (child < 0) return 23;
    if (!child) _exit(ipc_shm_lock_child());
    if (waitpid(child, &child_status, 0) != child || !WIFEXITED(child_status) || WEXITSTATUS(child_status)) return 24;
    return 0;
}

static int verify_ipc_semaphores(int original)
{
    if (semget(IPC_PRIVATE, 0, 0600) != -1 || errno != EINVAL) return 1;
    int set = semget(IPC_PRIVATE, 2, 0600);
    if (set < 0) return 2;
    int description = open("/tmp/pid-proc/sysvipc/sem", O_RDONLY | O_CLOEXEC);
    if (description < 0 || ipc_proc_has_id(description, set) != 1) return 31;
    close(description);
    unsigned short values[2] = { 1, 0 }, actual[2];
    if (semctl(set, 0, SETALL, (union ipc_semun){ .array = values }) < 0) return 3;
    struct sembuf rollback[2] = { {0, -1, 0}, {1, -1, IPC_NOWAIT} };
    if (semop(set, rollback, 2) != -1 || errno != EAGAIN
        || semctl(set, 0, GETALL, (union ipc_semun){ .array = actual }) < 0 || actual[0] != 1 || actual[1]) return 4;
    struct semid_ds status;
    if (semctl(set, 0, IPC_STAT, (union ipc_semun){ .buf = &status }) < 0
        || status.sem_nsems != 2 || status.sem_otime || status.sem_perm.uid != getuid()) return 5;
    struct sembuf duplicate[3] = { {0, 1, 0}, {0, -2, 0}, {1, 0, 0} };
    if (semop(set, duplicate, 3) < 0 || semctl(set, 0, GETVAL) != 0 || semctl(set, 0, GETPID) != getpid()) return 6;
    struct sembuf decrement = {0, -1, 0}; struct timespec zero = {0};
    if (semtimedop(set, &decrement, 1, &zero) != -1 || errno != EAGAIN || semctl(set, 0, GETNCNT)) return 7;
    struct timespec invalid = { .tv_nsec = 1000000000 };
    if (semtimedop(set, &decrement, 1, &invalid) != -1 || errno != EINVAL) return 8;
    struct sembuf outside = {2, 1, 0};
    if (semop(set, &outside, 1) != -1 || errno != EFBIG) return 9;
    if (ipc_sem_waiter_case(set, 0) || ipc_sem_waiter_case(set, 1)) return 10;
    if (ipc_sem_undo_case(set, 0) || ipc_sem_undo_case(set, 1)) return 11;
    if (semctl(set, 0, SETVAL, (union ipc_semun){ .val = 9 }) < 0) return 12;
    decrement.sem_op = -2; decrement.sem_flg = SEM_UNDO;
    if (semop(set, &decrement, 1) < 0 || semctl(set, 0, GETVAL) != 7) return 13;
    void *stack;
    if (posix_memalign(&stack, 16, 65536)) return 14;
    pid_t child = clone(ipc_sem_shared_child, (char *)stack + 65536, SIGCHLD | CLONE_SYSVSEM, (void *)(intptr_t)set);
    if (time_namespace_wait(child) || semctl(set, 0, GETVAL) != 6) return 15;
    free(stack);
    if (unshare(CLONE_SYSVSEM) < 0 || semctl(set, 0, GETVAL) != 9) return 16;
    if (semop(set, &decrement, 1) < 0 || unshare(CLONE_NEWIPC) < 0
        || semctl(set, 0, GETVAL) != -1 || errno != EINVAL) return 17;
    if (setns(original, CLONE_NEWIPC) < 0 || semctl(set, 0, GETVAL) != 9) return 18;
    int limits = open("/tmp/pid-proc/sys/kernel/sem", O_RDWR | O_CLOEXEC);
    static const char restricted[] = "2 3 1 1\n", defaults[] = "32000 1024000000 500 32000\n";
    if (limits < 0 || pwrite(limits, restricted, sizeof restricted - 1, 0) != sizeof restricted - 1) return 19;
    if (semget(IPC_PRIVATE, 1, 0600) != -1 || errno != ENOSPC
        || semop(set, rollback, 2) != -1 || errno != E2BIG) return 20;
    if (pwrite(limits, defaults, sizeof defaults - 1, 0) != sizeof defaults - 1) return 21;
    close(limits);
    if (semctl(set, 0, SETVAL, (union ipc_semun){ .val = 0 }) < 0 || ipc_sem_waiter_case(set, 2)) return 22;
    if (semctl(set, 0, GETVAL) != -1 || errno != EINVAL) return 23;
    return 0;
}
static int ipc_message_actor(int parent_key)
{
    if (unshare(CLONE_NEWIPC) < 0 || msgget(parent_key, 0) != -1 || errno != ENOENT) return 1;
    int original = open("/tmp/pid-proc/self/ns/ipc", O_RDONLY | O_CLOEXEC);
    int limit = open("/tmp/pid-proc/sys/kernel/msgmni", O_RDWR | O_CLOEXEC);
    if (original < 0 || limit < 0 || ioctl(original, NS_GET_NSTYPE) != CLONE_NEWIPC) return 2;
    int queue = msgget(IPC_PRIVATE, 0600);
    if (queue < 0) return 3;
    int description = open("/tmp/pid-proc/sysvipc/msg", O_RDONLY | O_CLOEXEC);
    if (description < 0 || ipc_proc_has_id(description, queue) != 1) return 28;
    close(description);
    struct { long type; char text[16]; } message = { 7, "seven" }, received;
    if (msgsnd(queue, &message, 5, 0) < 0) return 4;
    message.type = 2; memcpy(message.text, "two", 3);
    if (msgsnd(queue, &message, 3, 0) < 0) return 5;
    if (msgrcv(queue, &received, 1, 7, IPC_NOWAIT) != -1 || errno != E2BIG) return 6;
    if (msgrcv(queue, &received, sizeof received.text, 1, MSG_COPY | IPC_NOWAIT) != 3
        || received.type != 2 || memcmp(received.text, "two", 3)) return 20;
    if (msgrcv(queue, &received, 1, 0, MSG_COPY | MSG_NOERROR | IPC_NOWAIT) != -1 || errno != EINVAL) return 21;
    if (msgrcv(queue, (void *)1, 8, 0, MSG_COPY | IPC_NOWAIT) != -1 || errno != EFAULT) return 22;
    /* 普通 receive 不预读；COPY 对 buf 的读取必须先于无效 queue ID 返回 EINVAL。 */
    if (msgrcv(INT32_MAX, (void *)1, 8, 0, MSG_COPY | IPC_NOWAIT) != -1 || errno != EFAULT) return 23;
    if (msgrcv(queue, &received, sizeof received.text, -7, IPC_NOWAIT) != 3
        || received.type != 2 || memcmp(received.text, "two", 3)) return 7;
    if (msgrcv(queue, &received, 1, 7, MSG_NOERROR | IPC_NOWAIT) != 1
        || received.type != 7 || received.text[0] != 's') return 8;
    if (msgrcv(queue, &received, sizeof received.text, 0, IPC_NOWAIT) != -1 || errno != ENOMSG) return 9;
    struct msqid_ds status;
    if (msgctl(queue, IPC_STAT, &status) < 0 || status.msg_qnum || status.msg_cbytes
        || status.msg_lspid != getpid() || status.msg_lrpid != getpid()
        || status.msg_perm.uid != getuid() || status.msg_qbytes != 16384) return 10;
    if (ipc_waiter_case(queue, 0) || ipc_waiter_case(queue, 3)) return 24;
    status.msg_qbytes = 2;
    if (msgctl(queue, IPC_SET, &status) < 0) return 11;
    message.type = 1;
    if (msgsnd(queue, &message, 0, IPC_NOWAIT) < 0 || msgsnd(queue, &message, 0, IPC_NOWAIT) < 0
        || msgsnd(queue, &message, 0, IPC_NOWAIT) != -1 || errno != EAGAIN) return 12;
    if (ipc_waiter_case(queue, 1) || ipc_waiter_case(queue, 2)) return 25;
    if (msgctl(queue, IPC_STAT, &status) != -1 || errno != EINVAL) return 13;
    if (unshare(CLONE_NEWIPC) < 0) return 14;
    if (pwrite(limit, "0\n", 2, 0) != 2) return 15;
    if (msgget(IPC_PRIVATE, 0600) < 0) return 16;
    if (setns(original, CLONE_NEWIPC) < 0 || msgget(IPC_PRIVATE, 0600) != -1 || errno != ENOSPC) return 17;
    if (pwrite(limit, "32000\n", 6, 0) != 6) return 18;
    queue = msgget(parent_key, IPC_CREAT | IPC_EXCL | 0600);
    if (queue < 0 || msgget(parent_key, IPC_CREAT | IPC_EXCL | 0600) != -1 || errno != EEXIST) return 19;
    int semaphore_result = verify_ipc_semaphores(original);
    if (semaphore_result) { dprintf(2, "IPC semaphore step=%d errno=%d\n", semaphore_result, errno); return 26; }
    int shared_memory_result = verify_ipc_shared_memory(original);
    if (shared_memory_result) { dprintf(2, "IPC shared memory step=%d errno=%d\n", shared_memory_result, errno); return 27; }
    close(original); close(limit);
    return msgctl(queue, IPC_RMID, NULL) < 0;
}
static int verify_ipc_messages(void)
{
    int key = 0x4c495445 ^ getpid();
    int queue = msgget(key, IPC_CREAT | IPC_EXCL | 0600);
    if (queue < 0) return 1;
    pid_t child = fork();
    if (child == 0) {
        int result = ipc_message_actor(key);
        if (result) dprintf(2, "IPC message step=%d errno=%d\n", result, errno);
        _exit(result);
    }
    int failed = time_namespace_wait(child);
    if (msgctl(queue, IPC_RMID, NULL) < 0) failed = 1;
    return failed;
}

static int tty_control_actor(void)
{
    if (setsid() != getpid()) return 1;
    int master = posix_openpt(O_RDWR | O_NOCTTY | O_CLOEXEC);
    if (master < 0 || grantpt(master) < 0 || unlockpt(master) < 0) return 2;
    char name[64];
    if (ptsname_r(master, name, sizeof name)) return 3;
    int slave = open(name, O_RDWR | O_NOCTTY | O_CLOEXEC);
    if (slave < 0) return 4;
    int sid = -1, group = -1;
    int current = open("/dev/tty", O_RDWR | O_CLOEXEC);
    if (current >= 0 || errno != ENXIO || ioctl(master, TIOCGSID, &sid) != -1 || errno != ENOTTY
        || ioctl(slave, TIOCSCTTY, 0) < 0 || ioctl(master, TIOCGSID, &sid) < 0 || sid != getpid()
        || ioctl(master, TIOCGPGRP, &group) < 0 || group != getpid()
        || ioctl(master, TIOCNOTTY, 0) != -1 || errno != ENOTTY
        || tcsetpgrp(slave, 0) != -1 || errno != ESRCH) return 4;
    if (read(slave, NULL, 0) != 0 || write(master, "z\n", 2) != 2
        || read(slave, NULL, 0) != 0) return 27;
    char zero_input[2];
    if (read(slave, zero_input, 2) != 2 || memcmp(zero_input, "z\n", 2)) return 28;
    int ready[2], release[2];
    if (pipe(ready) < 0 || pipe(release) < 0) return 5;
    pid_t child = fork();
    if (child == 0) {
        int step = 0;
        if (setsid() != getpid()
            || ioctl(slave, TIOCSCTTY, 0) != -1 || errno != EPERM
            || ioctl(slave, TIOCSCTTY, 2) != -1 || errno != EPERM) step = 1;
        struct { uint32_t version; int32_t pid; } header = { UINT32_C(0x20080522), 0 };
        struct { uint32_t effective, permitted, inheritable; } caps[2], saved[2];
        if (!step && syscall(SYS_capget, &header, caps) < 0) step = 2;
        if (!step) {
            memcpy(saved, caps, sizeof caps);
            caps[0].effective &= ~(UINT32_C(1) << 21);
            if (syscall(SYS_capset, &header, caps) < 0
                || ioctl(slave, TIOCSCTTY, 1) != -1 || errno != EPERM
                || syscall(SYS_capset, &header, saved) < 0) step = 3;
        }
        if (!step && (ioctl(slave, TIOCSCTTY, 1) < 0 || tcgetpgrp(slave) != getpid())) step = 4;
        char result = step ? (char)step : 'R';
        if (write(ready[1], &result, 1) != 1) _exit(5);
        if (step) _exit(step);
        if (read(release[0], &result, 1) != 1) _exit(6);
        _exit(0);
    }
    char byte;
    int status;
    if (child < 0 || read(ready[0], &byte, 1) != 1 || byte != 'R') return 6;
    current = open("/dev/tty", O_RDWR | O_CLOEXEC);
    if (current >= 0 || errno != ENXIO
        || tcgetpgrp(slave) != -1 || errno != ENOTTY) return 7;
    if (write(release[1], "R", 1) != 1 || waitpid(child, &status, 0) != child
        || !WIFEXITED(status) || WEXITSTATUS(status)) return 8;
    if (ioctl(slave, TIOCSCTTY, 0) < 0 || tcgetpgrp(slave) != getpid()) return 9;
    signal(SIGHUP, SIG_IGN);
    child = fork();
    if (child == 0) {
        if (ioctl(slave, TIOCNOTTY, 0) < 0 || open("/dev/tty", O_RDWR) != -1 || errno != ENXIO
            || close(master) < 0 || write(ready[1], "R", 1) != 1
            || read(release[0], &byte, 1) != 1) _exit(1);
        _exit(0);
    }
    if (child < 0 || setpgid(child, child) < 0 || tcsetpgrp(slave, child) < 0
        || ioctl(slave, TIOCSCTTY, UINT64_C(0x100000002)) < 0 || tcgetpgrp(slave) != child) return 10;
    if (read(ready[0], &byte, 1) != 1 || byte != 'R' || close(master) < 0) return 11;
    current = open("/dev/tty", O_RDWR | O_CLOEXEC);
    if (current >= 0 || errno != ENXIO) return 12;
    if (write(release[1], "R", 1) != 1 || waitpid(child, &status, 0) != child
        || !WIFEXITED(status) || WEXITSTATUS(status)) return 13;
    close(slave);
    master = posix_openpt(O_RDWR | O_NOCTTY | O_CLOEXEC);
    if (master < 0 || grantpt(master) < 0 || unlockpt(master) < 0
        || ptsname_r(master, name, sizeof name)) return 14;
    slave = open(name, O_RDWR | O_CLOEXEC);
    if (slave < 0 || tcgetpgrp(slave) != getpid()
        || ioctl(slave, TIOCGSID, &sid) < 0 || sid != getpid()
        || ioctl(slave, TIOCNOTTY, 0) < 0) return 15;
    current = open("/dev/tty", O_RDWR | O_CLOEXEC);
    if (current >= 0 || errno != ENXIO || ioctl(slave, TIOCGSID, &sid) != -1 || errno != ENOTTY
        || ioctl(master, TIOCGPGRP, &group) < 0 || group != 0) return 16;
    close(master); close(slave);
    close(ready[0]); close(ready[1]); close(release[0]); close(release[1]);
    return 0;
}
static int verify_tty_control(void)
{
    pid_t child = fork();
    if (child == 0) {
        int result = tty_control_actor();
        if (result) dprintf(2, "TTY control step=%d errno=%d\n", result, errno);
        _exit(result);
    }
    return time_namespace_wait(child);
}

/* SCM_RIGHTS 使用真实 OFD；普通 read 必须消费并关闭 ancillary capability。 */
static ssize_t unix_send_fds(int socket, const int *files, size_t count)
{
    union { struct cmsghdr align; unsigned char bytes[CMSG_SPACE(253 * sizeof(int))]; } storage;
    memset(&storage, 0, sizeof storage);
    char byte = 'r';
    struct iovec iov = { .iov_base = &byte, .iov_len = 1 };
    struct msghdr message = { .msg_iov = &iov, .msg_iovlen = 1,
        .msg_control = storage.bytes, .msg_controllen = CMSG_SPACE(count * sizeof(int)) };
    struct cmsghdr *control = CMSG_FIRSTHDR(&message);
    control->cmsg_level = SOL_SOCKET;
    control->cmsg_type = SCM_RIGHTS;
    control->cmsg_len = CMSG_LEN(count * sizeof(int));
    memcpy(CMSG_DATA(control), files, count * sizeof(int));
    return sendmsg(socket, &message, MSG_DONTWAIT);
}
static int unix_rights_roundtrip(int type)
{
    int pair[2];
    if (socketpair(AF_UNIX, type | SOCK_CLOEXEC, 0, pair) < 0) return 1;
    int sent[3] = { pair[0], pair[0], pair[1] };
    if (unix_send_fds(pair[0], sent, 3) != 1) return 2;
    union { struct cmsghdr align; unsigned char bytes[CMSG_SPACE(3 * sizeof(int))]; } storage;
    memset(&storage, 0, sizeof storage);
    char byte = 0;
    struct iovec iov = { .iov_base = &byte, .iov_len = 1 };
    struct msghdr message = { .msg_iov = &iov, .msg_iovlen = 1,
        .msg_control = storage.bytes, .msg_controllen = sizeof storage.bytes };
    for (int round=0; round<3; round++) {
        message.msg_controllen = sizeof storage.bytes;
        message.msg_flags = 0;
        if (recvmsg(pair[1], &message, MSG_CMSG_CLOEXEC | MSG_DONTWAIT | (round < 2 ? MSG_PEEK : 0)) != 1 || byte != 'r'
            || message.msg_flags & MSG_CTRUNC) return 3;
        struct cmsghdr *control = CMSG_FIRSTHDR(&message);
        if (!control || control->cmsg_level != SOL_SOCKET || control->cmsg_type != SCM_RIGHTS
            || control->cmsg_len != CMSG_LEN(3 * sizeof(int))) return 4;
        int received[3]; memcpy(received, CMSG_DATA(control), sizeof received);
        for (size_t i = 0; i < 3; ++i) {
            struct stat original, passed;
            if (received[i] < 0 || !(fcntl(received[i], F_GETFD) & FD_CLOEXEC)
                || fstat(sent[i], &original) < 0 || fstat(received[i], &passed) < 0
                || original.st_dev != passed.st_dev || original.st_ino != passed.st_ino) return 5;
            close(received[i]);
        }
        struct pollfd ready = { .fd = pair[1], .events = POLLIN };
        if (poll(&ready,1,0) != (round<2 ? 1 : 0)) return 6;
    }
    close(pair[0]); close(pair[1]);
    return 0;
}
/* 固定 Linux scm_detach_fds：已复制的 FD 逐个发布，后续 fault 不回滚 prefix。 */
static int unix_rights_copyout_actor(int type)
{
    /* musl 会复制并写回 msghdr；故障顺序必须直接验证原生 syscall 的用户指针。 */
    long page = sysconf(_SC_PAGESIZE);
    int pair[2], original = open("/", O_PATH | O_CLOEXEC);
    if (page < 4096 || original < 0 || socketpair(AF_UNIX, type | SOCK_CLOEXEC, 0, pair)) return 1;
    unsigned char *pages = mmap(NULL, page * 2, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (pages == MAP_FAILED) return 2;
    char byte;
    struct iovec vector = { &byte, 1 };
    _Alignas(struct cmsghdr) unsigned char control[CMSG_SPACE(2 * sizeof(int))];
    for (int stage = 0; stage < 6; ++stage) {
        if (mprotect(pages, page * 2, PROT_READ | PROT_WRITE)) return 3;
        memset(pages, 0, page * 2); memset(control, 0, sizeof control);
        int passed[2] = { original, original };
        size_t count = stage == 0 ? 2 : 1;
        if (unix_send_fds(pair[0], passed, count) != 1) return 4;
        struct msghdr message = { .msg_iov = &vector, .msg_iovlen = 1,
            .msg_control = control, .msg_controllen = sizeof control };
        struct msghdr *target = &message;
        unsigned char *received = control + CMSG_LEN(0);
        if (stage == 0) {
            /* 第一 FD 在页尾，第二 FD fault：仅保留第一项并报告 CTRUNC。 */
            message.msg_control = pages + page - CMSG_LEN(sizeof(int));
            received = pages + page - sizeof(int);
            if (mprotect(pages + page, page, PROT_NONE)) return 5;
        } else if (stage == 1 || stage == 5) {
            /* header 只读但 FD 数组可写：header fault 保留 FD，cursor 不推进。 */
            message.msg_control = pages + page - CMSG_LEN(0);
            received = pages + page;
            if (mprotect(pages, page, PROT_READ)) return 6;
            if (stage == 5) {
                message.msg_controllen = CMSG_SPACE(sizeof(struct ucred));
                int enabled = 1;
                if (setsockopt(pair[1], SOL_SOCKET, SO_PASSCRED, &enabled, sizeof enabled)) return 7;
            }
        } else if (stage == 2) {
            /* namelen copyout 先于 name bytes，即使 unnamed source 长度为零仍须写回。 */
            message.msg_name = pages + page;
            message.msg_namelen = sizeof(struct sockaddr_un);
            target = (void *)pages;
            memcpy(target, &message, sizeof message);
            if (mprotect(pages, page, PROT_READ)) return 8;
        } else if (stage == 3) {
            target = (void *)pages;
            memcpy(target, &message, sizeof message);
            if (mprotect(pages, page, PROT_READ)) return 9;
        } else {
            /* flags 在可写第二页，controllen 在只读第一页，验证 flags→length 顺序。 */
            target = (void *)(pages + page - 48);
            memcpy(target, &message, sizeof message);
            if (mprotect(pages, page, PROT_READ)) return 10;
        }
        errno = 0;
        ssize_t result = syscall(SYS_recvmsg, pair[1], target, MSG_DONTWAIT | MSG_CMSG_CLOEXEC);
        if ((stage >= 2 && stage <= 4) ? (result != -1 || errno != EFAULT) : result != 1) return 11;
        int fd; memcpy(&fd, received, sizeof fd);
        struct stat sent, got;
        if (fd < 0 || fcntl(fd, F_GETFD) != FD_CLOEXEC
            || fstat(original, &sent) || fstat(fd, &got)
            || sent.st_dev != got.st_dev || sent.st_ino != got.st_ino) return 12;
        if (stage == 0 && (!(message.msg_flags & MSG_CTRUNC) || message.msg_controllen != sizeof control)) return 13;
        if ((stage == 1 || stage == 5) && (message.msg_controllen || (message.msg_flags & MSG_CTRUNC))) return 14;
        if (stage == 4 && !(target->msg_flags & MSG_CMSG_CLOEXEC)) return 15;
        close(fd);
        struct pollfd empty = { .fd = pair[1], .events = POLLIN };
        if (poll(&empty, 1, 0) != 0) return 16;
        /* scalar fault 的未发布尾部必须立即归还最低 FD slot。 */
        if (stage == 0) {
            int next = fcntl(original, F_DUPFD_CLOEXEC, fd);
            if (next != fd) return 17;
            close(next);
        }
    }
    munmap(pages, page * 2); close(original); close(pair[0]); close(pair[1]);
    return 0;
}

static int unix_rights_quota_actor(void)
{
    int pair[2];
    struct rlimit limit;
    if (socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, pair) < 0
        || getrlimit(RLIMIT_NOFILE, &limit) < 0) return 1;
    limit.rlim_cur = 4;
    if (setrlimit(RLIMIT_NOFILE, &limit) < 0 || setresuid(10001, 10001, 10001) < 0) return 2;
    int sent[253];
    for (size_t i = 0; i < 253; ++i) sent[i] = pair[0];
    /* 标准检查发生在 batch 之前：零 inflight 可一次超过 soft limit。 */
    if (unix_send_fds(pair[0], sent, 253) != 1) return 3;
    errno = 0;
    if (unix_send_fds(pair[0], sent, 1) != -1 || errno != ETOOMANYREFS) return 4;
    char byte;
    if (read(pair[1], &byte, 1) != 1 || byte != 'r') return 5;
    if (unix_send_fds(pair[0], sent, 1) != 1 || read(pair[1], &byte, 1) != 1) return 6;
    close(pair[0]); close(pair[1]);
    return 0;
}
static int unix_rights_privileged_actor(void)
{
    int pair[2]; struct rlimit limit;
    if (socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, pair) < 0
        || getrlimit(RLIMIT_NOFILE, &limit) < 0) return 1;
    limit.rlim_cur = 0;
    if (setrlimit(RLIMIT_NOFILE, &limit) < 0) return 2;
    int sent = pair[0]; char byte;
    if (unix_send_fds(pair[0], &sent, 1) != 1 || unix_send_fds(pair[0], &sent, 1) != 1
        || read(pair[1], &byte, 1) != 1 || read(pair[1], &byte, 1) != 1) return 3;
    close(pair[0]); close(pair[1]);
    return 0;
}
static long unix_rights_heap_kib(void)
{
    char bytes[4096];
    if (pid_proc_read("/tmp/pid-proc/meminfo", bytes, sizeof bytes) < 0) return -1;
    char *slab = strstr(bytes, "Slab:");
    if (!slab) return -1;
    char *end;
    unsigned long value = strtoul(slab + 5, &end, 10);
    return end == slab + 5 || value > LONG_MAX ? -1 : (long)value;
}
static int unix_accept_preparation(void)
{
    int listener = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    int client = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    struct sockaddr_un address = { .sun_family = AF_UNIX };
    memcpy(address.sun_path + 1, "accept-preparation", 18);
    socklen_t address_length = sizeof address.sun_family + 19;
    if (listener < 0 || client < 0 || bind(listener, (void *)&address, address_length) < 0
        || listen(listener, 4) < 0 || connect(client, (void *)&address, address_length) < 0) return 1;
    struct rlimit original, limited;
    if (getrlimit(RLIMIT_NOFILE, &original) < 0) return 2;
    limited = original; limited.rlim_cur = 0;
    if (setrlimit(RLIMIT_NOFILE, &limited) < 0) return 3;
    int failed = accept4(listener, NULL, NULL, SOCK_CLOEXEC);
    int failure = errno;
    if (setrlimit(RLIMIT_NOFILE, &original) < 0) return 4;
    if (failed != -1 || failure != EMFILE) return 5;
    int accepted = accept4(listener, NULL, NULL, SOCK_CLOEXEC | SOCK_NONBLOCK);
    if (accepted < 0 || fcntl(accepted, F_GETFD) != FD_CLOEXEC
        || !(fcntl(accepted, F_GETFL) & O_NONBLOCK)) return 6;
    char byte = 'a';
    if (write(client, &byte, 1) != 1 || read(accepted, &byte, 1) != 1 || byte != 'a') return 7;
    close(accepted); close(client);
    client = socket(AF_UNIX, SOCK_STREAM, 0);
    if (client < 0 || connect(client, (void *)&address, address_length) < 0) return 8;
    int expected = fcntl(listener, F_DUPFD_CLOEXEC, 0);
    if (expected < 0) return 9;
    close(expected);
    socklen_t peer_length = sizeof address;
    if (accept4(listener, (void *)(uintptr_t)1, &peer_length, SOCK_CLOEXEC) != -1 || errno != EFAULT) return 10;
    int check = dup(listener);
    if (check != expected) return 11;
    close(check);
    if (syscall(SYS_socketpair, AF_UNIX, SOCK_STREAM, 0, 0) != -1 || errno != EFAULT) return 12;
    check = dup(listener);
    if (check != expected) return 13;
    close(check); close(client); close(listener);
    return 0;
}

static int unix_closed_datagram_is_writable(void)
{
    int pair[2];
    if (socketpair(AF_UNIX, SOCK_DGRAM | SOCK_CLOEXEC, 0, pair) < 0) return 1;
    close(pair[1]);
    struct pollfd event = { .fd = pair[0], .events = POLLOUT };
    if (poll(&event, 1, 0) != 1 || !(event.revents & POLLOUT)) return 2;
    char byte = 'x';
    if (write(pair[0], &byte, 1) != -1 || errno != ECONNREFUSED) return 3;
    close(pair[0]);
    return 0;
}

static int unix_native_control_edges(void)
{
    int pair[2];
    if (socketpair(AF_UNIX, SOCK_DGRAM | SOCK_CLOEXEC, 0, pair) < 0) return 1;
    _Alignas(struct cmsghdr) unsigned char control[64] = {0};
    struct cmsghdr *header = (void *)control;
    header->cmsg_level = SOL_SOCKET; header->cmsg_type = SCM_RIGHTS;
    char byte = 'c';
    struct iovec iovec = { &byte, 1 };
    struct msghdr message = { .msg_iov = &iovec, .msg_iovlen = 1, .msg_control = control };
    for (size_t bytes = 0; bytes < sizeof(int); ++bytes) {
        header->cmsg_len = CMSG_LEN(bytes);
        message.msg_controllen = header->cmsg_len + 1;
        if (sendmsg(pair[0], &message, 0) != 1 || read(pair[1], &byte, 1) != 1) return 2;
    }
    header->cmsg_len = CMSG_LEN(sizeof(int) + 1);
    memcpy(CMSG_DATA(header), &pair[0], sizeof(int));
    message.msg_controllen = header->cmsg_len + 1;
    if (sendmsg(pair[0], &message, 0) != 1) return 3;
    memset(control, 0, sizeof control);
    message.msg_controllen = sizeof control;
    if (recvmsg(pair[1], &message, MSG_CMSG_CLOEXEC) != 1) return 4;
    header = CMSG_FIRSTHDR(&message);
    int received = -1;
    if (!header || header->cmsg_len != CMSG_LEN(sizeof(int))) return 5;
    memcpy(&received, CMSG_DATA(header), sizeof received);
    if (received < 0 || fcntl(received, F_GETFD) != FD_CLOEXEC) return 6;
    close(received); close(pair[0]); close(pair[1]);
    return 0;
}

static int socket_timeout_elapsed(struct timespec start, long minimum_ms)
{
    struct timespec end;
    if (clock_gettime(CLOCK_MONOTONIC, &end)) return -1;
    int64_t elapsed = (int64_t)(end.tv_sec - start.tv_sec) * 1000000000
        + end.tv_nsec - start.tv_nsec;
    return elapsed >= minimum_ms * 1000000 && elapsed < 5000000000LL ? 0 : -1;
}
static int socket_restart_ready = -1;
static void socket_restart_signal(int signal)
{
    (void)signal;
    ++ipc_signal_count;
    int saved = errno;
    if (write(socket_restart_ready, "H", 1) != 1) _exit(30);
    errno = saved;
}
/* 观察实际阻塞后发信号，handler 确认后才送数据；不以延时猜测 EINTR/restart 的先后。 */
static int socket_restart_case(int method, int mode)
{
    int pair[2], ready[2];
    if (socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, pair) || pipe2(ready, O_CLOEXEC)) return 1;
    pid_t child = fork();
    if (child == 0) {
        close(pair[0]); close(ready[0]);
        socket_restart_ready = ready[1];
        ipc_signal_count = 0;
        struct sigaction action = { .sa_handler = socket_restart_signal, .sa_flags = SA_RESTART };
        sigemptyset(&action.sa_mask);
        struct timeval value = { .tv_sec = 5 };
        if (sigaction(SIGUSR1, &action, NULL)
            || (mode && setsockopt(pair[1], SOL_SOCKET, SO_RCVTIMEO, &value, sizeof value))) _exit(2);
        if (mode == 2) {
            value = (struct timeval){0};
            if (setsockopt(pair[1], SOL_SOCKET, SO_RCVTIMEO, &value, sizeof value)) _exit(3);
        }
        char byte = 0;
        struct iovec vector = { .iov_base = &byte, .iov_len = 1 };
        struct msghdr message = { .msg_iov = &vector, .msg_iovlen = 1 };
        if (write(ready[1], "R", 1) != 1) _exit(4);
        ssize_t result;
        if (method == 0) result = read(pair[1], &byte, 1);
        else if (method == 1) result = readv(pair[1], &vector, 1);
        else if (method == 2) result = recv(pair[1], &byte, 1, 0);
        else if (method == 3) result = recvmsg(pair[1], &message, 0);
        else result = recvfrom(pair[1], &byte, 1, 0, NULL, NULL);
        if (mode == 1) {
            if (result != -1 || errno != EINTR || ipc_signal_count != 1) _exit(5);
            result = read(pair[1], &byte, 1);
        }
        _exit(result != 1 || byte != 'D' || ipc_signal_count != 1);
    }
    close(pair[1]); close(ready[1]);
    char byte;
    int failed = child < 0 || read(ready[0], &byte, 1) != 1 || byte != 'R'
        || ipc_wait_blocked(child) || kill(child, SIGUSR1)
        || read(ready[0], &byte, 1) != 1 || byte != 'H' || write(pair[0], "D", 1) != 1;
    if (failed && child > 0) kill(child, SIGKILL);
    int status = 0;
    if (child > 0 && (waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status))) failed = 1;
    close(pair[0]); close(ready[0]);
    return failed;
}
static int socket_timeouts(void)
{
    int pair[2];
    if (socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, pair)) return 1;
    int duplicate = dup(pair[1]);
    struct timeval value = {.tv_usec = 50000}, actual;
    if (duplicate < 0 || setsockopt(duplicate, SOL_SOCKET, SO_RCVTIMEO, &value, sizeof value)) return 2;
    socklen_t size = sizeof actual;
    if (getsockopt(pair[1], SOL_SOCKET, SO_RCVTIMEO, &actual, &size) || size != sizeof actual
        || actual.tv_sec != 0 || actual.tv_usec != 50000) return 3;
    char byte = 0;
    struct iovec vector = {.iov_base = &byte, .iov_len = 1};
    struct msghdr message = {.msg_iov = &vector, .msg_iovlen = 1};
    for (int method = 0; method < 5; ++method) {
        struct timespec start;
        if (clock_gettime(CLOCK_MONOTONIC, &start)) return 4;
        ssize_t result;
        if (method == 0) result = read(pair[1], &byte, 1);
        else if (method == 1) result = readv(pair[1], &vector, 1);
        else if (method == 2) result = recv(pair[1], &byte, 1, 0);
        else if (method == 3) result = recvmsg(pair[1], &message, 0);
        else result = recvfrom(pair[1], &byte, 1, 0, NULL, NULL);
        if (result != -1 || errno != EAGAIN || socket_timeout_elapsed(start, 40)) return 5;
    }
    value.tv_usec = 1000000;
    if (setsockopt(pair[1], SOL_SOCKET, SO_RCVTIMEO, &value, sizeof value) != -1 || errno != EDOM) return 6;
    size = sizeof actual;
    if (getsockopt(pair[1], SOL_SOCKET, SO_RCVTIMEO, &actual, &size) || actual.tv_usec != 50000) return 7;
    value = (struct timeval){.tv_sec = -1};
    if (setsockopt(pair[1], SOL_SOCKET, SO_RCVTIMEO, &value, sizeof value)
        || read(pair[1], &byte, 1) != -1 || errno != EAGAIN) return 8;
    size = sizeof actual;
    if (getsockopt(pair[1], SOL_SOCKET, SO_RCVTIMEO, &actual, &size) || actual.tv_sec || actual.tv_usec) return 9;
    value = (struct timeval){.tv_usec = 50000};
    if (setsockopt(pair[1], SOL_SOCKET, 66, &value, sizeof value)) return 10;
    size = 3;
    unsigned char short_value[4] = {0xcc,0xcc,0xcc,0xcc};
    if (getsockopt(pair[1], SOL_SOCKET, 66, short_value, &size) || size != 3 || short_value[3] != 0xcc) return 11;
    size = 0;
    if (getsockopt(pair[1], SOL_SOCKET, SO_RCVTIMEO, NULL, &size) || size != 0) return 12;
    size = (socklen_t)-1;
    if (getsockopt(pair[1], SOL_SOCKET, SO_RCVTIMEO, &actual, &size) != -1 || errno != EINVAL) return 13;
    if (recv(pair[1], &byte, 1, MSG_DONTWAIT) != -1 || errno != EAGAIN) return 14;
    if (write(pair[0], "D", 1) != 1 || read(pair[1], &byte, 1) != 1 || byte != 'D') return 15;
    close(duplicate); close(pair[0]); close(pair[1]);
    for (int kind = 0; kind < 2; ++kind) {
        if (socketpair(AF_UNIX, (kind ? SOCK_DGRAM : SOCK_STREAM) | SOCK_CLOEXEC, 0, pair)
            || setsockopt(pair[0], SOL_SOCKET, SO_SNDTIMEO, &value, sizeof value)
            || fcntl(pair[0], F_SETFL, O_NONBLOCK)) return 16;
        char payload[4096] = {0};
        size_t count = 0;
        while (send(pair[0], payload, sizeof payload, MSG_NOSIGNAL) > 0)
            if (++count > 2048) return 17;
        if (errno != EAGAIN || fcntl(pair[0], F_SETFL, 0)) return 18;
        for (int method = 0; method < 4; ++method) {
            struct timespec start;
            if (clock_gettime(CLOCK_MONOTONIC, &start)) return 19;
            ssize_t result;
            if (method == 0) result = write(pair[0], &byte, 1);
            else if (method == 1) result = writev(pair[0], &vector, 1);
            else if (method == 2) result = send(pair[0], &byte, 1, MSG_NOSIGNAL);
            else result = sendmsg(pair[0], &message, MSG_NOSIGNAL);
            if (result != -1 || errno != EAGAIN || socket_timeout_elapsed(start, 40)) return 20;
        }
        close(pair[0]); close(pair[1]);
    }
    int listener = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    struct sockaddr_un address = {.sun_family = AF_UNIX};
    if (listener < 0 || bind(listener, (struct sockaddr *)&address, sizeof address.sun_family)
        || listen(listener, 2) || setsockopt(listener, SOL_SOCKET, SO_RCVTIMEO, &value, sizeof value)) return 21;
    struct timespec start;
    if (clock_gettime(CLOCK_MONOTONIC, &start) || accept(listener, NULL, NULL) != -1
        || errno != EAGAIN || socket_timeout_elapsed(start, 40)) return 22;
    socklen_t address_length = sizeof address;
    int client = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    if (client < 0 || getsockname(listener, (struct sockaddr *)&address, &address_length)
        || connect(client, (struct sockaddr *)&address, address_length)) return 23;
    int accepted = accept(listener, NULL, NULL);
    size = sizeof actual;
    if (accepted < 0 || getsockopt(accepted, SOL_SOCKET, SO_RCVTIMEO, &actual, &size)
        || actual.tv_sec || actual.tv_usec) return 24;
    close(accepted); close(client); close(listener);
    for (int method = 0; method < 5; ++method)
        for (int mode = 0; mode < 3; ++mode)
            if (socket_restart_case(method, mode)) return 25 + method * 3 + mode;
    return 0;
}

struct unix_backlog_actor {
    int listener;
    int close_listener;
    int accepted;
};
static void *unix_backlog_release(void *input)
{
    struct unix_backlog_actor *actor = input;
    struct timespec delay = {.tv_nsec = 50000000};
    nanosleep(&delay, NULL);
    if (actor->close_listener) actor->accepted = close(actor->listener);
    else actor->accepted = accept(actor->listener, NULL, NULL);
    return NULL;
}
static int unix_blocking_connect(void)
{
    int listener = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    struct sockaddr_un address = {.sun_family = AF_UNIX};
    socklen_t size = sizeof address;
    if (listener < 0 || bind(listener, (struct sockaddr *)&address, sizeof address.sun_family)
        || getsockname(listener, (struct sockaddr *)&address, &size) || listen(listener, 0)) return 1;
    int first = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    int next = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC | SOCK_NONBLOCK, 0);
    if (first < 0 || next < 0 || connect(first, (struct sockaddr *)&address, size)
        || connect(next, (struct sockaddr *)&address, size) != -1 || errno != EAGAIN) return 2;
    struct timeval timeout = {.tv_usec = 50000};
    if (setsockopt(next, SOL_SOCKET, SO_SNDTIMEO, &timeout, sizeof timeout)
        || fcntl(next, F_SETFL, 0)) return 3;
    struct timespec start;
    if (clock_gettime(CLOCK_MONOTONIC, &start) || connect(next, (struct sockaddr *)&address, size) != -1
        || errno != EAGAIN || socket_timeout_elapsed(start, 40)) return 4;
    int accepted = accept(listener, NULL, NULL);
    if (accepted < 0 || connect(next, (struct sockaddr *)&address, size)) return 5;
    close(accepted); close(first);
    struct unix_backlog_actor actor = {.listener = listener, .accepted = -1};
    pthread_t thread;
    int third = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    timeout.tv_usec = 500000;
    if (third < 0 || setsockopt(third, SOL_SOCKET, SO_SNDTIMEO, &timeout, sizeof timeout)
        || pthread_create(&thread, NULL, unix_backlog_release, &actor)) return 6;
    if (clock_gettime(CLOCK_MONOTONIC, &start) || connect(third, (struct sockaddr *)&address, size)
        || socket_timeout_elapsed(start, 40) || pthread_join(thread, NULL) || actor.accepted < 0) return 7;
    close(actor.accepted); close(next);
    int closing = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    actor.close_listener = 1; actor.accepted = -1;
    if (closing < 0 || setsockopt(closing, SOL_SOCKET, SO_SNDTIMEO, &timeout, sizeof timeout)
        || pthread_create(&thread, NULL, unix_backlog_release, &actor)) return 8;
    if (connect(closing, (struct sockaddr *)&address, size) != -1 || errno != ECONNREFUSED
        || pthread_join(thread, NULL) || actor.accepted != 0) return 9;
    close(closing); close(third);
    return 0;
}

static int unix_full_pathname(void)
{
    char path[109];
    memset(path, 'p', sizeof path - 1);
    memcpy(path, "/tmp/", 5); path[108] = 0;
    unlink(path);
    struct sockaddr_un address = {.sun_family = AF_UNIX};
    memcpy(address.sun_path, path, sizeof address.sun_path);
    int listener = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    if (listener < 0 || bind(listener, (struct sockaddr *)&address, sizeof address)) return 1;
    unsigned char output[128]; memset(output, 0xcc, sizeof output);
    socklen_t length = sizeof output;
    if (getsockname(listener, (struct sockaddr *)output, &length) || length != sizeof address + 1
        || memcmp(output, &address, sizeof address) || output[sizeof address]
        || output[sizeof address + 1] != 0xcc) return 2;
    memset(output, 0xcc, sizeof output); length = sizeof address;
    if (getsockname(listener, (struct sockaddr *)output, &length) || length != sizeof address + 1
        || memcmp(output, &address, sizeof address) || output[sizeof address] != 0xcc) return 3;
    int client = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    if (client < 0 || listen(listener, 1) || connect(client, (struct sockaddr *)&address, sizeof address)) return 4;
    int accepted = accept(listener, NULL, NULL);
    length = sizeof output;
    if (accepted < 0 || getsockname(accepted, (struct sockaddr *)output, &length)
        || length != sizeof address + 1 || memcmp(output, &address, sizeof address)
        || output[sizeof address]) return 5;
    close(accepted); close(client); close(listener); unlink(path);
    return 0;
}

static int unix_autobind(void)
{
    struct sockaddr_un unnamed = {.sun_family = AF_UNIX};
    const socklen_t family_length = sizeof(unnamed.sun_family);
    for (size_t kind = 0; kind < 3; ++kind) {
        const int types[] = {SOCK_STREAM, SOCK_DGRAM, SOCK_SEQPACKET};
        int fd = socket(AF_UNIX, types[kind] | SOCK_CLOEXEC, 0);
        if (fd < 0 || bind(fd, (struct sockaddr *)&unnamed, family_length)) return 1;
        struct sockaddr_un bound = {0}, repeated = {0};
        socklen_t length = sizeof bound;
        if (getsockname(fd, (struct sockaddr *)&bound, &length) || length != family_length + 6
            || bound.sun_family != AF_UNIX || bound.sun_path[0]) return 2;
        for (size_t i = 1; i < 6; ++i)
            if (!((bound.sun_path[i] >= '0' && bound.sun_path[i] <= '9')
                  || (bound.sun_path[i] >= 'a' && bound.sun_path[i] <= 'f'))) return 3;
        if (bind(fd, (struct sockaddr *)&unnamed, family_length)) return 4;
        socklen_t repeated_length = sizeof repeated;
        if (getsockname(fd, (struct sockaddr *)&repeated, &repeated_length)
            || repeated_length != length || memcmp(&bound, &repeated, length)) return 5;
        int collision = socket(AF_UNIX, types[kind] | SOCK_CLOEXEC, 0);
        if (collision < 0 || bind(collision, (struct sockaddr *)&bound, length) != -1
            || errno != EADDRINUSE) return 6;
        int other = socket(AF_UNIX, types[(kind + 1) % 3] | SOCK_CLOEXEC, 0);
        if (other < 0 || bind(other, (struct sockaddr *)&bound, length)) return 7;
        if (connect(collision, (struct sockaddr *)&unnamed, family_length) != -1
            || errno != EINVAL) return 8;
        if (types[kind] == SOCK_DGRAM) {
            if (sendto(collision, "A", 1, 0, (struct sockaddr *)&bound, length) != 1) return 9;
            char byte = 0;
            if (recv(fd, &byte, 1, MSG_DONTWAIT) != 1 || byte != 'A') return 10;
        }
        close(fd);
        if (bind(collision, (struct sockaddr *)&bound, length)) return 11;
        close(collision);
        close(other);
    }
    return 0;
}

/* 消息凭据属于发送时的实际 PID/IDs；peer credentials 不代替每条消息的 controls。 */
static ssize_t unix_send_ucred(int fd, const void *bytes, size_t length,
                              const struct ucred *credentials, int passed)
{
    _Alignas(struct cmsghdr) unsigned char control[CMSG_SPACE(sizeof(struct ucred)) + CMSG_SPACE(sizeof(int))] = {0};
    struct iovec vector = { (void *)bytes, length };
    struct msghdr message = { .msg_iov = &vector, .msg_iovlen = 1 };
    size_t used = 0;
    if (credentials) {
        struct cmsghdr *header = (void *)control;
        header->cmsg_level = SOL_SOCKET; header->cmsg_type = SCM_CREDENTIALS;
        header->cmsg_len = CMSG_LEN(sizeof *credentials);
        memcpy(CMSG_DATA(header), credentials, sizeof *credentials);
        used = CMSG_SPACE(sizeof *credentials);
    }
    if (passed >= 0) {
        struct cmsghdr *header = (void *)(control + used);
        header->cmsg_level = SOL_SOCKET; header->cmsg_type = SCM_RIGHTS;
        header->cmsg_len = CMSG_LEN(sizeof passed);
        memcpy(CMSG_DATA(header), &passed, sizeof passed);
        used += CMSG_SPACE(sizeof passed);
    }
    if (used) { message.msg_control = control; message.msg_controllen = used; }
    return sendmsg(fd, &message, MSG_NOSIGNAL | MSG_DONTWAIT);
}

static int unix_recv_ucred(int fd, size_t length, int flags, size_t expected_bytes,
                          struct ucred expected, int expect_rights)
{
    _Alignas(struct cmsghdr) unsigned char control[128] = {0};
    char bytes[32];
    struct iovec vector = { bytes, length };
    struct msghdr message = { .msg_iov = &vector, .msg_iovlen = 1,
                             .msg_control = control, .msg_controllen = sizeof control };
    if (recvmsg(fd, &message, flags | MSG_DONTWAIT | MSG_CMSG_CLOEXEC) != (ssize_t)expected_bytes
        || (message.msg_flags & MSG_CTRUNC)) return -1;
    struct cmsghdr *header = CMSG_FIRSTHDR(&message);
    struct ucred credentials;
    if (!header || header->cmsg_level != SOL_SOCKET || header->cmsg_type != SCM_CREDENTIALS
        || header->cmsg_len != CMSG_LEN(sizeof credentials)) return -1;
    memcpy(&credentials, CMSG_DATA(header), sizeof credentials);
    if (credentials.pid != expected.pid || credentials.uid != expected.uid
        || credentials.gid != expected.gid) return -1;
    header = CMSG_NXTHDR(&message, header);
    if (!expect_rights) return header ? -1 : 0;
    if (!header || header->cmsg_level != SOL_SOCKET || header->cmsg_type != SCM_RIGHTS
        || header->cmsg_len != CMSG_LEN(sizeof(int))) return -1;
    int received;
    memcpy(&received, CMSG_DATA(header), sizeof received);
    if (received < 0 || fcntl(received, F_GETFD) != FD_CLOEXEC) return -1;
    close(received);
    return CMSG_NXTHDR(&message, header) ? -1 : 0;
}

static int unix_message_credentials(int type)
{
    int pair[2], enabled = 1;
    if (socketpair(AF_UNIX, type | SOCK_CLOEXEC, 0, pair)) return 1;
    /* 旧的无凭据 payload 不得借用后写入的凭据；stream 必须在真实边界停止。 */
    if (write(pair[0], "old", 3) != 3
        || setsockopt(pair[1], SOL_SOCKET, SO_PASSCRED, &enabled, sizeof enabled)) return 2;
    socklen_t option_length = sizeof enabled;
    int policy = 0;
    if (getsockopt(pair[1], SOL_SOCKET, SO_PASSCRED, &policy, &option_length)
        || policy != 1 || option_length != sizeof policy || write(pair[0], "new", 3) != 3) return 3;
    struct ucred own = { getpid(), getuid(), getgid() };
    struct ucred missing = { 0, 65534, 65534 };
    if (unix_recv_ucred(pair[1], 32, MSG_PEEK, 3, missing, 0)
        || unix_recv_ucred(pair[1], 32, 0, 3, missing, 0)
        || unix_recv_ucred(pair[1], 32, 0, 3, own, 0)) return 4;
    /* 显式 claim 即使发送时接收 option 关闭，也必须保存在实际消息。 */
    enabled = 0;
    if (setsockopt(pair[1], SOL_SOCKET, SO_PASSCRED, &enabled, sizeof enabled)
        || unix_send_ucred(pair[0], "E", 1, &own, -1) != 1) return 5;
    enabled = 1;
    if (setsockopt(pair[1], SOL_SOCKET, SO_PASSCRED, &enabled, sizeof enabled)
        || unix_recv_ucred(pair[1], 1, MSG_PEEK, 1, own, 0)
        || unix_recv_ucred(pair[1], 1, 0, 1, own, 0)) return 6;
    struct ucred invalid = own;
    invalid.pid = INT_MAX;
    if (unix_send_ucred(pair[0], "X", 1, &invalid, -1) != -1 || errno != ESRCH) return 7;
    invalid.uid = (uid_t)-1;
    if (unix_send_ucred(pair[0], "X", 1, &invalid, -1) != -1 || errno != EINVAL) return 8;
    /* credentials 在 rights 前，短 control 允许部分 ucred 并只截断实际溢出的内容。 */
    for (size_t capacity = 15; capacity <= 32; ++capacity) {
        if (unix_send_ucred(pair[0], "T", 1, &own, -1) != 1) return 9;
        _Alignas(struct cmsghdr) unsigned char control[40];
        memset(control, 0x7a, sizeof control);
        char byte;
        struct iovec vector = { &byte, 1 };
        struct msghdr message = { .msg_iov = &vector, .msg_iovlen = 1,
                                 .msg_control = control, .msg_controllen = capacity };
        if (recvmsg(pair[1], &message, MSG_DONTWAIT) != 1 || byte != 'T'
            || !!(message.msg_flags & MSG_CTRUNC) != (capacity < CMSG_LEN(sizeof own))) return 10;
        size_t expected_length = capacity < sizeof(struct cmsghdr) ? 0 : capacity < CMSG_SPACE(sizeof own) ? capacity : CMSG_SPACE(sizeof own);
        if (message.msg_controllen != expected_length) return 11;
        if (capacity >= sizeof(struct cmsghdr)) {
            struct cmsghdr *header = (void *)control;
            if (header->cmsg_type != SCM_CREDENTIALS || header->cmsg_level != SOL_SOCKET
                || header->cmsg_len != (capacity < CMSG_LEN(sizeof own) ? capacity : CMSG_LEN(sizeof own))) return 12;
        }
        for (size_t i = capacity; i < sizeof control; ++i) if (control[i] != 0x7a) return 13;
    }
    if (unix_send_ucred(pair[0], "R", 1, &own, pair[0]) != 1
        || unix_recv_ucred(pair[1], 1, MSG_PEEK, 1, own, 1)
        || unix_recv_ucred(pair[1], 1, MSG_PEEK, 1, own, 1)
        || unix_recv_ucred(pair[1], 1, 0, 1, own, 1)) return 14;
    if (type == SOCK_STREAM) {
        if (unix_send_ucred(pair[0], "", 0, &own, pair[0]) != 0) return 15;
        struct pollfd event = { .fd = pair[1], .events = POLLIN };
        if (poll(&event, 1, 0) != 0) return 16;
        if (unix_send_ucred(pair[0], "Z", 1, &own, pair[0]) != 1
            || unix_recv_ucred(pair[1], 0, MSG_PEEK, 0, own, 1)
            || unix_recv_ucred(pair[1], 0, MSG_PEEK, 0, own, 1)
            || unix_recv_ucred(pair[1], 0, 0, 0, own, 1)
            || unix_recv_ucred(pair[1], 1, 0, 1, own, 0)) return 17;
    }
    if (setsockopt(pair[0], SOL_SOCKET, SO_PASSCRED, &enabled, sizeof enabled)
        || write(pair[0], "A", 1) != 1 || unix_recv_ucred(pair[1], 1, 0, 1, own, 0)) return 18;
    struct sockaddr_un bound = {0};
    socklen_t name_length = sizeof bound;
    if (getsockname(pair[0], (void *)&bound, &name_length)
        || name_length != (type == SOCK_STREAM ? sizeof(sa_family_t) : sizeof(sa_family_t) + 6)
        || bound.sun_family != AF_UNIX) return 19;
    close(pair[0]); close(pair[1]);
    return 0;
}

static int unix_real_sender_credentials(void)
{
    int pair[2], enabled = 1;
    if (socketpair(AF_UNIX, SOCK_DGRAM | SOCK_CLOEXEC, 0, pair)
        || setsockopt(pair[1], SOL_SOCKET, SO_PASSCRED, &enabled, sizeof enabled)) return 1;
    pid_t parent = getpid(), child = fork();
    if (child < 0) return 2;
    if (child == 0) {
        close(pair[1]);
        if (setresgid(2000, 2001, 2002) || setresuid(1000, 1001, 1002)) _exit(3);
        struct ucred claim = { parent, 1000, 2000 };
        if (unix_send_ucred(pair[0], "bad", 3, &claim, -1) != -1 || errno != EPERM) _exit(4);
        claim.pid = getpid(); claim.uid = 1003;
        if (unix_send_ucred(pair[0], "bad", 3, &claim, -1) != -1 || errno != EPERM) _exit(5);
        if (write(pair[0], "real", 4) != 4) _exit(6);
        claim.uid = 1002; claim.gid = 2002;
        if (unix_send_ucred(pair[0], "saved", 5, &claim, -1) != 5) _exit(7);
        _exit(0);
    }
    close(pair[0]);
    int status;
    if (waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status)) return 8;
    if (unix_recv_ucred(pair[1], 32, 0, 4, (struct ucred){ child, 1000, 2000 }, 0)
        || unix_recv_ucred(pair[1], 32, 0, 5, (struct ucred){ child, 1002, 2002 }, 0)) return 9;
    close(pair[1]);
    return 0;
}

static int unix_seqpacket_lifecycle(void)
{
    int listener = socket(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0);
    int client = socket(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0);
    if (listener < 0 || client < 0) return 1;
    char byte;
    if (send(client, "x", 1, MSG_DONTWAIT | MSG_NOSIGNAL) != -1 || errno != ENOTCONN
        || recv(client, &byte, 1, MSG_DONTWAIT) != -1 || errno != ENOTCONN) return 2;
    struct sockaddr_un address = { .sun_family = AF_UNIX };
    int used = snprintf(address.sun_path + 1, sizeof address.sun_path - 1, "seq-lifecycle-%d", getpid());
    socklen_t address_length = sizeof(sa_family_t) + 1 + used;
    struct sockaddr_un peer = {0};
    socklen_t peer_length = sizeof peer;
    if (getpeername(client, (void *)&peer, &peer_length) != -1 || errno != ENOTCONN) return 3;
    int enabled = 1;
    if (bind(listener, (void *)&address, address_length)
        || setsockopt(listener, SOL_SOCKET, SO_PASSCRED, &enabled, sizeof enabled)
        || listen(listener, 0) || connect(client, (void *)&address, address_length)) return 4;
    /* 消息在 accept 之前进入同一 child queue，凭据属于实际发送者。 */
    if (send(client, "one", 3, 0) != 3 || send(client, "twotwo", 6, 0) != 6
        || shutdown(client, SHUT_WR)) return 5;
    enabled = 0;
    if (setsockopt(listener, SOL_SOCKET, SO_PASSCRED, &enabled, sizeof enabled)) return 6;
    int server = accept4(listener, NULL, NULL, SOCK_CLOEXEC | SOCK_NONBLOCK);
    int type = 0; socklen_t option_length = sizeof type;
    if (server < 0 || getsockopt(server, SOL_SOCKET, SO_TYPE, &type, &option_length)
        || type != SOCK_SEQPACKET || getsockopt(server, SOL_SOCKET, SO_PASSCRED, &enabled, &option_length)
        || enabled != 1) return 7;
    struct ucred own = { getpid(), getuid(), getgid() };
    if (unix_recv_ucred(server, 32, MSG_PEEK, 3, own, 0)
        || unix_recv_ucred(server, 32, 0, 3, own, 0)) return 8;
    char bytes[3];
    struct iovec vector = { bytes, sizeof bytes };
    _Alignas(struct cmsghdr) unsigned char control[64];
    struct msghdr message = { .msg_iov = &vector, .msg_iovlen = 1, .msg_control = control, .msg_controllen = sizeof control };
    if (recvmsg(server, &message, MSG_TRUNC) != 6 || memcmp(bytes, "two", 3)
        || !(message.msg_flags & MSG_TRUNC) || recv(server, bytes, sizeof bytes, MSG_DONTWAIT) != 0) return 9;
    struct pollfd event = { .fd = server, .events = POLLIN | POLLOUT };
    if (poll(&event, 1, 0) != 1 || !(event.revents & POLLIN) || !(event.revents & POLLOUT)
        || (event.revents & POLLHUP)) return 10;
    /* SeqPacket 忽略已验证的显式地址，仍向唯一 connected peer 投递。 */
    struct sockaddr ignored = { .sa_family = AF_INET };
    if (sendto(server, "reply", 5, MSG_NOSIGNAL, &ignored, sizeof ignored) != 5
        || recv(client, bytes, sizeof bytes, MSG_TRUNC) != 5 || memcmp(bytes, "rep", 3)) return 11;
    if (sendto(server, "x", 1, MSG_NOSIGNAL, (void *)(uintptr_t)1, sizeof ignored) != -1
        || errno != EFAULT || send(client, "x", 1, MSG_NOSIGNAL) != -1 || errno != EPIPE) return 12;
    peer_length = sizeof peer;
    if (getpeername(client, (void *)&peer, &peer_length) || peer_length != address_length
        || memcmp(&peer, &address, address_length)) return 13;
    if (shutdown(server, SHUT_RDWR)) return 14;
    event = (struct pollfd){ .fd = client, .events = POLLIN | POLLOUT };
    if (poll(&event, 1, 0) != 1 || !(event.revents & POLLHUP)
        || recv(client, bytes, sizeof bytes, MSG_DONTWAIT) != 0) return 15;
    close(server);
    peer_length = sizeof peer;
    if (getpeername(client, (void *)&peer, &peer_length) || peer_length != address_length
        || memcmp(&peer, &address, address_length)) return 16;
    close(client); close(listener);
    return 0;
}

static int unix_pathname_datagram(void)
{
    int source = socket(AF_UNIX, SOCK_DGRAM | SOCK_CLOEXEC, 0);
    int target = socket(AF_UNIX, SOCK_DGRAM | SOCK_CLOEXEC, 0);
    struct sockaddr_un destination = { .sun_family = AF_UNIX }, sender = { .sun_family = AF_UNIX };
    snprintf(destination.sun_path, sizeof destination.sun_path, "/tmp/uds-target-%d", getpid());
    snprintf(sender.sun_path, sizeof sender.sun_path, "/tmp/uds-source-%d", getpid());
    socklen_t destination_length = sizeof(sa_family_t) + strlen(destination.sun_path) + 1;
    socklen_t sender_length = sizeof(sa_family_t) + strlen(sender.sun_path) + 1;
    if (source < 0 || target < 0 || bind(target, (void *)&destination, destination_length)
        || sendto(source, "P", 1, 0, (void *)&destination, destination_length) != 1
        || bind(source, (void *)&sender, sender_length)) return 1;
    struct stat original, replacement;
    if (stat(destination.sun_path, &original) || unlink(destination.sun_path)) return 3;
    int next = socket(AF_UNIX, SOCK_DGRAM | SOCK_CLOEXEC, 0);
    if (next < 0 || bind(next, (void *)&destination, destination_length)
        || stat(destination.sun_path, &replacement)
        || replacement.st_dev != original.st_dev || replacement.st_ino == original.st_ino
        || sendto(source, "R", 1, 0, (void *)&destination, destination_length) != 1) return 4;
    close(source);
    for (int peek = 1; peek >= 0; --peek) {
        struct sockaddr_un received = {0}; socklen_t length = sizeof received;
        char byte = 0;
        if (recvfrom(target, &byte, 1, (peek ? MSG_PEEK : 0) | MSG_DONTWAIT, (void *)&received, &length) != 1
            || byte != 'P' || length != sender_length || memcmp(&received, &sender, length)) return 2;
    }
    char byte;
    if (recv(target, &byte, 1, MSG_DONTWAIT) != -1 || errno != EAGAIN
        || recv(next, &byte, 1, MSG_DONTWAIT) != 1 || byte != 'R') return 5;
    close(target); close(next); unlink(destination.sun_path); unlink(sender.sun_path);
    return 0;
}

static int unix_close_reset(int type)
{
    int pair[2]; char bytes[16];
    if (socketpair(AF_UNIX, type | SOCK_CLOEXEC, 0, pair)
        || send(pair[0], "unread", 6, MSG_NOSIGNAL) != 6
        || send(pair[1], "queued", 6, MSG_NOSIGNAL) != 6) return 1;
    close(pair[1]);
    struct pollfd event = { .fd = pair[0], .events = POLLIN | POLLOUT | POLLRDHUP };
    if (poll(&event, 1, 0) != 1 || !(event.revents & POLLERR)
        || !(event.revents & POLLHUP) || !(event.revents & POLLRDHUP)) return 2;
    if (type == SOCK_STREAM && (recv(pair[0], bytes, sizeof bytes, MSG_DONTWAIT) != 6
        || memcmp(bytes, "queued", 6))) return 3;
    if (recv(pair[0], bytes, sizeof bytes, MSG_DONTWAIT) != -1 || errno != ECONNRESET) return 4;
    int error = -1; socklen_t length = sizeof error;
    if (getsockopt(pair[0], SOL_SOCKET, SO_ERROR, &error, &length) || error) return 5;
    if (type == SOCK_SEQPACKET && (recv(pair[0], bytes, sizeof bytes, MSG_DONTWAIT) != 6
        || memcmp(bytes, "queued", 6))) return 6;
    if (recv(pair[0], bytes, sizeof bytes, MSG_DONTWAIT) != 0) return 7;
    close(pair[0]);
    if (socketpair(AF_UNIX, type | SOCK_CLOEXEC, 0, pair)
        || send(pair[0], "lost", 4, MSG_NOSIGNAL) != 4) return 8;
    close(pair[1]);
    length = sizeof error;
    if (getsockopt(pair[0], SOL_SOCKET, SO_ERROR, &error, &length) || error != ECONNRESET
        || getsockopt(pair[0], SOL_SOCKET, SO_ERROR, &error, &length) || error
        || recv(pair[0], bytes, sizeof bytes, MSG_DONTWAIT) != 0) return 9;
    close(pair[0]);
    return 0;
}

static int unix_stream_shutdown_drain(void)
{
    int pair[2]; char bytes[16];
    if (socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, pair)
        || send(pair[0], "history", 7, MSG_NOSIGNAL) != 7
        || shutdown(pair[1], SHUT_RD)) return 1;
    struct pollfd event = { .fd = pair[1], .events = POLLIN | POLLOUT | POLLRDHUP };
    if (poll(&event, 1, 0) != 1 || !(event.revents & POLLIN)
        || !(event.revents & POLLRDHUP) || (event.revents & (POLLHUP | POLLERR))) return 2;
    if (send(pair[0], "new", 3, MSG_NOSIGNAL) != -1 || errno != EPIPE) return 3;
    if (recv(pair[1], bytes, sizeof bytes, MSG_DONTWAIT) != 7 || memcmp(bytes, "history", 7)
        || recv(pair[1], bytes, sizeof bytes, MSG_DONTWAIT) != 0) return 4;
    if (shutdown(pair[1], SHUT_WR)) return 5;
    event.revents = 0;
    if (poll(&event, 1, 0) != 1 || !(event.revents & POLLHUP)) return 6;
    close(pair[0]); close(pair[1]);
    return 0;
}

static int unix_datagram_disconnect(void)
{
    int pair[2]; char byte;
    if (socketpair(AF_UNIX, SOCK_DGRAM | SOCK_CLOEXEC, 0, pair)
        || send(pair[1], "Q", 1, MSG_NOSIGNAL) != 1) return 1;
    struct sockaddr address = { .sa_family = AF_UNSPEC };
    if (connect(pair[0], &address, sizeof address)) return 2;
    if (recv(pair[0], &byte, 1, MSG_DONTWAIT) != -1 || errno != EAGAIN
        || send(pair[0], "x", 1, MSG_NOSIGNAL) != -1 || errno != ENOTCONN) return 3;
    int error = 0; socklen_t length = sizeof error;
    if (getsockopt(pair[1], SOL_SOCKET, SO_ERROR, &error, &length) || error != ECONNRESET
        || getsockopt(pair[1], SOL_SOCKET, SO_ERROR, &error, &length) || error) return 4;
    close(pair[0]); close(pair[1]);
    if (socketpair(AF_UNIX, SOCK_DGRAM | SOCK_CLOEXEC, 0, pair)
        || send(pair[1], "lost", 4, MSG_NOSIGNAL) != 4) return 5;
    close(pair[1]);
    if (send(pair[0], "x", 1, MSG_NOSIGNAL) != -1 || errno != ECONNREFUSED
        || send(pair[0], "x", 1, MSG_NOSIGNAL) != -1 || errno != ENOTCONN
        || recv(pair[0], &byte, 1, MSG_DONTWAIT) != -1 || errno != EAGAIN) return 6;
    close(pair[0]);
    return 0;
}

static int unix_packet_memory(int type)
{
    int pair[2];
    if (socketpair(AF_UNIX, type | SOCK_CLOEXEC, 0, pair)) return 1;
    /* 互连 packet 不使用非互连 receiver 的 max_dgram_qlen 限制。 */
    for (int i = 0; i < 16; ++i)
        if (send(pair[0], "q", 1, MSG_DONTWAIT | MSG_NOSIGNAL) != 1) return 2;
    char byte;
    for (int i = 0; i < 16; ++i)
        if (recv(pair[1], &byte, 1, MSG_DONTWAIT) != 1 || byte != 'q') return 3;
    const size_t length = 100000;
    unsigned char *payload = malloc(length), *output = malloc(length + 2);
    if (!payload || !output) return 4;
    memset(payload, 0x79, length); memset(output, 0xaa, length + 2);
    if (send(pair[0], payload, length, MSG_DONTWAIT | MSG_NOSIGNAL) != (ssize_t)length) return 5;
    struct pollfd event = { .fd = pair[0], .events = POLLOUT };
    if (poll(&event, 1, 0) != 0 || (event.revents & POLLOUT)) return 6;
    for (int i = 0; i < 3; ++i) {
        if (recv(pair[1], output + 1, length, (i < 2 ? MSG_PEEK : 0) | MSG_DONTWAIT) != (ssize_t)length
            || memcmp(output + 1, payload, length) || output[0] != 0xaa || output[length + 1] != 0xaa) return 7;
        event.revents = 0;
        if (i < 2 && poll(&event, 1, 0) != 0) return 8;
    }
    if (poll(&event, 1, 0) != 1 || !(event.revents & POLLOUT)) return 9;
    free(payload); free(output); close(pair[0]); close(pair[1]);
    return 0;
}

static const char *core_buffer_paths[] = {
    "/proc/sys/net/core/wmem_default", "/proc/sys/net/core/rmem_default",
    "/proc/sys/net/core/wmem_max", "/proc/sys/net/core/rmem_max",
    "/proc/sys/net/core/optmem_max"
};
static int core_buffer_read(size_t index, unsigned long *value)
{
    int fd = open(core_buffer_paths[index], O_RDONLY | O_CLOEXEC);
    if (fd < 0) return -1;
    char text[64], *end;
    ssize_t count = read(fd, text, sizeof text - 1);
    close(fd);
    if (count <= 0) return -1;
    text[count] = 0;
    errno = 0; *value = strtoul(text, &end, 10);
    return errno || end == text || strcmp(end, "\n") ? -1 : 0;
}
static int core_buffer_write(size_t index, unsigned long value)
{
    int fd = open(core_buffer_paths[index], O_WRONLY | O_CLOEXEC);
    if (fd < 0) return -1;
    char text[64]; int length = snprintf(text, sizeof text, "%lu\n", value);
    ssize_t count = write(fd, text, (size_t)length);
    int saved = errno; close(fd); errno = saved;
    return count == length ? 0 : -1;
}
static int unix_global_buffer_options(void)
{
    unsigned long saved[4], actual_limit;
    for (size_t i = 0; i < 4; ++i)
        if (core_buffer_read(i, &saved[i])) return 1;
    int pair[2] = {-1, -1}, next[2] = {-1, -1}, result = 0, actual;
    socklen_t size = sizeof actual;
    if (socketpair(AF_UNIX, SOCK_DGRAM | SOCK_CLOEXEC, 0, pair)) { result = 2; goto restore; }
    const unsigned long tuned[] = {32768, 16384, 65536, 32768};
    for (size_t i = 0; i < 4; ++i)
        if (core_buffer_write(i, tuned[i])) { result = 3; goto restore; }
    if (socketpair(AF_UNIX, SOCK_DGRAM | SOCK_CLOEXEC, 0, next)
        || getsockopt(pair[0], SOL_SOCKET, SO_SNDBUF, &actual, &size)
        || actual != (int)saved[0]) { result = 4; goto restore; }
    size = sizeof actual;
    if (getsockopt(next[0], SOL_SOCKET, SO_SNDBUF, &actual, &size)
        || actual != 32768) { result = 5; goto restore; }
    size = sizeof actual;
    if (getsockopt(next[0], SOL_SOCKET, SO_RCVBUF, &actual, &size)
        || actual != 16384) { result = 6; goto restore; }
    int requested = -1; size = sizeof actual;
    if (setsockopt(pair[0], SOL_SOCKET, SO_SNDBUF, &requested, sizeof requested)
        || getsockopt(pair[0], SOL_SOCKET, SO_SNDBUF, &actual, &size)
        || actual != 131072) { result = 7; goto restore; }
    if (core_buffer_write(2, 8192)) { result = 8; goto restore; }
    size = sizeof actual;
    if (getsockopt(pair[0], SOL_SOCKET, SO_SNDBUF, &actual, &size)
        || actual != 131072
        || setsockopt(pair[0], SOL_SOCKET, SO_SNDBUF, &requested, sizeof requested)) { result = 9; goto restore; }
    size = sizeof actual;
    if (getsockopt(pair[0], SOL_SOCKET, SO_SNDBUF, &actual, &size)
        || actual != 16384) { result = 10; goto restore; }
    int force = 65536;
    if (setsockopt(pair[0], SOL_SOCKET, SO_SNDBUFFORCE, &force, sizeof force)
        || setsockopt(pair[0], SOL_SOCKET, SO_RCVBUFFORCE, &force, sizeof force)) { result = 15; goto restore; }
    size = sizeof actual;
    if (getsockopt(pair[0], SOL_SOCKET, SO_SNDBUF, &actual, &size)
        || actual != 131072) { result = 16; goto restore; }
    size = sizeof actual;
    if (getsockopt(pair[0], SOL_SOCKET, SO_RCVBUF, &actual, &size)
        || actual != 131072) { result = 17; goto restore; }
    force = -1;
    if (setsockopt(pair[0], SOL_SOCKET, SO_SNDBUFFORCE, &force, sizeof force)
        || setsockopt(pair[0], SOL_SOCKET, SO_RCVBUFFORCE, &force, sizeof force)) { result = 18; goto restore; }
    size = sizeof actual;
    if (getsockopt(pair[0], SOL_SOCKET, SO_SNDBUF, &actual, &size)
        || actual < 4096 || actual >= 16384) { result = 19; goto restore; }
    size = sizeof actual;
    if (getsockopt(pair[0], SOL_SOCKET, SO_RCVBUF, &actual, &size)
        || actual < 2048 || actual >= 16384) { result = 20; goto restore; }
    for (size_t i = 0; i < 4; ++i) {
        errno = 0;
        if (!core_buffer_write(i, 0) || errno != EINVAL
            || core_buffer_read(i, &actual_limit)
            || actual_limit != (i == 2 ? 8192 : tuned[i])) { result = 11; goto restore; }
    }
    pid_t child = fork();
    if (child < 0) { result = 12; goto restore; }
    if (!child) {
        if (setuid(1000)) _exit(1);
        int request = 65536;
        const int options[] = { SO_SNDBUFFORCE, SO_RCVBUFFORCE };
        for (size_t i = 0; i < 2; ++i) {
            errno = 0;
            if (setsockopt(pair[0], SOL_SOCKET, options[i], &request, sizeof request) != -1 || errno != EPERM) _exit(4);
            errno = 0;
            if (setsockopt(pair[0], SOL_SOCKET, options[i], (void *)1, sizeof request) != -1 || errno != EFAULT) _exit(5);
        }
        for (size_t i = 0; i < 5; ++i) {
            errno = 0;
            if (!core_buffer_write(i, 32768) || (errno != EACCES && errno != EPERM)) _exit(2);
            if (core_buffer_read(i, &actual_limit)) _exit(3);
        }
        _exit(0);
    }
    int status;
    if (waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status)) result = 13;
restore:
    // 全局 sysctl 必须在任何失败出口归还；后续 ABI 案例使用原预算。
    for (size_t i = 0; i < 4; ++i)
        if (core_buffer_write(i, saved[i])) result = 14;
    for (size_t i = 0; i < 2; ++i) {
        if (pair[i] >= 0) close(pair[i]);
        if (next[i] >= 0) close(next[i]);
    }
    return result;
}

static int unix_optmem_options(int type)
{
    unsigned long saved;
    if (core_buffer_read(4, &saved)) return 1;
    int pair[2] = {-1, -1}, file = -1, result = 0;
    if (socketpair(AF_UNIX, type | SOCK_CLOEXEC | SOCK_NONBLOCK, 0, pair)
        || (file = open("/dev/null", O_RDONLY | O_CLOEXEC)) < 0) { result = 2; goto restore; }
    unsigned char control[CMSG_SPACE(6 * sizeof(int))];
    memset(control, 0, sizeof control);
    char byte = 'o'; struct iovec iov = { &byte, 1 };
    struct msghdr message = { .msg_iov = &iov, .msg_iovlen = 1,
        .msg_control = control, .msg_controllen = sizeof control };
    struct cmsghdr *header = CMSG_FIRSTHDR(&message);
    header->cmsg_level = SOL_SOCKET; header->cmsg_type = SCM_RIGHTS;
    header->cmsg_len = CMSG_LEN(6 * sizeof(int));
    for (size_t i = 0; i < 6; ++i) memcpy(CMSG_DATA(header) + i * sizeof(int), &file, sizeof file);
    if (core_buffer_write(4, sizeof control)) { result = 3; goto restore; }
    errno = 0;
    if (sendmsg(pair[0], &message, MSG_NOSIGNAL) != -1 || errno != ENOBUFS) { result = 4; goto restore; }
    if (core_buffer_write(4, sizeof control + 1)) { result = 5; goto restore; }
    for (int round = 0; round < 16; ++round) {
        // 先制造已预收费 heap copy fault，再用相同 socket 成功发送，证明 fault/success 都退款。
        void *pointer = message.msg_control;
        message.msg_control = (void *)1; errno = 0;
        if (syscall(SYS_sendmsg, pair[0], &message, MSG_NOSIGNAL) != -1 || errno != EFAULT) { result = 6; goto restore; }
        message.msg_control = pointer;
        if (sendmsg(pair[0], &message, MSG_NOSIGNAL) != 1) { result = 7; goto restore; }
        unsigned char received[CMSG_SPACE(6 * sizeof(int))];
        struct msghdr output = { .msg_iov = &iov, .msg_iovlen = 1,
            .msg_control = received, .msg_controllen = sizeof received };
        if (recvmsg(pair[1], &output, MSG_CMSG_CLOEXEC) != 1 || output.msg_flags & MSG_CTRUNC) { result = 8; goto restore; }
        struct cmsghdr *rights = CMSG_FIRSTHDR(&output);
        if (!rights || rights->cmsg_level != SOL_SOCKET || rights->cmsg_type != SCM_RIGHTS
            || rights->cmsg_len != CMSG_LEN(6 * sizeof(int))) { result = 9; goto restore; }
        for (size_t i = 0; i < 6; ++i) {
            int fd; memcpy(&fd, CMSG_DATA(rights) + i * sizeof(int), sizeof fd);
            if (!(fcntl(fd, F_GETFD) & FD_CLOEXEC)) result = 10;
            close(fd);
        }
        if (result) goto restore;
    }
    if (core_buffer_write(4, 0)) { result = 11; goto restore; }
    errno = 0;
    if (sendmsg(pair[0], &message, MSG_NOSIGNAL) != -1 || errno != ENOBUFS) { result = 12; goto restore; }
    // 一个 FD 的32-byte stack control 在零 optmem 时仍成功。
    message.msg_controllen = CMSG_SPACE(sizeof(int)); header->cmsg_len = CMSG_LEN(sizeof(int));
    if (sendmsg(pair[0], &message, MSG_NOSIGNAL) != 1) { result = 13; goto restore; }
    unsigned char received[CMSG_SPACE(sizeof(int))];
    struct msghdr output = { .msg_iov = &iov, .msg_iovlen = 1,
        .msg_control = received, .msg_controllen = sizeof received };
    if (recvmsg(pair[1], &output, MSG_CMSG_CLOEXEC) != 1 || output.msg_flags & MSG_CTRUNC) { result = 14; goto restore; }
    struct cmsghdr *rights = CMSG_FIRSTHDR(&output);
    if (!rights || rights->cmsg_len != CMSG_LEN(sizeof(int))) { result = 15; goto restore; }
    int passed; memcpy(&passed, CMSG_DATA(rights), sizeof passed); close(passed);
restore:
    if (core_buffer_write(4, saved)) result = 16;
    if (file >= 0) close(file);
    for (size_t i = 0; i < 2; ++i) if (pair[i] >= 0) close(pair[i]);
    return result;
}

static int unix_buffer_options(int type)
{
    int pair[2], value = 32768, actual = 0;
    socklen_t size = sizeof actual;
    if (socketpair(AF_UNIX, type | SOCK_CLOEXEC | SOCK_NONBLOCK, 0, pair)
        || setsockopt(pair[0], SOL_SOCKET, SO_SNDBUF, &value, sizeof value)
        || getsockopt(pair[0], SOL_SOCKET, SO_SNDBUF, &actual, &size)
        || actual != value * 2 || size != sizeof actual) return 1;
    int copy = dup(pair[0]);
    value = -1; size = sizeof actual;
    if (copy < 0 || setsockopt(copy, SOL_SOCKET, SO_SNDBUF, &value, sizeof value)
        || getsockopt(pair[0], SOL_SOCKET, SO_SNDBUF, &actual, &size)
        || actual < 65536) return 2;
    unsigned char short_value[3] = {0xaa, 0xaa, 0xaa}; size = 1;
    if (getsockopt(copy, SOL_SOCKET, SO_SNDBUF, short_value + 1, &size)
        || size != 1 || short_value[1] != ((unsigned char *)&actual)[0]
        || short_value[0] != 0xaa || short_value[2] != 0xaa) return 3;
    value = 0; size = sizeof actual;
    if (setsockopt(pair[0], SOL_SOCKET, SO_SNDBUF, &value, sizeof value)
        || getsockopt(pair[0], SOL_SOCKET, SO_SNDBUF, &actual, &size)
        || actual < 4096) return 4;
    size = sizeof value;
    int receive = 0;
    if (setsockopt(pair[1], SOL_SOCKET, SO_RCVBUF, &receive, sizeof receive)
        || getsockopt(pair[1], SOL_SOCKET, SO_RCVBUF, &receive, &size)
        || receive < 2048) return 5;
    unsigned char payload[8192], output[8192]; memset(payload, 0x62, sizeof payload);
    size_t queued = 0;
    if (type == SOCK_STREAM) {
        /* 一个真实 send budget 限制整条 byte stream；peek 与部分读取分别观察和归还费用。 */
        for (int i = 0; i < 64; ++i) {
            ssize_t count = send(pair[0], payload, sizeof payload, MSG_DONTWAIT | MSG_NOSIGNAL);
            if (count < 0) { if (errno != EAGAIN) return 6; break; }
            if (!count) return 7;
            queued += (size_t)count;
            if (i == 63) return 8;
        }
        if (!queued) return 9;
        struct pollfd event = { .fd = pair[0], .events = POLLOUT };
        if (poll(&event, 1, 0)) return 10;
        for (int i = 0; i < 2; ++i)
            if (recv(pair[1], output, 97, MSG_PEEK | MSG_DONTWAIT) != 97
                || memcmp(output, payload, 97) || poll(&event, 1, 0)) return 11;
        ssize_t prefix = recv(pair[1], output, 97, MSG_DONTWAIT);
        if (prefix != 97 || memcmp(output, payload, 97) || poll(&event, 1, 0)
            || send(pair[0], payload, 1, MSG_DONTWAIT | MSG_NOSIGNAL) != -1 || errno != EAGAIN) return 16;
        queued -= 97;
        while (queued) {
            ssize_t count = recv(pair[1], output, 97, MSG_DONTWAIT);
            if (count <= 0 || (size_t)count > queued || memcmp(output, payload, (size_t)count)) return 12;
            queued -= (size_t)count;
        }
        if (poll(&event, 1, 0) != 1 || !(event.revents & POLLOUT)
            || send(pair[0], payload, 1, MSG_DONTWAIT | MSG_NOSIGNAL) != 1) return 13;
    } else {
        /* actual sndbuf-32 是 packet 长度约束；RCVBUF 不创建额外 Unix receiver byte cap。 */
        unsigned char *large = malloc((size_t)actual);
        if (!large) return 14;
        memset(large, 0x63, (size_t)actual);
        if (send(pair[0], large, (size_t)actual - 31, MSG_DONTWAIT | MSG_NOSIGNAL) != -1
            || errno != EMSGSIZE
            || send(pair[0], large, (size_t)actual - 32, MSG_DONTWAIT | MSG_NOSIGNAL) != actual - 32
            || recv(pair[1], large, (size_t)actual, MSG_DONTWAIT) != actual - 32) return 15;
        free(large);
    }
    close(copy); close(pair[0]); close(pair[1]); return 0;
}

static int unix_send_protocol_edges(void)
{
    /* musl sendmsg 先复制 control；用原生 syscall 验证内核的 EFAULT 与协议错误优先级。 */
    int pair[2], fd = socket(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0);
    struct sockaddr_un unnamed; socklen_t unnamed_length = sizeof unnamed;
    if (fd < 0 || getsockname(fd, (void *)&unnamed, &unnamed_length)
        || unnamed_length != sizeof unnamed.sun_family || unnamed.sun_family != AF_UNIX) return 14;
    unsigned char control[CMSG_SPACE(0)] = {0};
    struct cmsghdr *header = (struct cmsghdr *)control;
    header->cmsg_len = CMSG_LEN(0); header->cmsg_level = SOL_SOCKET; header->cmsg_type = 0x1234;
    struct msghdr message = { .msg_control = control, .msg_controllen = sizeof control };
    if (fd < 0 || syscall(SYS_sendmsg, fd, &message, MSG_NOSIGNAL) != -1 || errno != ENOTCONN) return 1;
    message.msg_control = (void *)1;
    if (syscall(SYS_sendmsg, fd, &message, MSG_NOSIGNAL) != -1 || errno != EFAULT) return 2;
    message.msg_control = control; message.msg_name = (void *)1; message.msg_namelen = 2;
    if (syscall(SYS_sendmsg, fd, &message, MSG_NOSIGNAL) != -1 || errno != EFAULT) return 3;
    close(fd);
    if (socketpair(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0, pair)
        || send(pair[0], "unread", 6, MSG_NOSIGNAL) != 6) return 4;
    close(pair[1]); message.msg_name = NULL; message.msg_namelen = 0;
    if (syscall(SYS_sendmsg, pair[0], &message, MSG_NOSIGNAL) != -1 || errno != ECONNRESET
        || syscall(SYS_sendmsg, pair[0], &message, MSG_NOSIGNAL) != -1 || errno != EINVAL) return 5;
    close(pair[0]);
    if (socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, pair)) return 6;
    struct sockaddr_in named = { .sin_family = AF_INET };
    struct iovec invalid = { .iov_base = (void *)1, .iov_len = 1 };
    message = (struct msghdr){ .msg_name = &named, .msg_namelen = sizeof named,
        .msg_iov = &invalid, .msg_iovlen = 1 };
    if (syscall(SYS_sendmsg, pair[0], &message, MSG_NOSIGNAL) != -1 || errno != EISCONN) return 7;
    message.msg_control = control; message.msg_controllen = sizeof control;
    if (syscall(SYS_sendmsg, pair[0], &message, MSG_NOSIGNAL) != -1 || errno != EINVAL) return 8;
    message.msg_name = (void *)1;
    if (syscall(SYS_sendmsg, pair[0], &message, MSG_NOSIGNAL) != -1 || errno != EFAULT) return 9;
    if (shutdown(pair[1], SHUT_RD)
        || send(pair[0], NULL, 0, MSG_NOSIGNAL) != 0
        || write(pair[0], NULL, 0) != 0) return 10;
    close(pair[1]);
    if (send(pair[0], NULL, 0, MSG_NOSIGNAL) != -1 || errno != EPIPE) return 11;
    struct iovec empty = { .iov_base = (void *)1, .iov_len = 0 };
    if (readv(pair[0], &empty, 1) || writev(pair[0], &empty, 1)) return 12;
    close(pair[0]);
    fd = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    if (fd < 0 || sendto(fd, NULL, 0, MSG_NOSIGNAL, (struct sockaddr *)&named, sizeof named) != -1
        || errno != EOPNOTSUPP || send(fd, NULL, 0, MSG_NOSIGNAL) != -1 || errno != ENOTCONN) return 13;
    close(fd); return 0;
}

static int unix_blocking_stream_prefix(int via_write, int timed)
{
    int pair[2], small = 0;
    const size_t length = timed ? 8192 : 100000;
    unsigned char *payload = malloc(length);
    if (!payload || socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, pair)
        || setsockopt(pair[0], SOL_SOCKET, SO_SNDBUF, &small, sizeof small)) return 1;
    memset(payload, 0x68, length);
    if (timed) {
        struct timeval timeout = { .tv_usec = 50000 };
        if (setsockopt(pair[0], SOL_SOCKET, SO_SNDTIMEO, &timeout, sizeof timeout)) return 2;
        ssize_t count = via_write ? write(pair[0], payload, length) : send(pair[0], payload, length, MSG_NOSIGNAL);
        if (count <= 0 || (size_t)count >= length) return 3;
        size_t read_count = 0; unsigned char output[97];
        while (read_count < (size_t)count) {
            ssize_t got = recv(pair[1], output, sizeof output, MSG_DONTWAIT);
            if (got <= 0 || read_count + (size_t)got > (size_t)count || memcmp(output, payload, (size_t)got)) return 4;
            read_count += (size_t)got;
        }
    } else {
        pid_t child = fork(); if (child < 0) return 5;
        if (!child) {
            close(pair[0]); usleep(30000);
            size_t done = 0; unsigned char output[997];
            while (done < length) {
                ssize_t count = read(pair[1], output, sizeof output);
                if (count <= 0 || done + (size_t)count > length || memcmp(output, payload, (size_t)count)) _exit(6);
                done += (size_t)count;
            }
            close(pair[1]); _exit(0);
        }
        close(pair[1]);
        ssize_t count = via_write ? write(pair[0], payload, length) : send(pair[0], payload, length, MSG_NOSIGNAL);
        // 若 implementation 在第一次 short write 返回，关闭 writer 让 child 得到 EOF，不能无限阻塞验证。
        close(pair[0]); int status;
        if (waitpid(child, &status, 0) != child || count != (ssize_t)length
            || !WIFEXITED(status) || WEXITSTATUS(status)) return 7;
        free(payload); return 0;
    }
    free(payload); close(pair[0]); close(pair[1]); return 0;
}

/* 原生generic socket ioctl使用OFD捕获NET；读不需权限，写在copy后重新检查NET_ADMIN。 */
static int socket_interface_permissions(void)
{
    const int domains[] = { AF_INET, AF_UNIX };
    for (size_t i = 0; i < sizeof domains / sizeof domains[0]; ++i) {
        int fd = socket(domains[i], SOCK_DGRAM | SOCK_CLOEXEC, 0);
        if (fd < 0) return 1;
        struct ifreq query;
        memset(&query, 0, sizeof query);
        strcpy(query.ifr_name, "eth0");
        if (ioctl(fd, SIOCGIFFLAGS, &query)) { close(fd); return 2; }
        pid_t child = fork();
        if (child < 0) { close(fd); return 3; }
        if (!child) {
            if (setresgid(1000, 1000, 1000) || setresuid(1000, 1000, 1000)) _exit(1);
            struct ifreq request;
            memset(&request, 0, sizeof request);
            strcpy(request.ifr_name, "eth0");
            if (ioctl(fd, SIOCGIFFLAGS, &request)) _exit(2);
            const unsigned long mutations[] = {
                SIOCSIFFLAGS, SIOCSIFADDR, SIOCSIFNETMASK, SIOCSIFBRDADDR, SIOCSIFMTU
            };
            for (size_t n = 0; n < sizeof mutations / sizeof mutations[0]; ++n) {
                int expected = domains[i] == AF_INET || mutations[n] == SIOCSIFFLAGS
                    || mutations[n] == SIOCSIFMTU ? EPERM : ENOTTY;
                if (ioctl(fd, mutations[n], &request) != -1 || errno != expected) _exit(3);
                strcpy(request.ifr_name, "missing-device");
                if (ioctl(fd, mutations[n], &request) != -1 || errno != expected) _exit(4);
                strcpy(request.ifr_name, "eth0");
                if (ioctl(fd, mutations[n], (void *)1) != -1 || errno != EFAULT) _exit(5);
            }
            struct rtentry route;
            memset(&route, 0, sizeof route);
            int expected = domains[i] == AF_INET ? EPERM : ENOTTY;
            if (ioctl(fd, SIOCADDRT, &route) != -1 || errno != expected) _exit(6);
            if (ioctl(fd, SIOCDELRT, &route) != -1 || errno != expected) _exit(7);
            if (ioctl(fd, SIOCADDRT, (void *)1) != -1 || errno != EFAULT) _exit(8);
            _exit(0);
        }
        int status;
        int result = waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status);
        close(fd);
        if (result) return 4;
    }
    int fd = socket(AF_INET, SOCK_DGRAM | SOCK_CLOEXEC, 0);
    if (fd < 0) return 5;
    pid_t child = fork();
    if (child < 0) { close(fd); return 6; }
    if (!child) {
        if (setresgid(1000, 1000, 1000) || setresuid(1000, 1000, 1000)) _exit(1);
        // 原生未绑定时第一次设置不需要NET_RAW；之后同名和解绑都需要。
        if (setsockopt(fd, SOL_SOCKET, SO_BINDTODEVICE, "eth0", 5)) _exit(2);
        if (setsockopt(fd, SOL_SOCKET, SO_BINDTODEVICE, "eth0", 5) != -1 || errno != EPERM) _exit(3);
        if (setsockopt(fd, SOL_SOCKET, SO_BINDTODEVICE, 0, 0) != -1 || errno != EPERM) _exit(4);
        if (setsockopt(fd, SOL_SOCKET, SO_BINDTODEVICE, "missing", 8) != -1 || errno != ENODEV) _exit(5);
        if (setsockopt(fd, SOL_SOCKET, SO_BINDTODEVICE, (void *)1, 5) != -1 || errno != EFAULT) _exit(6);
        // 第16byte不可读：固定源码最多读取15byte，再由NET owner按名字返回ENODEV。
        long page_size = sysconf(_SC_PAGESIZE);
        char *page = mmap(0, (size_t)page_size * 2, PROT_READ | PROT_WRITE,
            MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (page == MAP_FAILED || mprotect(page + page_size, (size_t)page_size, PROT_NONE)) _exit(7);
        memset(page + page_size - 15, 'x', 15);
        if (setsockopt(fd, SOL_SOCKET, SO_BINDTODEVICE, page + page_size - 15, 16) != -1 || errno != ENODEV) _exit(8);
        if (setsockopt(fd, SOL_SOCKET, SO_BINDTODEVICE, (void *)1, (socklen_t)-1) != -1 || errno != EINVAL) _exit(9);
        char name[IFNAMSIZ]; socklen_t length = sizeof name;
        if (getsockopt(fd, SOL_SOCKET, SO_BINDTODEVICE, name, &length) || length != 5 || strcmp(name, "eth0")) _exit(10);
        _exit(0);
    }
    int status;
    int result = waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status);
    close(fd);
    if (result) return 7;
    return 0;
}

static int verify_unix_rights(void)
{
    int interface = socket_interface_permissions();
    if (interface) { dprintf(2, "socket interface permission step=%d errno=%d\n", interface, errno); return 1; }
    int global_buffer = unix_global_buffer_options();
    if (global_buffer) { dprintf(2, "UNIX global buffer step=%d errno=%d\n", global_buffer, errno); return 1; }
    int protocol = unix_send_protocol_edges();
    if (protocol) { dprintf(2, "UNIX send protocol step=%d errno=%d\n", protocol, errno); return 1; }
    for (int via_write = 0; via_write < 2; ++via_write)
        for (int timed = 0; timed < 2; ++timed) {
            int result = unix_blocking_stream_prefix(via_write, timed);
            if (result) { dprintf(2, "UNIX blocking prefix write=%d timed=%d step=%d errno=%d\n", via_write, timed, result, errno); return 1; }
        }
    const int buffer_types[] = { SOCK_STREAM, SOCK_DGRAM, SOCK_SEQPACKET };
    for (size_t i = 0; i < sizeof buffer_types / sizeof buffer_types[0]; ++i) {
        int optmem = unix_optmem_options(buffer_types[i]);
        if (optmem) { dprintf(2, "UNIX optmem type=%d step=%d errno=%d\n", buffer_types[i], optmem, errno); return 1; }
        int result = unix_buffer_options(buffer_types[i]);
        if (result) { dprintf(2, "UNIX buffer options type=%d step=%d errno=%d\n", buffer_types[i], result, errno); return 1; }
    }
    const int packet_types[] = { SOCK_DGRAM, SOCK_SEQPACKET };
    for (size_t i = 0; i < sizeof packet_types / sizeof packet_types[0]; ++i) {
        int result = unix_packet_memory(packet_types[i]);
        if (result) { dprintf(2, "UNIX packet memory type=%d step=%d errno=%d\n", packet_types[i], result, errno); return 1; }
    }
    const int close_types[] = { SOCK_STREAM, SOCK_SEQPACKET };
    for (size_t i = 0; i < sizeof close_types / sizeof close_types[0]; ++i) {
        int result = unix_close_reset(close_types[i]);
        if (result) { dprintf(2, "UNIX close reset type=%d step=%d errno=%d\n", close_types[i], result, errno); return 1; }
    }
    int lifecycle = unix_stream_shutdown_drain();
    if (lifecycle) { dprintf(2, "UNIX stream shutdown step=%d errno=%d\n", lifecycle, errno); return 1; }
    lifecycle = unix_datagram_disconnect();
    if (lifecycle) { dprintf(2, "UNIX datagram disconnect step=%d errno=%d\n", lifecycle, errno); return 1; }
    lifecycle = unix_seqpacket_lifecycle();
    if (lifecycle) { dprintf(2, "UNIX seqpacket lifecycle step=%d errno=%d\n", lifecycle, errno); return 1; }
    lifecycle = unix_pathname_datagram();
    if (lifecycle) { dprintf(2, "UNIX pathname datagram step=%d errno=%d\n", lifecycle, errno); return 1; }
    const int credential_types[] = { SOCK_STREAM, SOCK_DGRAM, SOCK_SEQPACKET };
    for (size_t i = 0; i < sizeof credential_types / sizeof credential_types[0]; ++i) {
        int result = unix_message_credentials(credential_types[i]);
        if (result) { dprintf(2, "UNIX credentials type=%d step=%d errno=%d\n", credential_types[i], result, errno); return 1; }
    }
    int result = unix_real_sender_credentials();
    if (result) { dprintf(2, "UNIX real credentials step=%d errno=%d\n", result, errno); return 1; }
    if (socket_timeouts() || unix_blocking_connect() || unix_full_pathname() || unix_autobind() || unix_rights_roundtrip(SOCK_STREAM) || unix_rights_roundtrip(SOCK_DGRAM) || unix_rights_roundtrip(SOCK_SEQPACKET)
        || unix_rights_copyout_actor(SOCK_STREAM) || unix_rights_copyout_actor(SOCK_DGRAM) || unix_rights_copyout_actor(SOCK_SEQPACKET)
        || unix_accept_preparation() || unix_native_control_edges() || unix_closed_datagram_is_writable()) return 1;
    pid_t child = fork();
    if (child == 0) {
        int result = unix_rights_quota_actor();
        if (result) dprintf(2, "UNIX quota step=%d errno=%d\n", result, errno);
        _exit(result);
    }
    if (time_namespace_wait(child)) return 2;
    child = fork();
    if (child == 0) {
        int result = unix_rights_privileged_actor();
        if (result) dprintf(2, "UNIX privileged quota step=%d errno=%d\n", result, errno);
        _exit(result);
    }
    if (time_namespace_wait(child)) return 3;
    long heap_before = unix_rights_heap_kib();
    if (heap_before < 0) return 4;
    /* 关闭两个实际 descriptor 后保留互相 queued 的 socket capabilities，异步 worker 必须回收。 */
    for (size_t i = 0; i < 16; ++i) {
        int pair[2];
        if (socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, pair) < 0
            || unix_send_fds(pair[0], &pair[0], 1) != 1
            || unix_send_fds(pair[1], &pair[1], 1) != 1) return 4;
        close(pair[0]); close(pair[1]);
    }
    /* 16 对 stream 的 byte rings 超过 2 MiB；允许新 metadata/cache 容量增长 512 KiB，不能保留环。 */
    for (size_t attempt = 0; attempt < 512; ++attempt) {
        long current = unix_rights_heap_kib();
        if (current < 0) return 5;
        if (current <= heap_before + 512) return 0;
        nanosleep(&(struct timespec){ .tv_nsec = 20000000 }, NULL);
    }
    return 6;
}

static int tcp_closed_and_listener_shutdown(void)
{
    int fd = socket(AF_INET, SOCK_STREAM | SOCK_CLOEXEC | SOCK_NONBLOCK, 0);
    if (fd < 0) return 1;
    if (shutdown(fd, SHUT_RD) != -1 || errno != ENOTCONN) return 2;
    char byte;
    if (recv(fd, &byte, 1, MSG_DONTWAIT) != 0) return 3;
    struct pollfd observed = { .fd = fd, .events = POLLIN | POLLOUT | POLLRDHUP };
    if (poll(&observed, 1, 0) != 1
        || (observed.revents & (POLLIN | POLLOUT | POLLRDHUP | POLLHUP))
            != (POLLIN | POLLOUT | POLLRDHUP | POLLHUP)) return 4;
    struct sockaddr_in bound = { .sin_family = AF_INET };
    if (bind(fd, (struct sockaddr *)&bound, sizeof bound)) return 5;
    socklen_t length = sizeof bound;
    if (getsockname(fd, (struct sockaddr *)&bound, &length)
        || length != sizeof bound || !bound.sin_port || bound.sin_addr.s_addr) return 6;
    struct sockaddr disconnect = { .sa_family = AF_UNSPEC };
    if (connect(fd, &disconnect, sizeof disconnect)) return 7;
    if (recv(fd, &byte, 1, MSG_DONTWAIT) != -1 || errno != ENOTCONN) return 8;
    if (send(fd, &byte, 1, MSG_NOSIGNAL) != -1 || errno != EPIPE) return 9;
    if (listen(fd, 1) || shutdown(fd, SHUT_WR)) return 10;
    if (accept(fd, 0, 0) != -1 || errno != EAGAIN) return 11;
    if (shutdown(fd, SHUT_RD)) return 12;
    if (accept(fd, 0, 0) != -1 || errno != EINVAL) return 13;
    struct sockaddr_in after = {0}; length = sizeof after;
    if (getsockname(fd, (struct sockaddr *)&after, &length)
        || memcmp(&after, &bound, sizeof bound)) return 14;
    if (listen(fd, 1) || connect(fd, &disconnect, sizeof disconnect)) return 15;
    close(fd);
    return 0;
}

static int robust_wait_zombie(pid_t child)
{
    char path[96], status[2048];
    snprintf(path, sizeof path, "/tmp/pid-proc/%d/status", child);
    struct timespec start, now;
    if (clock_gettime(CLOCK_MONOTONIC, &start)) return 1;
    for (;;) {
        if (pid_proc_read(path, status, sizeof status) >= 0 && strstr(status, "State:\tZ")) return 0;
        if (clock_gettime(CLOCK_MONOTONIC, &now) || now.tv_sec - start.tv_sec >= 2) return 1;
        sched_yield();
    }
}

static int robust_real_credentials_actor(void)
{
    struct robust_list_head *head = mmap(NULL, 4096, PROT_READ | PROT_WRITE,
        MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (head == MAP_FAILED) return 1;
    head->list.next = &head->list;
    int ready[2], release[2];
    if (pipe(ready) || pipe(release)) return 2;
    pid_t child = fork();
    if (child < 0) return 3;
    if (!child) {
        close(ready[0]); close(release[1]);
        if (setresgid(1000, 1000, 1000) || setresuid(1000, 1000, 1000)
            || prctl(PR_SET_DUMPABLE, 1)
            || syscall(SYS_set_robust_list, head, sizeof *head)
            || write(ready[1], "R", 1) != 1) _exit(4);
        char byte;
        _exit(read(release[0], &byte, 1) != 1 ? 5 : 0);
    }
    close(ready[1]); close(release[0]);
    char byte;
    int failed = read(ready[0], &byte, 1) != 1 || byte != 'R';
    close(ready[0]);
    struct { uint32_t version; int32_t pid; } header = { UINT32_C(0x20080522), 0 };
    struct { uint32_t effective, permitted, inheritable; } caps[2];
    if (!failed && (syscall(SYS_setfsgid, 1000) != 0 || syscall(SYS_setfsuid, 1000) != 0
        || syscall(SYS_capget, &header, caps))) failed = 6;
    /* 移除 effective SYS_PTRACE；permitted 留存。FS 匹配不能授权 REALCREDS query。 */
    if (!failed) { caps[0].effective &= ~(UINT32_C(1) << 19); if (syscall(SYS_capset, &header, caps)) failed = 7; }
    struct robust_list_head *observed = NULL; size_t length = 0;
    if (!failed && (syscall(SYS_get_robust_list, child, &observed, &length) != -1
        || errno != EPERM)) failed = 8;
    if (!failed && (syscall(SYS_setfsuid, 0) != 1000 || syscall(SYS_setfsgid, 0) != 1000
        || setresgid(1000, -1, -1) || setresuid(1000, -1, -1))) failed = 9;
    /* REAL 匹配且 target permitted 是 caller permitted 子集；FS 不匹配也应成功。 */
    if (!failed && (syscall(SYS_get_robust_list, child, &observed, &length)
        || observed != head || length != sizeof *head)) failed = 10;
    if (write(release[1], "X", 1) != 1 && !failed) failed = 11;
    close(release[1]);
    if (!failed && (robust_wait_zombie(child)
        || syscall(SYS_get_robust_list, child, &observed, &length)
        || observed || length != sizeof *head)) failed = 12;
    if (failed) kill(child, SIGKILL);
    if (time_namespace_wait(child) && !failed) failed = 13;
    if (!failed && (syscall(SYS_get_robust_list, child, &observed, &length) != -1
        || errno != ESRCH)) failed = 14;
    if (!failed) {
        child = fork();
        if (child < 0) failed = 15;
        else if (!child) {
            if (setresgid(1000, 1000, 1000) || setresuid(1000, 1000, 1000)
                || prctl(PR_SET_DUMPABLE, 0)) _exit(16);
            _exit(0);
        } else {
            if (robust_wait_zombie(child)
                || syscall(SYS_get_robust_list, child, &observed, &length) != -1
                || errno != EPERM) failed = 17;
            if (failed) kill(child, SIGKILL);
            if (time_namespace_wait(child) && !failed) failed = 18;
        }
    }
    munmap(head, 4096);
    return failed;
}

static int verify_pid_timer_thread_identity(void)
{
    const int signal = SIGRTMIN + 2;
    sigset_t blocked, old;
    sigemptyset(&blocked); sigaddset(&blocked, signal);
    if (sigprocmask(SIG_BLOCK, &blocked, &old)) return 1;
    struct sigevent event = { .sigev_notify = SIGEV_THREAD_ID, .sigev_signo = signal,
        .sigev_value.sival_ptr = (void *)(uintptr_t)97 };
    int tid = syscall(SYS_gettid), id;
    memcpy((unsigned char *)&event + 16, &tid, sizeof tid);
    if (tid != 1 || syscall(SYS_timer_create, CLOCK_MONOTONIC, &event, &id)) return 2;
    struct itimerspec once = { .it_value.tv_nsec = 1000000 };
    if (syscall(SYS_timer_settime, id, 0, &once, 0)) return 3;
    uint64_t mask = UINT64_C(1) << (signal - 1);
    siginfo_t info;
    struct timespec timeout = { .tv_sec = 1 };
    if (syscall(SYS_rt_sigtimedwait, &mask, &info, &timeout, 8) != signal
        || info.si_code != SI_TIMER || info.si_timerid != id
        || (uintptr_t)info.si_value.sival_ptr != 97 || syscall(SYS_timer_delete, id)) return 4;
    return sigprocmask(SIG_SETMASK, &old, 0) ? 5 : 0;
}

#ifndef __WNOTHREAD
#define __WNOTHREAD 0x20000000
#endif
#ifndef __WALL
#define __WALL 0x40000000
#endif
#ifndef __WCLONE
#define __WCLONE 0x80000000
#endif

static int clone_wait_status(pid_t child, int options, int code)
{
    int status = -1;
    return waitpid(child, &status, options) != child || !WIFEXITED(status) || WEXITSTATUS(status) != code;
}

/* 真实 parent exec image；继承的标准 FD 释放 child，SIGCHLD 通知不改变其 __WCLONE 类别。 */
int verify_clone_parent_exec_image(pid_t child, int release)
{
    sigset_t wanted; sigemptyset(&wanted); sigaddset(&wanted, SIGCHLD);
    siginfo_t info = {0}; struct timespec timeout = {2, 0};
    if (write(release, "X", 1) != 1 || sigtimedwait(&wanted, &info, &timeout) != SIGCHLD
        || info.si_pid != child || info.si_code != CLD_EXITED || info.si_status != 32
        || waitpid(child, NULL, WNOHANG) != -1 || errno != ECHILD
        || clone_wait_status(child, __WCLONE, 32)) return 33;
    close(release);
    return 0;
}

static int clone_parent_exec_actor(void)
{
    sigset_t signals; sigemptyset(&signals); sigaddset(&signals, SIGCHLD); sigaddset(&signals, SIGUSR1);
    if (sigprocmask(SIG_BLOCK, &signals, NULL)) return 34;
    int release[2]; if (pipe(release)) return 35;
    pid_t child = syscall(SYS_clone, SIGUSR1, 0, 0, 0, 0);
    if (!child) {
        close(release[1]); char byte;
        _exit(read(release[0], &byte, 1) == 1 ? 32 : 36);
    }
    if (child <= 0) return 37;
    close(release[0]);
    char pid_text[32], fd_text[32];
    snprintf(pid_text, sizeof pid_text, "%d", child); snprintf(fd_text, sizeof fd_text, "%d", release[1]);
    execl("/bin/init", "init", "clone-parent-exec-image", pid_text, fd_text, (char *)0);
    return 38;
}

static int verify_clone_orphan_adoption(void)
{
    int ready[2], release[2]; if (pipe(ready) || pipe(release)) return 1;
    pid_t creator = fork();
    if (!creator) {
        pid_t child = syscall(SYS_clone, SIGUSR1, 0, 0, 0, 0);
        if (!child) {
            close(ready[0]); close(ready[1]); close(release[1]);
            char byte; _exit(read(release[0], &byte, 1) == 1 && getppid() == 1 ? 39 : 40);
        }
        _exit(child <= 0 || write(ready[1], &child, sizeof child) != sizeof child ? 41 : 0);
    }
    close(ready[1]); close(release[0]);
    pid_t child;
    if (creator <= 0 || read(ready[0], &child, sizeof child) != sizeof child
        || clone_wait_status(creator, 0, 0) || write(release[1], "X", 1) != 1
        || waitpid(child, NULL, __WCLONE | WNOHANG) != -1 || errno != ECHILD
        || clone_wait_status(child, 0, 39)) return 2;
    close(ready[0]); close(release[1]);
    return 0;
}

struct clone_creator_test { int ready[2], child_release[2], creator_release[2]; int result; };
static void *clone_wait_creator(void *argument)
{
    struct clone_creator_test *test = argument;
    pid_t child = syscall(SYS_clone, SIGCHLD, 0, 0, 0, 0);
    if (!child) {
        char byte;
        _exit(read(test->child_release[0], &byte, 1) == 1 ? 25 : 26);
    }
    char byte;
    test->result = child <= 0 || write(test->ready[1], &child, sizeof child) != sizeof child
        || read(test->creator_release[0], &byte, 1) != 1;
    return NULL;
}

struct clone_parent_test { pid_t expected_parent; int ready; };
static int clone_parent_sibling(void *argument)
{
    struct clone_parent_test *test = argument;
    pid_t pid = getpid();
    return getppid() != test->expected_parent || write(test->ready, &pid, sizeof pid) != sizeof pid ? 28 : 27;
}

static int verify_clone_wait_relations(void)
{
    sigset_t signals, original;
    sigemptyset(&signals); sigaddset(&signals, SIGUSR1); sigaddset(&signals, SIGRTMIN + 3);
    if (sigprocmask(SIG_BLOCK, &signals, &original)) return 1;
    /* SIGCHLD 的 IGN/NOCLDWAIT 不能 autoreap 非 SIGCHLD 和零信号 child。 */
    struct sigaction ignored = { .sa_handler = SIG_IGN, .sa_flags = SA_NOCLDWAIT }, old;
    if (sigaction(SIGCHLD, &ignored, &old)) return 2;
    const int notification[] = { 0, SIGUSR1, SIGRTMIN + 3 };
    for (unsigned int index = 0; index < sizeof notification / sizeof notification[0]; ++index) {
        pid_t child = syscall(SYS_clone, notification[index], 0, 0, 0, 0);
        if (!child) _exit(20 + (int)index);
        if (child <= 0 || waitpid(child, NULL, WNOHANG) != -1 || errno != ECHILD) return 3;
        if (notification[index]) {
            sigset_t wanted; sigemptyset(&wanted); sigaddset(&wanted, notification[index]);
            siginfo_t info = {0}; struct timespec timeout = {2, 0};
            if (sigtimedwait(&wanted, &info, &timeout) != notification[index]
                || info.si_signo != notification[index] || info.si_code != CLD_EXITED
                || info.si_pid != child || info.si_status != 20 + (int)index) return 4;
        }
        if (clone_wait_status(child, index == 1 ? __WALL | __WCLONE : __WCLONE, 20 + (int)index)) return 5;
    }
    if (sigaction(SIGCHLD, &old, NULL)) return 6;
    pid_t ordinary = fork();
    if (!ordinary) _exit(24);
    if (ordinary <= 0 || waitpid(ordinary, NULL, __WCLONE | WNOHANG) != -1 || errno != ECHILD
        || clone_wait_status(ordinary, __WALL | __WCLONE, 24)) return 7;
    /* Creator Thread 仍存活时 __WNOTHREAD 不得观察 sibling 的 child。 */
    struct clone_creator_test test = {0}; pthread_t thread;
    if (pipe(test.ready) || pipe(test.child_release) || pipe(test.creator_release)
        || pthread_create(&thread, NULL, clone_wait_creator, &test)) return 8;
    pid_t child;
    if (read(test.ready[0], &child, sizeof child) != sizeof child || child <= 0
        || waitpid(child, NULL, __WNOTHREAD | __WALL | WNOHANG) != -1 || errno != ECHILD
        || waitpid(child, NULL, WNOHANG) != 0 || write(test.child_release[1], "X", 1) != 1
        || clone_wait_status(child, 0, 25) || write(test.creator_release[1], "X", 1) != 1
        || pthread_join(thread, NULL) || test.result) return 9;
    for (int index = 0; index < 2; ++index) {
        close(test.ready[index]); close(test.child_release[index]); close(test.creator_release[index]);
    }
    /* CLONE_PARENT 新 sibling 的 real parent/exit signal 继承 caller relation，资源仍从 caller 取。 */
    int ready[2], release[2];
    if (pipe(ready) || pipe(release)) return 10;
    pid_t parent = getpid(), creator = fork();
    if (!creator) {
        void *stack = mmap(NULL, 65536, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        struct clone_parent_test sibling = { .expected_parent = parent, .ready = ready[1] };
        pid_t child = stack == MAP_FAILED ? -1 : clone(clone_parent_sibling, (char *)stack + 65536,
            CLONE_PARENT | SIGUSR1, &sibling);
        if (child <= 0 || waitpid(child, NULL, __WALL | WNOHANG) != -1 || errno != ECHILD) _exit(29);
        char byte; _exit(read(release[0], &byte, 1) == 1 ? 0 : 30);
    }
    close(ready[1]); close(release[0]);
    if (creator <= 0 || read(ready[0], &child, sizeof child) != sizeof child
        || waitpid(child, NULL, __WCLONE | WNOHANG) != -1 || errno != ECHILD
        || clone_wait_status(child, 0, 27) || write(release[1], "X", 1) != 1
        || clone_wait_status(creator, 0, 0)) return 11;
    close(ready[0]); close(release[1]);
    /* child exec 后自身 exit_signal 必须恢复 SIGCHLD。 */
    child = syscall(SYS_clone, SIGUSR1, 0, 0, 0, 0);
    if (!child) { execl("/bin/init", "init", "clone-exit-image", (char *)0); _exit(127); }
    if (child <= 0 || robust_wait_zombie(child)
        || waitpid(child, NULL, __WCLONE | WNOHANG) != -1 || errno != ECHILD
        || clone_wait_status(child, 0, 31)) return 12;
    child = fork();
    if (!child) _exit(clone_parent_exec_actor());
    if (clone_wait_status(child, 0, 0)) return 14;
    if (syscall(SYS_clone, 65, 0, 0, 0, 0) != -1 || errno != EINVAL) return 15;
    if (sigprocmask(SIG_SETMASK, &original, NULL)) return 13;
    return 0;
}

static int pid_namespace_image(int ancestor_peer)
{
    if (getpid() != 1 || syscall(SYS_gettid) != 1 || getppid() != 0
        || getpgid(0) != 1 || getsid(0) != 1 || getpgid(1) != 1 || getsid(1) != 1) return 1;
    if (syscall(SYS_getpgid, UINT64_C(0x100000001)) != 1
        || setpgid(-1, 0) != -1 || errno != EINVAL
        || getpgid(-1) != -1 || errno != ESRCH) return 2;
    struct ucred credentials;
    socklen_t size = sizeof credentials;
    if (getsockopt(ancestor_peer, SOL_SOCKET, SO_PEERCRED, &credentials, &size) < 0
        || credentials.pid != 0) return 3;
    close(ancestor_peer);
    int own[2];
    if (socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, own) < 0) return 4;
    size = sizeof credentials;
    if (getsockopt(own[0], SOL_SOCKET, SO_PEERCRED, &credentials, &size) < 0
        || credentials.pid != 1) return 5;
    close(own[0]); close(own[1]);
    struct { uint32_t version; int32_t pid; } cap_header = { 0x20080522, 1 };
    uint32_t cap_data[6];
    if (syscall(SYS_capget, &cap_header, cap_data) < 0
        || syscall(SYS_capset, &cap_header, cap_data) < 0) return 19;
    int unconnected = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    size = sizeof credentials;
    if (unconnected < 0 || getsockopt(unconnected, SOL_SOCKET, SO_PEERCRED, &credentials, &size) < 0
        || credentials.pid != -1 || credentials.uid != (uid_t)-1 || credentials.gid != (gid_t)-1) return 21;
    struct sockaddr_un address = { .sun_family = AF_UNIX };
    const char abstract_name[] = "liteos-pid-peercred";
    memcpy(address.sun_path + 1, abstract_name, sizeof abstract_name);
    socklen_t address_size = sizeof address.sun_family + 1 + sizeof abstract_name;
    if (bind(unconnected, (struct sockaddr *)&address, address_size) < 0
        || syscall(SYS_setresuid, -1, 1001, -1) < 0 || listen(unconnected, 1) < 0
        || syscall(SYS_setresuid, -1, 0, -1) < 0) return 22;
    int client = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    size = sizeof credentials;
    if (client < 0 || connect(client, (struct sockaddr *)&address, address_size) < 0
        || getsockopt(client, SOL_SOCKET, SO_PEERCRED, &credentials, &size) < 0
        || credentials.pid != 1 || credentials.uid != 1001) return 23;
    int accepted = accept4(unconnected, NULL, NULL, SOCK_CLOEXEC);
    size = sizeof credentials;
    if (accepted < 0 || getsockopt(accepted, SOL_SOCKET, SO_PEERCRED, &credentials, &size) < 0
        || credentials.pid != 1 || credentials.uid != 0) return 24;
    close(accepted); close(client);
    size = sizeof credentials;
    if (listen(unconnected, 2) < 0 || getsockopt(unconnected, SOL_SOCKET, SO_PEERCRED, &credentials, &size) < 0
        || credentials.uid != 0) return 25;
    close(unconnected);
    /* 同 namespace 对 init 的默认 KILL/STOP/USR1 都被忽略，不留下挂起请求。 */
    if (kill(1, SIGKILL) < 0 || kill(1, SIGSTOP) < 0 || kill(1, SIGUSR1) < 0) return 6;
    if (unshare(CLONE_NEWNS) < 0 || mkdir("/tmp/pid-proc", 0700) < 0
        || mount(NULL, "/tmp/pid-proc", "proc", 0, NULL) < 0) return 7;
    char bytes[2048];
    if (pid_proc_read("/tmp/pid-proc/self/status", bytes, sizeof bytes) < 0
        || !strstr(bytes, "NStgid:\t1\n") || !strstr(bytes, "NSpid:\t1\n")) return 8;
    int maximum = open("/tmp/pid-proc/sys/kernel/pid_max", O_RDWR | O_CLOEXEC);
    if (maximum < 0 || write(maximum, "4096\n", 5) != 5
        || lseek(maximum, 0, SEEK_SET) != 0 || read(maximum, bytes, sizeof bytes) < 5
        || memcmp(bytes, "4096\n", 5)) return 9;
    close(maximum);
    int completion[2];
    if (pipe(completion) < 0) return 10;
    pid_t worker = fork();
    if (worker < 0) return 11;
    if (worker == 0) { close(completion[0]); _exit(37); }
    close(completion[1]);
    if (worker != 2 || read(completion[0], bytes, 1) != 0) return 12;
    close(completion[0]);
    /* exit_files 先于 zombie publication，观察实际 Z 作为屏障；截止时间只用于诊断死锁。 */
    struct timespec start, now;
    if (clock_gettime(CLOCK_MONOTONIC, &start) < 0) return 13;
    for (;;) {
        if (pid_proc_read("/tmp/pid-proc/2/status", bytes, sizeof bytes) >= 0
            && strstr(bytes, "State:\tZ")) break;
        if (clock_gettime(CLOCK_MONOTONIC, &now) < 0 || now.tv_sec - start.tv_sec >= 2) return 14;
        sched_yield();
    }
    if (!strstr(bytes, "Threads:\t1\n") || kill(worker, 0) < 0
        || kill(worker, SIGUSR1) < 0) return 15;
    cap_header.pid = worker;
    if (syscall(SYS_capget, &cap_header, cap_data) < 0) return 20;
    struct rlimit limit;
    if (prlimit(worker, RLIMIT_NOFILE, NULL, &limit) < 0
        || pid_proc_read("/tmp/pid-proc/2/task/2/stat", bytes, sizeof bytes) < 0
        || strncmp(bytes, "2 (", 3)) return 16;
    int status;
    if (waitpid(worker, &status, 0) != worker || !WIFEXITED(status) || WEXITSTATUS(status) != 37
        || open("/tmp/pid-proc/2/status", O_RDONLY) != -1 || errno != ENOENT) return 17;
    if (verify_pid_timer_thread_identity()) return 36;
    if (verify_clone3_namespace()) return 31;
    if (verify_proc_thread_entries()) return 32;
    if (verify_sleep_restart()) return 37;
    if (verify_tty_control()) return 33;
    if (verify_ipc_messages()) return 34;
    if (verify_unix_rights()) return 35;
    int tcp_shutdown = tcp_closed_and_listener_shutdown();
    if (tcp_shutdown) { dprintf(2, "TCP shutdown step=%d errno=%d\n", tcp_shutdown, errno); return 38; }
    worker = fork();
    if (worker < 0) return 39;
    if (!worker) {
        int result = robust_real_credentials_actor();
        if (result) dprintf(2, "robust REALCREDS step=%d errno=%d\n", result, errno);
        _exit(result);
    }
    if (time_namespace_wait(worker)) return 39;
    if (syscall(SYS_clone, CLONE_PARENT, 0, 0, 0, 0) != -1 || errno != EINVAL) return 40;
    worker = fork();
    if (worker < 0) return 40;
    if (!worker) {
        int result = verify_clone_wait_relations();
        if (result) dprintf(2, "clone/wait relation step=%d errno=%d\n", result, errno);
        _exit(result);
    }
    if (time_namespace_wait(worker)) return 40;
    if (verify_clone_orphan_adoption()) return 41;
    worker = fork();
    if (worker < 0) return 42;
    if (!worker) {
        int result = verify_pidfds("/tmp/pid-proc");
        if (result) dprintf(2, "pidfd step=%d errno=%d\n", result, errno);
        _exit(result);
    }
    if (time_namespace_wait(worker)) return 42;
    worker = fork();
    if (worker < 0) return 18;
    if (worker == 0) { for (;;) pause(); }
    /* init 正常退出必须杀死、回收实际 namespace members；observer mount/nsfs 不阻止完成。 */
    return 0;
}
static int pid_namespace_parent(void)
{
    int initial = open("/proc/self/ns/pid", O_RDONLY | O_CLOEXEC);
    int maximum = open("/proc/sys/kernel/pid_max", O_RDONLY | O_CLOEXEC);
    char before[32], after[32];
    ssize_t length = maximum < 0 ? -1 : read(maximum, before, sizeof before);
    if (initial < 0 || length <= 0) return 1;
    int pair[2];
    if (socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, pair) < 0
        || unshare(CLONE_NEWPID) < 0) return 2;
    int pending = open("/proc/self/ns/pid_for_children", O_RDONLY | O_CLOEXEC);
    struct stat old_stat, new_stat;
    if (pending < 0 || fstat(initial, &old_stat) < 0 || fstat(pending, &new_stat) < 0
        || old_stat.st_ino == new_stat.st_ino || ioctl(pending, NS_GET_NSTYPE) != CLONE_NEWPID) return 3;
    pid_t child = fork();
    if (child == 0) {
        close(pair[1]);
        int result = pid_namespace_image(pair[0]);
        if (result) dprintf(2, "pid namespace image step=%d errno=%d\n", result, errno);
        _exit(result);
    }
    close(pair[0]); close(pair[1]);
    int status = -1;
    if (child <= 1 || waitpid(child, &status, 0) != child || !WIFEXITED(status)
        || WEXITSTATUS(status)) {
        dprintf(2, "pid namespace init child=%d status=%d errno=%d\n", child, status, errno);
        return 4;
    }
    if (lseek(maximum, 0, SEEK_SET) != 0 || read(maximum, after, sizeof after) != length
        || memcmp(before, after, (size_t)length)) return 5;
    errno = 0;
    if (fork() != -1 || errno != ENOMEM) return 6;
    if (setns(initial, CLONE_NEWPID) < 0) return 7;
    child = fork();
    if (child == 0) _exit(getpid() > 1 ? 0 : 1);
    if (time_namespace_wait(child)) return 8;
    close(initial); close(pending); close(maximum);
    return 0;
}
static int verify_pid_namespaces(void)
{
    pid_t child = fork();
    if (child == 0) {
        int result = pid_namespace_parent();
        if (result) dprintf(2, "pid namespace parent step=%d errno=%d\n", result, errno);
        _exit(result);
    }
    return time_namespace_wait(child);
}
