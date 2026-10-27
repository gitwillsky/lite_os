#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <stdatomic.h>
#include <sys/prctl.h>
#include <linux/capability.h>
#include <signal.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/ptrace.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>

#define CHECK(test) do { if (!(test)) { dprintf(2, "ptrace line=%d errno=%d\n", __LINE__, errno); return __LINE__; } } while (0)
static volatile sig_atomic_t delivered;
static volatile sig_atomic_t delivered_code, delivered_pid, delivered_value;
static void delivered_signal(int signal, siginfo_t *info, void *context)
{
    (void)context;
    delivered = signal; delivered_code = info->si_code; delivered_pid = info->si_pid;
    delivered_value = info->si_value.sival_int;
}

static int trace_me(void)
{
    unsigned long *page = mmap(0, 4096, PROT_READ|PROT_WRITE, MAP_PRIVATE|MAP_ANONYMOUS, -1, 0);
    CHECK(page != MAP_FAILED);
    page[0] = 0x123456;
    CHECK(mprotect(page, 4096, PROT_READ|PROT_EXEC) == 0);
    unsigned long *none = mmap(0, 4096, PROT_READ|PROT_WRITE, MAP_PRIVATE|MAP_ANONYMOUS, -1, 0);
    CHECK(none != MAP_FAILED); none[0] = 0x55; CHECK(mprotect(none, 4096, PROT_NONE) == 0);
    pid_t child = fork();
    CHECK(child >= 0);
    if (!child) {
        signal(SIGUSR1, SIG_IGN);
        struct sigaction action = { .sa_sigaction = delivered_signal, .sa_flags = SA_SIGINFO };
        sigemptyset(&action.sa_mask);
        if (sigaction(SIGUSR2, &action, 0)) _exit(14);
        if (ptrace(PTRACE_TRACEME, 0, 0, 0)) _exit(10);
        raise(SIGSTOP);
        if (page[0] != 0xabcdef || mprotect(none, 4096, PROT_READ) || none[0] != 0x99) _exit(11);
        raise(SIGUSR1);
        if (delivered) _exit(12);
        raise(SIGSTOP);
        _exit(delivered == SIGUSR2 && delivered_code == SI_USER && delivered_pid == getppid() ? 0 : 13);
    }
    int status;
    siginfo_t observation;
    CHECK(waitid(P_PID, child, &observation, WSTOPPED|WNOWAIT) == 0 && observation.si_code == CLD_TRAPPED && observation.si_pid == child);
    CHECK(waitpid(child, &status, 0) == child && WIFSTOPPED(status) && WSTOPSIG(status) == SIGSTOP);
    siginfo_t info;
    CHECK(ptrace(PTRACE_GETSIGINFO, child, 0, &info) == 0 && info.si_signo == SIGSTOP);
    errno = 0;
    CHECK(ptrace(PTRACE_PEEKDATA, child, page, 0) == 0x123456 && !errno);
    CHECK(ptrace(PTRACE_POKETEXT, child, page, 0xabcdefUL) == 0);
    CHECK(ptrace(PTRACE_PEEKDATA, child, none, 0) == 0x55);
    CHECK(ptrace(PTRACE_POKEDATA, child, none, 0x99UL) == 0);
    CHECK(mprotect(none, 4096, PROT_READ) == 0 && none[0] == 0x55);
    CHECK(ptrace(PTRACE_PEEKDATA, child, 0, 0) == -1 && errno == EIO);
    CHECK(syscall(SYS_ptrace, PTRACE_PEEKDATA, child, page, 0) == -1 && errno == EFAULT);
    CHECK(page[0] == 0x123456); /* 目标 RX page 必须 COW，不能污染 parent。 */
    unsigned long registers[34] = {0};
    struct iovec iov = {registers, sizeof registers};
    CHECK(ptrace(PTRACE_GETREGSET, child, (void *)1, &iov) == 0);
#if defined(__aarch64__)
    CHECK(iov.iov_len == 272 && registers[32] != 0);
    unsigned long pstate = registers[33];
    registers[33] |= 0x4;
    CHECK(ptrace(PTRACE_SETREGSET, child, (void *)1, &iov) == -1 && errno == EINVAL);
    registers[33] = pstate;
#else
    CHECK(iov.iov_len == 256 && registers[0] != 0);
#endif
    CHECK(ptrace(PTRACE_SETREGSET, child, (void *)1, &iov) == 0);
    /* iov_len 大于实际 regset 时只访问 native 长度，不能读取相邻 PROT_NONE guard。 */
    char *regspace = mmap(0, 8192, PROT_READ|PROT_WRITE, MAP_PRIVATE|MAP_ANONYMOUS, -1, 0);
    CHECK(regspace != MAP_FAILED && mprotect(regspace + 4096, 4096, PROT_NONE) == 0);
    size_t native = iov.iov_len;
    void *regbase = regspace + 4096 - native;
    memcpy(regbase, registers, native);
    struct iovec bounded = {regbase, native + 8};
    CHECK(ptrace(PTRACE_SETREGSET, child, (void *)1, &bounded) == 0 && bounded.iov_len == native);
    CHECK(munmap(regspace, 8192) == 0);
    CHECK(ptrace(PTRACE_CONT, child, 0, 0) == 0);
    CHECK(waitpid(child, &status, 0) == child && WIFSTOPPED(status) && WSTOPSIG(status) == SIGUSR1);
    CHECK(ptrace(PTRACE_CONT, child, 0, 0) == 0);
    CHECK(waitpid(child, &status, 0) == child && WIFSTOPPED(status) && WSTOPSIG(status) == SIGSTOP);
    CHECK(ptrace(PTRACE_DETACH, child, 0, SIGUSR2) == 0);
    CHECK(waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0);
    CHECK(waitpid(child, &status, WNOHANG) == -1 && errno == ECHILD);
    CHECK(munmap(page, 4096) == 0 && munmap(none, 4096) == 0);
    return 0;
}

