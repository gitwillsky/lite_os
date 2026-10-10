#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/prctl.h>
#include <sys/resource.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static _Noreturn void mm_fail(int step)
{
	dprintf(2, "thread mm fail step=%d\n", step);
	_exit(step);
}

static int child_ok(pid_t child)
{
	int status = -1;
	if (child > 0 && waitpid(child, &status, 0) == child && WIFEXITED(status) && !WEXITSTATUS(status)) return 1;
	printf("thread mm child status=%d\n", status);
	return 0;
}

static ssize_t proc_read(const char *path, char *buffer, size_t size)
{
	int fd = open(path, O_RDONLY);
	if (fd < 0) return -1;
	ssize_t count = read(fd, buffer, size - 1);
	close(fd);
	if (count >= 0) buffer[count] = 0;
	return count;
}

/* comm may contain spaces; fields after its closing parenthesis start at field 3. */
static long long stat_field(char *text, int wanted)
{
	char *cursor = strrchr(text, ')');
	if (!cursor) return -1;
	cursor += 2;
	for (int field = 3; field < wanted; field++) {
		cursor = strchr(cursor, ' ');
		if (!cursor) return -1;
		cursor++;
	}
	return strtoll(cursor, 0, 10);
}

static int last_user_exit(void)
{
	int *word = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
	if (word == MAP_FAILED) return 0;
	pid_t child = fork();
	if (child == 0) {
		*word = syscall(SYS_gettid);
		if (syscall(SYS_set_tid_address, word) != *word) mm_fail(81);
		/* A different mm sharing this page does not make this child's mm_users > 1. */
		syscall(SYS_exit, 0);
		mm_fail(82);
	}
	int okay = child_ok(child) && *word == child;
	munmap(word, 4096);
	return okay;
}

int verify_thread_mm_exec_image(void)
{
	char finish;
	return write(33, "E", 1) == 1 && read(34, &finish, 1) == 1 ? 0 : 83;
}

static int shared_exec(void *word)
{
	*(int *)word = syscall(SYS_gettid);
	if (syscall(SYS_set_tid_address, word) != *(int *)word) mm_fail(84);
	execl("/bin/init", "init", "thread-mm-exec", (char *)0);
	mm_fail(85);
}

