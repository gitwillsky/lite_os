#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/filter.h>
#include <linux/seccomp.h>
#include <stdint.h>
#include <stddef.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

/* 独立 Linux UAPI consumer；不使用 kernel 私有布局或按自身 codec 做 roundtrip。 */
_Static_assert(sizeof(struct seccomp_data) == 64, "Linux seccomp_data");
_Static_assert(sizeof(struct seccomp_notif) == 80, "Linux seccomp_notif");
_Static_assert(offsetof(struct seccomp_notif, data) == 16, "Linux notification data offset");
_Static_assert(sizeof(struct seccomp_notif_resp) == 24, "Linux seccomp_notif_resp");
_Static_assert(sizeof(struct seccomp_notif_addfd) == 24, "Linux seccomp_notif_addfd");

static int listener(void)
{
    struct sock_filter instructions[] = {
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, 0),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_getppid, 0, 1),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_USER_NOTIF),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
    };
    struct sock_fprog filter = { .len = 4, .filter = instructions };
    return syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER,
        SECCOMP_FILTER_FLAG_NEW_LISTENER | SECCOMP_FILTER_FLAG_WAIT_KILLABLE_RECV
        | SECCOMP_FILTER_FLAG_LOG | SECCOMP_FILTER_FLAG_SPEC_ALLOW, &filter);
}

static int child_requests(int observer, int source, int writer, pid_t parent)
{
    if (close(observer) || close(source) || close(writer)) return 20;
    if (syscall(SYS_getppid) != 4321) return 21;
    if (syscall(SYS_getppid) != parent) return 22;
    errno = 0;
    if (syscall(SYS_getppid) != -1 || errno != EACCES) return 23;
    int injected = syscall(SYS_getppid);
    if (injected < 0 || !(fcntl(injected, F_GETFD) & FD_CLOEXEC)) return 24;
    char value = 0;
    if (read(injected, &value, 1) != 1 || value != 'N' || close(injected)) return 25;
    errno = 0;
    return syscall(SYS_getppid) == -1 && errno == ENOSYS ? 0 : 26;
}

int verify_seccomp_notifications(void)
{
    struct seccomp_notif_sizes sizes = {0};
    if (syscall(SYS_seccomp, SECCOMP_GET_NOTIF_SIZES, 0, &sizes)
        || sizes.seccomp_notif != sizeof(struct seccomp_notif)
        || sizes.seccomp_notif_resp != sizeof(struct seccomp_notif_resp)
        || sizes.seccomp_data != sizeof(struct seccomp_data)) return 1;
    if (prctl(PR_SET_NO_NEW_PRIVS, 1UL, 0UL, 0UL, 0UL)) return 2;
    int data[2];
    if (pipe(data) || write(data[1], "N", 1) != 1) return 3;
    int observer = listener();
    if (observer < 0 || !(fcntl(observer, F_GETFD) & FD_CLOEXEC)) return 4;
    errno = 0;
    int duplicate = listener();
    if (duplicate != -1 || errno != EBUSY) return 5;
    if (ioctl(observer, SECCOMP_IOCTL_NOTIF_SET_FLAGS,
              (unsigned long)SECCOMP_USER_NOTIF_FD_SYNC_WAKE_UP)) return 6;
    pid_t parent = getpid();
    pid_t child = fork();
    if (child == 0) _exit(child_requests(observer, data[0], data[1], parent));
    if (child < 0) return 7;
    for (int stage = 0; stage < 5; stage++) {
        struct seccomp_notif request = {0};
        if (ioctl(observer, SECCOMP_IOCTL_NOTIF_RECV, &request)
            || request.pid != (uint32_t)child || request.flags
            || request.data.nr != SYS_getppid || !request.data.instruction_pointer) return 8;
        uint64_t id = request.id;
        if (ioctl(observer, SECCOMP_IOCTL_NOTIF_ID_VALID, &id)) return 9;
        if (stage == 3) {
            struct seccomp_notif_addfd addfd = {
                .id = id, .flags = SECCOMP_ADDFD_FLAG_SEND,
                .srcfd = data[0], .newfd_flags = O_CLOEXEC,
            };
            if (ioctl(observer, SECCOMP_IOCTL_NOTIF_ADDFD, &addfd) < 0) return 10;
        } else if (stage == 4) {
            if (close(observer)) return 11;
        } else {
            struct seccomp_notif_resp response = { .id = id };
            if (stage == 0) response.val = 4321;
            if (stage == 1) response.flags = SECCOMP_USER_NOTIF_FLAG_CONTINUE;
            if (stage == 2) response.error = -EACCES;
            if (ioctl(observer, SECCOMP_IOCTL_NOTIF_SEND, &response)) return 12;
        }
    }
    int status;
    if (waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status)) return 13;
    return close(data[0]) || close(data[1]) ? 14 : 0;
}