/* si_signo 与 stop signal 独立；blocked injection 必须保留 SETSIGINFO 并进入真实 pending。 */
static int blocked_signal_info(void)
{
    pid_t child = fork(); CHECK(child >= 0);
    if (!child) {
        delivered = 0;
        struct sigaction action = { .sa_sigaction = delivered_signal, .sa_flags = SA_SIGINFO };
        sigset_t blocked, pending; sigemptyset(&blocked); sigaddset(&blocked, SIGUSR2);
        sigemptyset(&action.sa_mask);
        if (sigaction(SIGUSR2, &action, 0) || sigprocmask(SIG_BLOCK, &blocked, 0)
            || ptrace(PTRACE_TRACEME, 0, 0, 0)) _exit(10);
        raise(SIGSTOP);
        if (delivered || sigpending(&pending) || !sigismember(&pending, SIGUSR2)) _exit(11);
        if (sigprocmask(SIG_UNBLOCK, &blocked, 0)) _exit(12);
        _exit(delivered == SIGUSR2 && delivered_code == SI_QUEUE && delivered_pid == 123
            && delivered_value == 456 ? 0 : 13);
    }
    int status; CHECK(waitpid(child, &status, 0) == child && WIFSTOPPED(status));
    siginfo_t info = { .si_signo = SIGUSR2, .si_code = SI_QUEUE, .si_pid = 123, .si_uid = 7 };
    info.si_value.sival_int = 456;
    CHECK(ptrace(PTRACE_SETSIGINFO, child, 0, &info) == 0);
    memset(&info, 0, sizeof info);
    CHECK(ptrace(PTRACE_GETSIGINFO, child, 0, &info) == 0 && info.si_signo == SIGUSR2
        && info.si_code == SI_QUEUE && info.si_pid == 123 && info.si_value.sival_int == 456);
    CHECK(ptrace(PTRACE_DETACH, child, 0, SIGUSR2) == 0);
    CHECK(waitpid(child, &status, 0) == child && WIFEXITED(status) && !WEXITSTATUS(status));
    return 0;
}