static int shared_user_exec(void)
{
	int ready[2], finish[2];
	if (pipe(ready) || pipe(finish) || dup2(ready[1], 33) < 0 || dup2(finish[0], 34) < 0) return 0;
	int word = -1;
	void *stack = mmap(0, 65536, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	if (stack == MAP_FAILED) return 0;
	pid_t child = clone(shared_exec, (char *)stack + 65536, CLONE_VM | SIGCHLD, &word);
	char observed;
	int okay = child > 0 && read(ready[0], &observed, 1) == 1 && observed == 'E' && word == 0;
	if (write(finish[1], "F", 1) != 1 || !child_ok(child)) okay = 0;
	munmap(stack, 65536);
	close(33); close(34);
	close(ready[0]); close(ready[1]); close(finish[0]); close(finish[1]);
	return okay;
}

struct read_only_probe { int *word; atomic_int waiter_tid; };
static void *read_only_waiter(void *argument)
{
	struct read_only_probe *probe = argument;
	atomic_store(&probe->waiter_tid, syscall(SYS_gettid));
	struct timespec timeout = { .tv_sec = 2 };
	return syscall(SYS_futex, probe->word, 0, 0x5678, &timeout, 0, 0) == 0 ? argument : 0;
}

static int read_only_exit(void *word)
{
	if (syscall(SYS_set_tid_address, word) <= 0) mm_fail(102);
	syscall(SYS_exit, 0);
	mm_fail(103);
}

static int read_only_release(void)
{
	struct read_only_probe probe = { .waiter_tid = 0 };
	probe.word = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	void *stack = mmap(0, 65536, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	if (probe.word == MAP_FAILED || stack == MAP_FAILED) return 0;
	*probe.word = 0x5678;
	if (mprotect(probe.word, 4096, PROT_READ)) return 0;
	pthread_t waiter;
	if (pthread_create(&waiter, 0, read_only_waiter, &probe)) return 0;
	char path[96], text[2048];
	for (int attempt = 0; ; attempt++) {
		pid_t tid = atomic_load(&probe.waiter_tid);
		if (attempt == 2000) return 0;
		if (tid) {
			snprintf(path, sizeof path, "/mm-proc/%d/task/%d/status", getpid(), tid);
			if (proc_read(path, text, sizeof text) >= 0 && strstr(text, "State:\tS")) break;
		}
		struct timespec pause = { .tv_nsec = 1000000 };
		nanosleep(&pause, 0);
	}
	pid_t child = clone(read_only_exit, (char *)stack + 65536, CLONE_VM | SIGCHLD, probe.word);
	void *joined;
	int okay = child_ok(child) && pthread_join(waiter, &joined) == 0 && joined == &probe && *probe.word == 0x5678;
	munmap(probe.word, 4096); munmap(stack, 65536);
	return okay;
}

static void parent_death(int signal)
{
	(void)signal;
	write(37, "D", 1);
	_exit(0);
}

static void *promote_worker(void *unused)
{
	(void)unused;
	char process_path[96], thread_path[96], text[2048], thread_text[2048];
	pid_t pid = getpid(), tid = syscall(SYS_gettid);
	snprintf(process_path, sizeof process_path, "/mm-proc/%d/status", pid);
	for (int attempt = 0; ; attempt++) {
		ssize_t count = proc_read(process_path, text, sizeof text);
		if (count < 0 || attempt == 2000) {
			dprintf(2, "thread mm leader status path=%s attempt=%d count=%ld errno=%d text=%s\n",
			        process_path, attempt, (long)count, errno, count < 0 ? "unreadable" : text);
			mm_fail(86);
		}
		if (strstr(text, "State:\tZ (zombie)")) break;
		struct timespec pause = { .tv_nsec = 1000000 };
		nanosleep(&pause, 0);
	}
	/* Linux nr_threads 包含尚未 release_task 的 zombie leader；存活 worker 不替代该历史 task。 */
	if (!strstr(text, "Threads:\t2\n") || !strstr(text, "FDSize:\t0\n") || strstr(text, "VmSize:")) mm_fail(87);
    for (const char **name=(const char*[]){"root","cwd",0}; *name; name++) {
        char link[96], target[128];
        snprintf(link,sizeof link,"/mm-proc/%d/%s",pid,*name);
        errno=0; if(readlink(link,target,sizeof target)!=-1 || errno!=ENOENT) mm_fail(102);
    }

	snprintf(thread_path, sizeof thread_path, "/mm-proc/%d/task/%d/status", pid, tid);
	if (proc_read(thread_path, thread_text, sizeof thread_text) < 0 || !strstr(thread_text, "VmSize:")
	    || strstr(thread_text, "FDSize:\t0\n") || strstr(thread_text, "State:\tZ")) mm_fail(88);
	snprintf(process_path, sizeof process_path, "/mm-proc/%d/cmdline", pid);
	if (proc_read(process_path, text, sizeof text) != 0) mm_fail(89);
	snprintf(process_path, sizeof process_path, "/mm-proc/%d/statm", pid);
	if (proc_read(process_path, text, sizeof text) < 0 || strcmp(text, "0 0 0 0 0 0 0\n")) mm_fail(90);
	snprintf(process_path, sizeof process_path, "/mm-proc/%d/stat", pid);
	snprintf(thread_path, sizeof thread_path, "/mm-proc/%d/task/%d/stat", pid, tid);
	if (setpriority(PRIO_PROCESS, tid, 5) || proc_read(process_path, text, sizeof text) < 0
	    || proc_read(thread_path, thread_text, sizeof thread_text) < 0 || stat_field(text, 19) != 0
	    || stat_field(thread_text, 19) != 5 || stat_field(text, 23) != 0 || stat_field(thread_text, 23) <= 0) mm_fail(91);
	long long start = stat_field(text, 22);
	if (start < 0 || stat_field(thread_text, 22) <= start) mm_fail(92);
	int armed[2];
	if (pipe(armed)) mm_fail(93);
	pid_t grandchild = fork();
	if (grandchild == 0) {
		struct sigaction action = { .sa_handler = parent_death };
		if (sigaction(SIGUSR1, &action, 0) || prctl(PR_SET_PDEATHSIG, SIGUSR1) || write(armed[1], "A", 1) != 1) mm_fail(94);
		for (;;) pause();
	}
	char marker;
	if (grandchild < 0 || read(armed[0], &marker, 1) != 1 || marker != 'A') mm_fail(95);
	close(armed[0]); close(armed[1]);
	char old_tid[32], original_start[32];
	snprintf(old_tid, sizeof old_tid, "%d", tid);
	snprintf(original_start, sizeof original_start, "%lld", start);
	execl("/bin/init", "init", "thread-mm-promoted", old_tid, original_start, (char *)0);
	mm_fail(96);
}

int verify_thread_mm_promoted_image(pid_t old_tid, long long original_start)
{
	char process_path[96], thread_path[96], text[2048], thread_text[2048];
	pid_t pid = getpid();
	if (syscall(SYS_gettid) != pid || old_tid == pid) return 97;
	errno = 0;
	if (syscall(SYS_tgkill, pid, old_tid, 0) != -1 || errno != ESRCH) return 98;
	snprintf(process_path, sizeof process_path, "/mm-proc/%d/stat", pid);
	snprintf(thread_path, sizeof thread_path, "/mm-proc/%d/task/%d/stat", pid, pid);
	if (proc_read(process_path, text, sizeof text) < 0 || proc_read(thread_path, thread_text, sizeof thread_text) < 0
	    || stat_field(text, 22) != original_start || stat_field(thread_text, 22) != original_start
	    || stat_field(text, 19) != 5 || stat_field(text, 23) <= 0) return 99;
	return verify_thread_mm_exec_image();
}

static int nonleader_exec(void)
{
	int ready[2], finish[2], death[2];
	if (pipe(ready) || pipe(finish) || pipe(death) || dup2(ready[1], 33) < 0 || dup2(finish[0], 34) < 0
	    || dup2(death[0], 36) < 0 || dup2(death[1], 37) < 0 || fcntl(36, F_SETFL, O_NONBLOCK)) return 0;
	pid_t child = fork();
	if (child == 0) {
		struct timespec pause = { .tv_nsec = 50000000 };
		nanosleep(&pause, 0);
		pthread_t worker;
		if (pthread_create(&worker, 0, promote_worker, 0)) mm_fail(100);
		syscall(SYS_exit, 0);
		mm_fail(101);
	}
	char observed;
	int okay = child > 0 && read(ready[0], &observed, 1) == 1 && observed == 'E';
	errno = 0;
	if (read(36, &observed, 1) != -1 || errno != EAGAIN) okay = 0;
	if (write(finish[1], "F", 1) != 1 || !child_ok(child) || fcntl(36, F_SETFL, 0)
	    || read(36, &observed, 1) != 1 || observed != 'D') okay = 0;
	close(33); close(34); close(36); close(37);
	close(ready[0]); close(ready[1]); close(finish[0]); close(finish[1]); close(death[0]); close(death[1]);
	return okay;
}

/* Isolated ELF pages let user munmap/MAP_FIXED exercise loaded VMAs without touching libc state. */
static volatile unsigned char elf_mapping_pages[3 * 4096] __attribute__((aligned(4096)));
static int elf_user_vma_mutation(void)
{
    pid_t child = fork();
    if (child == 0) {
        unsigned char *pages = (unsigned char *)elf_mapping_pages;
        pages[0] = 0x12; pages[4096] = 0x34; pages[8192] = 0x56;
        if (mprotect(pages + 4096, 4096, PROT_READ) || pages[4096] != 0x34
            || mprotect(pages + 4096, 4096, PROT_READ | PROT_WRITE)) mm_fail(110);
        if (munmap(pages + 4096, 4096)) mm_fail(111);
        void *middle = mmap(pages + 4096, 4096, PROT_READ | PROT_WRITE,
            MAP_FIXED_NOREPLACE | MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (middle != pages + 4096 || pages[4096] != 0 || pages[0] != 0x12 || pages[8192] != 0x56) mm_fail(112);
        if (mmap(pages + 8192, 4096, PROT_READ | PROT_WRITE,
            MAP_FIXED | MAP_PRIVATE | MAP_ANONYMOUS, -1, 0) != pages + 8192
            || pages[8192] != 0 || pages[0] != 0x12) mm_fail(113);
        _exit(0);
    }
    return child_ok(child);
}
/* VM_GROWSDOWN survives protection splits; a hole can be replaced without changing other fragments. */
static _Noreturn void split_real_stack(void)
{
    volatile unsigned char pages[8 * 4096] __attribute__((aligned(4096)));
    for (size_t offset = 0; offset < sizeof pages; offset += 4096) pages[offset] = 0x61;
    unsigned char *middle = (unsigned char *)pages + 4 * 4096;
    if (mprotect(middle, 4096, PROT_READ) || *middle != 0x61
        || mprotect(middle, 4096, PROT_READ | PROT_WRITE)) mm_fail(114);
    if (munmap(middle, 4096)
        || mmap(middle, 4096, PROT_READ | PROT_WRITE,
            MAP_FIXED_NOREPLACE | MAP_PRIVATE | MAP_ANONYMOUS, -1, 0) != middle
        || *middle != 0 || pages[0] != 0x61 || pages[7 * 4096] != 0x61) mm_fail(115);
    _exit(0);
}
static int stack_user_vma_mutation(void)
{
    pid_t child = fork();
    if (child == 0) split_real_stack();
    return child_ok(child);
}
struct fixed_probe { void *address; atomic_int ready, stop, observed_hole; atomic_uint attempts; };
static void *fixed_competitor(void *argument)
{
    struct fixed_probe *probe = argument;
    atomic_store(&probe->ready, 1);
    do {
        errno = 0;
        void *result = mmap(probe->address, 4096, PROT_READ | PROT_WRITE,
            MAP_FIXED_NOREPLACE | MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (result != MAP_FAILED || errno != EEXIST) atomic_store(&probe->observed_hole, 1);
        atomic_fetch_add(&probe->attempts, 1);
        sched_yield();
    } while (!atomic_load(&probe->stop));
    return argument;
}
static int fixed_replacement_has_no_shared_mm_gap(void)
{
    struct fixed_probe probe = {0};
    probe.address = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (probe.address == MAP_FAILED) return 0;
    pthread_t competitor;
    if (pthread_create(&competitor, 0, fixed_competitor, &probe)) return 0;
    while (!atomic_load(&probe.ready)) sched_yield();
    int okay = 1;
    for (int round = 0; round < 2000; round++) {
        if (mmap(probe.address, 4096, PROT_READ | PROT_WRITE,
            MAP_FIXED | MAP_PRIVATE | MAP_ANONYMOUS, -1, 0) != probe.address) { okay = 0; break; }
        sched_yield();
    }
    atomic_store(&probe.stop, 1);
    void *joined;
    if (pthread_join(competitor, &joined) || joined != &probe || atomic_load(&probe.observed_hole)
        || !atomic_load(&probe.attempts)) okay = 0;
    if (munmap(probe.address, 4096)) okay = 0;
    return okay;
}

int verify_thread_mm(void)
{
	if (mkdir("/mm-proc", 0755) || mount("proc", "/mm-proc", "proc", 0, 0)) return 1;
    if (!elf_user_vma_mutation()) return 7;
    if (!stack_user_vma_mutation()) return 8;
    if (!fixed_replacement_has_no_shared_mm_gap()) return 9;
	if (!last_user_exit()) return 2;
	if (!shared_user_exec()) return 3;
	if (!read_only_release()) return 6;
	if (!nonleader_exec()) return 4;
	if (umount("/mm-proc") || rmdir("/mm-proc")) return 5;
	puts("LiteOS musl thread mm lifetime nonleader exec identity and proc views ok");
	return 0;
}
