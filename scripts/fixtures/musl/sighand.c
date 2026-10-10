#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static void handler_a(int signal) { (void)signal; }
static void handler_b(int signal) { (void)signal; }
static _Noreturn void sighand_fail(int step)
{
	dprintf(2, "sighand fail step=%d\n", step);
	_exit(step);
}
static int child_ok(pid_t child)
{
	int status = -1;
	if (child > 0 && waitpid(child, &status, 0) == child && WIFEXITED(status) && !WEXITSTATUS(status)) return 1;
	printf("sighand child status=%d\n", status);
	return 0;
}
static int install(int signal, void (*handler)(int), int flags)
{
	struct sigaction action = { .sa_handler = handler, .sa_flags = flags };
	sigemptyset(&action.sa_mask);
	sigaddset(&action.sa_mask, SIGUSR1);
	return sigaction(signal, &action, 0) == 0;
}
static int poll_signal(int signal)
{
	sigset_t set;
	sigemptyset(&set);
	sigaddset(&set, signal);
	struct timespec timeout = {0};
	return sigtimedwait(&set, 0, &timeout);
}
struct clone_input { int ready[2], finish[2]; int exec; };
static int shared_child(void *opaque)
{
	struct clone_input *input = opaque;
	if (input->exec) {
		/* Disposition and blocked pending survive independently from parent's shared table. */
		if (kill(getpid(), SIGUSR1) || dup2(input->ready[1], 33) != 33) sighand_fail(116);
		execl("/bin/init", "init", "sighand-exec", (char *)0);
		sighand_fail(117);
	}
	if (!install(SIGUSR1, handler_b, 0)) sighand_fail(118);
	if (poll_signal(SIGUSR1) != -1 || errno != EAGAIN) sighand_fail(119);
	sigset_t set;
	sigemptyset(&set); sigaddset(&set, SIGUSR1);
	if (sigprocmask(SIG_UNBLOCK, &set, 0)) sighand_fail(120);
	char finish;
	if (write(input->ready[1], "R", 1) != 1 || read(input->finish[0], &finish, 1) != 1) sighand_fail(121);
	return 0;
}
int verify_sighand_exec_image(void)
{
	struct sigaction caught, ignored;
	if (sigaction(SIGUSR1, 0, &caught) || sigaction(SIGUSR2, 0, &ignored)) return 122;
	if (caught.sa_handler != SIG_DFL || caught.sa_flags != 0 || sigismember(&caught.sa_mask, SIGUSR1)
	    || ignored.sa_handler != SIG_IGN || ignored.sa_flags != 0 || sigismember(&ignored.sa_mask, SIGUSR1)) return 123;
	if (poll_signal(SIGUSR1) != SIGUSR1 || write(33, "E", 1) != 1) return 124;
	return 0;
}
static int shared_and_exec(void)
{
	sigset_t set, original;
	sigemptyset(&set); sigaddset(&set, SIGUSR1);
	if (sigprocmask(SIG_BLOCK, &set, &original)) return 0;
	if (!install(SIGUSR1, handler_a, SA_RESTART) || !install(SIGUSR2, SIG_IGN, SA_RESETHAND)) return 0;
	void *stack = mmap(0, 128 * 1024, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	if (stack == MAP_FAILED) return 0;
	for (int exec = 0; exec < 2; exec++) {
		struct clone_input input = { .exec = exec };
		if (pipe(input.ready) || pipe(input.finish)) return 0;
		if (exec == 0 && kill(getpid(), SIGUSR1)) return 0;
		pid_t child = clone(shared_child, (char *)stack + 128 * 1024, CLONE_VM | CLONE_SIGHAND | SIGCHLD, &input);
		char ready;
		if (child <= 0 || read(input.ready[0], &ready, 1) != 1 || ready != (exec ? 'E' : 'R')) return 0;
		struct sigaction action, ignored;
		sigset_t mask;
		if (sigaction(SIGUSR1, 0, &action) || sigaction(SIGUSR2, 0, &ignored) || sigprocmask(SIG_SETMASK, 0, &mask)) return 0;
		if (action.sa_handler != (exec ? handler_a : handler_b) || !sigismember(&mask, SIGUSR1)
		    || ignored.sa_handler != SIG_IGN || !(ignored.sa_flags & SA_RESETHAND)) return 0;
		if (!exec && (poll_signal(SIGUSR1) != SIGUSR1 || write(input.finish[1], "F", 1) != 1)) return 0;
		if (!child_ok(child)) return 0;
		close(input.ready[0]); close(input.ready[1]); close(input.finish[0]); close(input.finish[1]);
		if (!install(SIGUSR1, handler_a, SA_RESTART)) return 0;
	}
	munmap(stack, 128 * 1024);
	if (sigprocmask(SIG_SETMASK, &original, 0)) return 0;
	/* CLONE_SIGHAND requires CLONE_VM before any child is published. */
	errno = 0;
	if (syscall(SYS_clone, CLONE_SIGHAND | SIGCHLD, 0, 0, 0, 0) != -1 || errno != EINVAL) return 0;
	pid_t child = fork();
	if (child == 0) { if (!install(SIGUSR1, handler_b, 0)) sighand_fail(125); _exit(0); }
	struct sigaction action;
	return child_ok(child) && !sigaction(SIGUSR1, 0, &action) && action.sa_handler == handler_a;
}
static atomic_int worker_tid, worker_phase, worker_result;
static void *pending_worker(void *unused)
{
	(void)unused;
	atomic_store(&worker_tid, syscall(SYS_gettid));
	while (!atomic_load(&worker_phase)) sched_yield();
	int okay = poll_signal(SIGUSR1) == -1 && errno == EAGAIN
	    && poll_signal(SIGCHLD) == -1 && errno == EAGAIN;
	atomic_store(&worker_result, okay);
	return 0;
}
static int discard_pending(void)
{
	sigset_t set, original;
	sigemptyset(&set); sigaddset(&set, SIGUSR1); sigaddset(&set, SIGCHLD);
	if (sigprocmask(SIG_BLOCK, &set, &original)
	    || !install(SIGUSR1, handler_a, 0) || !install(SIGCHLD, handler_a, 0)) return 0;
	pthread_t worker;
	atomic_store(&worker_tid, 0); atomic_store(&worker_phase, 0); atomic_store(&worker_result, 0);
	if (pthread_create(&worker, 0, pending_worker, 0)) return 0;
	while (!atomic_load(&worker_tid)) sched_yield();
	for (int signal = SIGUSR1; signal <= SIGCHLD; signal += SIGCHLD - SIGUSR1) {
		if (kill(getpid(), signal) || syscall(SYS_tgkill, getpid(), syscall(SYS_gettid), signal)
		    || syscall(SYS_tgkill, getpid(), atomic_load(&worker_tid), signal)) return 0;
	}
	if (!install(SIGUSR1, SIG_IGN, 0) || !install(SIGCHLD, SIG_DFL, 0)) return 0;
	int okay = poll_signal(SIGUSR1) == -1 && errno == EAGAIN
	    && poll_signal(SIGCHLD) == -1 && errno == EAGAIN;
	atomic_store(&worker_phase, 1);
	if (pthread_join(worker, 0) || !atomic_load(&worker_result)) return 0;
	return okay && !sigprocmask(SIG_SETMASK, &original, 0);
}
static volatile sig_atomic_t one_shot_ok;
static void one_shot(int signal)
{
	struct sigaction old;
	one_shot_ok = !sigaction(signal, 0, &old) && old.sa_handler == SIG_DFL
	    && (old.sa_flags & SA_RESETHAND) && sigismember(&old.sa_mask, SIGUSR1);
	/* Installation during handler/frame delivery must persist. */
	if (!install(signal, handler_b, 0)) one_shot_ok = 0;
}
static int reset_hand(void)
{
	one_shot_ok = 0;
	if (!install(SIGUSR1, one_shot, SA_RESETHAND) || kill(getpid(), SIGUSR1)) return 0;
	struct sigaction action;
	return one_shot_ok && !sigaction(SIGUSR1, 0, &action) && action.sa_handler == handler_b;
}
static int ignored_arrival_and_flags(void)
{
    sigset_t set, original;
    sigemptyset(&set); sigaddset(&set, SIGUSR1); sigaddset(&set, SIGCHLD);
    if (sigprocmask(SIG_BLOCK, &set, &original)
        || !install(SIGUSR1, SIG_IGN, SA_RESTART | 0x400 | 0x800 | 0x20000000)
        || !install(SIGCHLD, SIG_DFL, 0)) return 0;
    struct sigaction queried;
    if (sigaction(SIGUSR1, 0, &queried) || !(queried.sa_flags & SA_RESTART)
        || queried.sa_flags & (0x400 | 0x800 | 0x20000000)) return 0;
    if (syscall(SYS_tgkill, getpid(), syscall(SYS_gettid), SIGUSR1)
        || poll_signal(SIGUSR1) != SIGUSR1 || kill(getpid(), SIGUSR1)
        || poll_signal(SIGUSR1) != SIGUSR1 || kill(getpid(), SIGCHLD)
        || poll_signal(SIGCHLD) != SIGCHLD) return 0;
    return !sigprocmask(SIG_SETMASK, &original, 0);
}
static int vfork_autoreap_exit(void)
{
    for (int mode = 0; mode < 2; mode++) {
        if (!install(SIGCHLD, mode ? SIG_DFL : SIG_IGN, mode ? SA_NOCLDWAIT : 0)) return 0;
        pid_t child = vfork();
        if (child == 0) _exit(0);
        int status;
        if (child <= 0 || waitpid(child, &status, 0) != -1 || errno != ECHILD) return 0;
    }
    return install(SIGCHLD, SIG_DFL, 0);
}
static int child_dispositions(void)
{
    sigset_t set, original;
    sigemptyset(&set); sigaddset(&set, SIGCHLD);
    if (sigprocmask(SIG_BLOCK, &set, &original)) return 0;
    for (int mode = 0; mode < 4; mode++) {
        while (poll_signal(SIGCHLD) == SIGCHLD) { }
        void (*handler)(int) = mode == 1 ? SIG_IGN : mode == 3 ? handler_a : SIG_DFL;
        if (!install(SIGCHLD, handler, mode >= 2 ? SA_NOCLDWAIT : 0)) return 0;
        pid_t child = fork();
        if (child == 0) _exit(0);
        int status = -1;
        errno = 0;
        pid_t waited = waitpid(child, &status, 0);
        if (mode == 0 ? waited != child || !WIFEXITED(status) || WEXITSTATUS(status)
            : waited != -1 || errno != ECHILD) return 0;
        if (mode == 1) { if (poll_signal(SIGCHLD) != -1 || errno != EAGAIN) return 0; }
        else if (sigwaitinfo(&set, 0) != SIGCHLD) return 0;
        if (kill(child, 0) != -1 || errno != ESRCH) return 0;
    }
    if (!install(SIGCHLD, handler_a, SA_NOCLDSTOP)) return 0;
    pid_t stopped = fork();
    if (stopped == 0) { raise(SIGSTOP); for (;;) pause(); }
    int status = -1;
    if (stopped <= 0 || waitpid(stopped, &status, WUNTRACED) != stopped || !WIFSTOPPED(status)
        || poll_signal(SIGCHLD) != -1 || errno != EAGAIN) return 0;
    if (kill(stopped, SIGCONT) || waitpid(stopped, &status, WCONTINUED) != stopped || !WIFCONTINUED(status)
        || poll_signal(SIGCHLD) != -1 || errno != EAGAIN) return 0;
    if (kill(stopped, SIGTERM) || waitpid(stopped, &status, 0) != stopped || !WIFSIGNALED(status)) return 0;
    /* Exit still generates SIGCHLD; publication can follow the waiter's status wake. */
    if (sigwaitinfo(&set, 0) != SIGCHLD) return 0;
    return install(SIGCHLD, SIG_DFL, 0) && !sigprocmask(SIG_SETMASK, &original, 0);
}
/* Freeze the unblocked leader in vfork while the blocked creator worker waits. This
 * prevents a correctly generated group signal being consumed by an unblocked sibling. */
static atomic_int creator_gate, creator_ok;
static int creator_freeze_leader(void *unused)
{
    (void)unused;
    atomic_store(&creator_gate, 1);
    while (atomic_load(&creator_gate) != 2) sched_yield();
    return 0;
}
static void *creator_signal_worker(void *unused)
{
    (void)unused;
    sigset_t set;
    sigemptyset(&set); sigaddset(&set, SIGCHLD);
    if (pthread_sigmask(SIG_BLOCK, &set, 0)) {
        atomic_store(&creator_gate, 2); return 0;
    }
    while (!atomic_load(&creator_gate)) sched_yield();
    pid_t child = fork();
    if (child == 0) _exit(0);
    struct timespec timeout = { .tv_sec = 3 };
    int result = child > 0 && sigtimedwait(&set, 0, &timeout) == SIGCHLD;
    int status;
    result = result && waitpid(child, &status, 0) == child && WIFEXITED(status) && !WEXITSTATUS(status);
    atomic_store(&creator_ok, result);
    atomic_store(&creator_gate, 2);
    return 0;
}
static int creator_mask_generation(void)
{
    sigset_t set, original;
    sigemptyset(&set); sigaddset(&set, SIGCHLD);
    if (sigprocmask(SIG_UNBLOCK, &set, &original) || !install(SIGCHLD, SIG_DFL, 0)) return 0;
    atomic_store(&creator_gate, 0); atomic_store(&creator_ok, 0);
    pthread_t worker;
    void *stack = mmap(0, 128 * 1024, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (stack == MAP_FAILED || pthread_create(&worker, 0, creator_signal_worker, 0)) return 0;
    pid_t freezer = clone(creator_freeze_leader, (char *)stack + 128 * 1024,
        CLONE_VM | CLONE_VFORK | SIGCHLD, 0);
    int result = child_ok(freezer) && !pthread_join(worker, 0) && atomic_load(&creator_ok);
    munmap(stack, 128 * 1024);
    return result && !sigprocmask(SIG_SETMASK, &original, 0);
}
int verify_sighand(void)
{
	pid_t child = fork();
	if (child == 0) {
		if (!shared_and_exec()) sighand_fail(126);
		if (!discard_pending()) sighand_fail(127);
		if (!reset_hand()) sighand_fail(128);
        if (!ignored_arrival_and_flags()) sighand_fail(130);
        if (!child_dispositions()) sighand_fail(131);
        if (!vfork_autoreap_exit()) sighand_fail(132);
        if (!creator_mask_generation()) sighand_fail(133);
		_exit(0);
	}
	if (!child_ok(child)) return 129;
	puts("LiteOS musl sighand sharing exec detach pending discard and one shot ok");
	return 0;
}