static int attach_exit(void)
{
    int fds[2]; CHECK(pipe(fds) == 0);
    pid_t victim = fork(); CHECK(victim >= 0);
    if (!victim) { char value; close(fds[1]); read(fds[0], &value, 1); _exit(23); }
    close(fds[0]);
    CHECK(ptrace(PTRACE_ATTACH, victim, 0, 0) == 0);
    int status;
    CHECK(waitpid(victim, &status, 0) == victim && WIFSTOPPED(status));
    CHECK(ptrace(PTRACE_ATTACH, victim, 0, 0) == -1 && errno == EPERM);
    CHECK(ptrace(PTRACE_CONT, victim, 0, 0) == 0);
    CHECK(write(fds[1], "x", 1) == 1); close(fds[1]);
    CHECK(waitpid(victim, &status, 0) == victim && WIFEXITED(status) && WEXITSTATUS(status) == 23);
    CHECK(waitpid(victim, &status, WNOHANG) == -1 && errno == ECHILD);
    return 0;
}


static int trace_exec(void)
{
    pid_t child = fork(); CHECK(child >= 0);
    if (!child) { if (ptrace(PTRACE_TRACEME, 0, 0, 0)) _exit(10); execl("/proc/self/exe", "ptrace", "ptrace-exec-image", (char *)0); _exit(11); }
    int status;
    CHECK(waitpid(child, &status, 0) == child && WIFSTOPPED(status) && WSTOPSIG(status) == SIGTRAP);
    CHECK(ptrace(PTRACE_CONT, child, 0, 0) == 0);
    CHECK(waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0);
    return 0;
}

static int external_tracer(void)
{
    int release[2], ready[2]; CHECK(pipe(release) == 0 && pipe(ready) == 0);
    pid_t victim = fork(); CHECK(victim >= 0);
    if (!victim) { close(release[1]); char byte; read(release[0], &byte, 1); _exit(27); }
    close(release[0]);
    pid_t tracer = fork(); CHECK(tracer >= 0);
    if (!tracer) {
        int status; close(ready[0]);
        if (ptrace(PTRACE_ATTACH, victim, 0, 0) || waitpid(victim, &status, 0) != victim || !WIFSTOPPED(status)) _exit(10);
        if (ptrace(PTRACE_CONT, victim, 0, 0) || write(ready[1], "x", 1) != 1) _exit(11);
        if (waitpid(victim, &status, 0) != victim || !WIFEXITED(status) || WEXITSTATUS(status) != 27) _exit(12);
        _exit(0);
    }
    close(ready[1]); char byte; CHECK(read(ready[0], &byte, 1) == 1); close(ready[0]);
    CHECK(write(release[1], "x", 1) == 1); close(release[1]);
    int status; CHECK(waitpid(tracer, &status, 0) == tracer && WIFEXITED(status) && !WEXITSTATUS(status));
    CHECK(waitpid(victim, &status, 0) == victim && WIFEXITED(status) && WEXITSTATUS(status) == 27);
    CHECK(waitpid(victim, &status, WNOHANG) == -1 && errno == ECHILD);
    return 0;
}

static int tracer_exit(void)
{
    int resumed[2]; CHECK(pipe(resumed) == 0);
    pid_t tracer = fork(); CHECK(tracer >= 0);
    if (!tracer) {
        close(resumed[0]); pid_t victim = fork(); if (victim < 0) _exit(10);
        if (!victim) { if (ptrace(PTRACE_TRACEME, 0, 0, 0)) _exit(11); raise(SIGSTOP); write(resumed[1], "x", 1); _exit(0); }
        int status; if (waitpid(victim, &status, 0) != victim || !WIFSTOPPED(status)) _exit(12);
        _exit(0); /* exit_ptrace 必须解除跟踪并恢复 victim。 */
    }
    close(resumed[1]); int status; CHECK(waitpid(tracer, &status, 0) == tracer && WIFEXITED(status) && !WEXITSTATUS(status));
    char byte; CHECK(read(resumed[0], &byte, 1) == 1); close(resumed[0]);
    return 0;
}

static int credential_denial(void)
{
    int ready[2], release[2]; CHECK(pipe(ready) == 0 && pipe(release) == 0);
    pid_t child = fork(); CHECK(child >= 0);
    if (!child) { close(release[1]); close(ready[0]); if (prctl(PR_SET_DUMPABLE, 0)) _exit(10); write(ready[1], "x", 1); char byte; read(release[0], &byte, 1); _exit(0); }
    close(ready[1]); close(release[0]); char byte; CHECK(read(ready[0], &byte, 1) == 1); close(ready[0]);
    struct __user_cap_header_struct header = {_LINUX_CAPABILITY_VERSION_3, 0};
    struct __user_cap_data_struct caps[2], saved[2];
    CHECK(syscall(SYS_capget, &header, caps) == 0); memcpy(saved, caps, sizeof caps);
    caps[0].effective &= ~(1U << CAP_SYS_PTRACE);
    CHECK(syscall(SYS_capset, &header, caps) == 0);
    CHECK(ptrace(PTRACE_ATTACH, child, 0, 0) == -1 && errno == EPERM);
    CHECK(syscall(SYS_capset, &header, saved) == 0);
    CHECK(write(release[1], "x", 1) == 1); close(release[1]);
    int status; CHECK(waitpid(child, &status, 0) == child && WIFEXITED(status) && !WEXITSTATUS(status));
    return 0;
}

static int worker_ready[2], worker_release[2];
static _Atomic unsigned *heartbeats;
static void *worker_main(void *unused)
{
    (void)unused; pid_t tid = syscall(SYS_gettid); write(worker_ready[1], &tid, sizeof tid);
    char byte; read(worker_release[0], &byte, 1); return 0;
}
static int thread_attach(void)
{
    heartbeats = mmap(0, 4096, PROT_READ|PROT_WRITE, MAP_SHARED|MAP_ANONYMOUS, -1, 0);
    CHECK(heartbeats != MAP_FAILED && pipe(worker_ready) == 0 && pipe(worker_release) == 0);
    pid_t child = fork(); CHECK(child >= 0);
    if (!child) {
        close(worker_ready[0]); close(worker_release[1]); pthread_t thread;
        if (pthread_create(&thread, 0, worker_main, 0)) _exit(10);
        while (atomic_load(heartbeats + 1) == 0) { atomic_fetch_add(heartbeats, 1); usleep(1000); }
        pthread_join(thread, 0); _exit(37);
    }
    close(worker_ready[1]); close(worker_release[0]); pid_t tid;
    CHECK(read(worker_ready[0], &tid, sizeof tid) == sizeof tid); close(worker_ready[0]);
    CHECK(ptrace(PTRACE_ATTACH, tid, 0, 0) == 0); int status;
    CHECK(waitpid(tid, &status, __WALL) == tid && WIFSTOPPED(status));
    unsigned before = atomic_load(heartbeats); usleep(20000); CHECK(atomic_load(heartbeats) > before);
    CHECK(ptrace(PTRACE_CONT, tid, 0, 0) == 0);
    CHECK(write(worker_release[1], "x", 1) == 1); close(worker_release[1]);
    CHECK(waitpid(tid, &status, __WALL) == tid && WIFEXITED(status) && !WEXITSTATUS(status));
    atomic_store(heartbeats + 1, 1);
    CHECK(waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 37);
    CHECK(munmap(heartbeats, 4096) == 0); return 0;
}


__attribute__((noreturn)) static void redirected_exit(void) { _exit(41); }
static int redirect_blocked_syscall(void)
{
    int fds[2]; CHECK(pipe(fds) == 0);
    pid_t child = fork(); CHECK(child >= 0);
    if (!child) { close(fds[1]); char byte; read(fds[0], &byte, 1); _exit(42); }
    usleep(10000);
    CHECK(ptrace(PTRACE_ATTACH, child, 0, 0) == 0); int status;
    CHECK(waitpid(child, &status, 0) == child && WIFSTOPPED(status));
    unsigned long registers[34]; struct iovec iov = {registers, sizeof registers};
    CHECK(ptrace(PTRACE_GETREGSET, child, (void *)1, &iov) == 0);
#if defined(__aarch64__)
    registers[32] = (uintptr_t)redirected_exit;
#else
    registers[0] = (uintptr_t)redirected_exit;
#endif
    CHECK(ptrace(PTRACE_SETREGSET, child, (void *)1, &iov) == 0);
    CHECK(ptrace(PTRACE_CONT, child, 0, 0) == 0);
    CHECK(write(fds[1], "x", 1) == 1); close(fds[1]); close(fds[0]);
    CHECK(waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 41);
    return 0;
}


struct concurrent_wait {
    pid_t child;
    _Atomic int ready;
    _Atomic int finish;
    int failure;
    int status;
    pid_t waited;
};
static void *own_attachment(void *opaque)
{
    struct concurrent_wait *race = opaque;
    siginfo_t info;
    if (ptrace(PTRACE_ATTACH, race->child, 0, 0) || waitid(P_PID, race->child, &info, WSTOPPED|WNOWAIT)) race->failure = 1;
    atomic_store(&race->ready, 1);
    while (!atomic_load(&race->finish)) sched_yield();
    return 0;
}
static void *consume_attachment_event(void *opaque)
{
    struct concurrent_wait *race = opaque;
    race->waited = waitpid(race->child, &race->status, 0);
    return 0;
}
static int concurrent_tracer_exit(void)
{
    for (int iteration = 0; iteration < 16; ++iteration) {
        int fds[2]; CHECK(pipe(fds) == 0);
        pid_t child = fork(); CHECK(child >= 0);
        if (!child) { close(fds[1]); char byte; read(fds[0], &byte, 1); _exit(0); }
        struct concurrent_wait race = {.child = child};
        pthread_t owner, waiter;
        CHECK(pthread_create(&owner, 0, own_attachment, &race) == 0);
        while (!atomic_load(&race.ready)) sched_yield();
        CHECK(!race.failure);
        CHECK(pthread_create(&waiter, 0, consume_attachment_event, &race) == 0);
        atomic_store(&race.finish, 1);
        CHECK(pthread_join(owner, 0) == 0);
        CHECK(write(fds[1], "x", 1) == 1); close(fds[1]); close(fds[0]);
        CHECK(pthread_join(waiter, 0) == 0 && race.waited == child);
        int status = race.status;
        if (WIFSTOPPED(status)) CHECK(waitpid(child, &status, 0) == child);
        CHECK(WIFEXITED(status) && !WEXITSTATUS(status));
    }
    return 0;
}


static void *exec_as_worker(void *opaque)
{
    int fd = *(int *)opaque;
    pid_t tid = syscall(SYS_gettid);
    if (ptrace(PTRACE_TRACEME, 0, 0, 0)) tid = -1;
    write(fd, &tid, sizeof tid);
    if (tid < 0) _exit(10);
    raise(SIGSTOP);
    execl("/proc/self/exe", "ptrace", "ptrace-exec-image", (char *)0);
    _exit(11);
}
static int traced_worker_exec_promotes_identity(void)
{
    int fds[2]; CHECK(pipe(fds) == 0);
    pid_t child = fork(); CHECK(child >= 0);
    if (!child) {
        close(fds[0]); pthread_t worker;
        if (pthread_create(&worker, 0, exec_as_worker, &fds[1])) _exit(12);
        for (;;) pause();
    }
    close(fds[1]); pid_t tid;
    CHECK(read(fds[0], &tid, sizeof tid) == sizeof tid && tid > 0 && tid != child); close(fds[0]);
    int status;
    CHECK(waitpid(tid, &status, 0) == tid && WIFSTOPPED(status) && WSTOPSIG(status) == SIGSTOP);
    CHECK(ptrace(PTRACE_CONT, tid, 0, 0) == 0);
    CHECK(waitpid(-1, &status, 0) == child && WIFSTOPPED(status) && WSTOPSIG(status) == SIGTRAP);
    CHECK(ptrace(PTRACE_CONT, child, 0, 0) == 0);
    CHECK(waitpid(child, &status, 0) == child && WIFEXITED(status) && !WEXITSTATUS(status));
    CHECK(waitpid(tid, &status, WNOHANG) == -1 && errno == ECHILD);
    return 0;
}

int verify_ptrace(void)
{
    int result;
#define RUN(test) do { printf("ptrace %s\n", #test); if ((result = test())) return result; } while (0)
    RUN(redirect_blocked_syscall); RUN(trace_me); RUN(blocked_signal_info); RUN(attach_exit); RUN(trace_exec);
    RUN(external_tracer); RUN(tracer_exit); RUN(credential_denial); RUN(thread_attach); RUN(concurrent_tracer_exit); RUN(traced_worker_exec_promotes_identity);
#undef RUN
    return 0;
}
#ifdef PTRACE_STANDALONE
int main(int argc, char **argv)
{
    if (argc == 2 && !strcmp(argv[1], "ptrace-exec-image")) return 0;
    int result = verify_ptrace(); printf("PTRACE_RESULT=%d\n", result); return result != 0;
}
#endif
