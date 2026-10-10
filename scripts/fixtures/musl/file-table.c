#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

/* Thread 变体走 musl 的 raw Linux assembler entry；公开 clone wrapper 拒绝 Thread flags。 */
extern int __clone(int (*)(void *), void *, int, void *, int *, void *, int *);
static const size_t stack_size = 64 * 1024;

static int lock_file(int fd, int command, short type)
{
	struct flock lock = { .l_type = type, .l_whence = SEEK_SET, .l_len = 100 };
	return fcntl(fd, command, &lock);
}

static int conflict_is(int fd, short type, pid_t pid)
{
	struct flock lock = { .l_type = F_WRLCK, .l_whence = SEEK_SET, .l_len = 100 };
	return fcntl(fd, F_GETLK, &lock) == 0 && lock.l_type == type
	    && (type == F_UNLCK || lock.l_pid == pid);
}

static int child_ok(pid_t child)
{
	int status = -1;
	if (child > 0 && waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0) return 1;
	printf("file table child status=%d\n", status);
	return 0;
}

static void wait_phase(atomic_int *phase, int wanted)
{
	struct timespec pause = { .tv_nsec = 1000000 };
	for (int attempt = 0; atomic_load(phase) != wanted; attempt++) {
		if (attempt == 2000) _exit(90);
		nanosleep(&pause, 0);
	}
}

struct sharing_probe { atomic_int phase; int fd, second; };
static void *detach_table(void *argument)
{
	struct sharing_probe *probe = argument;
	if (fcntl(probe->fd, F_SETFD, FD_CLOEXEC) != 0) return 0;
	atomic_store(&probe->phase, 1);
	wait_phase(&probe->phase, 2);
	if (unshare(CLONE_FILES | CLONE_FS) != 0 || close(probe->fd) != 0
	    || fcntl(probe->second, F_SETFD, FD_CLOEXEC) != 0 || chdir("/files-private") != 0
	    || umask(077) != 022) return 0;
	/* 新表即使 TGID 相同也不能继承原 POSIX owner。 */
	if (!conflict_is(probe->second, F_WRLCK, getpid())) return 0;
	errno = 0;
	if (lock_file(probe->second, F_SETLK, F_WRLCK) != -1 || errno != EAGAIN) return 0;
	atomic_store(&probe->phase, 3);
	wait_phase(&probe->phase, 4);
	return argument;
}

static int private_table_clone(void *argument)
{
	int fd = *(int *)argument;
	/* 不带 CLONE_FILES 的 Thread 有独立 fd flags/close，OFD 仍共享。 */
	if (fcntl(fd, F_SETFD, FD_CLOEXEC) != 0 || fcntl(fd, F_SETFL, O_APPEND) != 0 || close(fd) != 0)
		syscall(SYS_exit, 91);
	return 0;
}

static int sharing_and_unshare(void)
{
	pid_t child = fork();
	if (child == 0) {
		if (mkdir("/files-private", 0755) != 0) _exit(1);
		umask(022);
		struct sharing_probe probe = { .fd = open("/files-lock", O_RDWR | O_CREAT | O_TRUNC, 0600) };
		probe.second = dup(probe.fd);
		if (probe.fd < 0 || probe.second < 0 || lock_file(probe.fd, F_SETLK, F_WRLCK) != 0) _exit(2);
		pthread_t worker;
		if (pthread_create(&worker, 0, detach_table, &probe) != 0) _exit(3);
		wait_phase(&probe.phase, 1);
		if (fcntl(probe.fd, F_GETFD) != FD_CLOEXEC) _exit(4);
		atomic_store(&probe.phase, 2);
		wait_phase(&probe.phase, 3);
		char cwd[64];
		if (fcntl(probe.fd, F_GETFD) != FD_CLOEXEC || fcntl(probe.second, F_GETFD) != 0
		    || !getcwd(cwd, sizeof cwd) || strcmp(cwd, "/") != 0 || umask(022) != 022) _exit(5);
		atomic_store(&probe.phase, 4);
		void *joined;
		if (pthread_join(worker, &joined) != 0 || joined != &probe) _exit(6);
		if (lock_file(probe.fd, F_SETLK, F_UNLCK) != 0 || fcntl(probe.fd, F_SETFD, 0) != 0) _exit(7);
		char *stack = mmap(0, stack_size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
		if (stack == MAP_FAILED) _exit(8);
		atomic_int clear_tid = -1;
		int flags = CLONE_VM | CLONE_FS | CLONE_SIGHAND | CLONE_THREAD | CLONE_SYSVSEM | CLONE_SETTLS | CLONE_CHILD_CLEARTID;
		if (__clone(private_table_clone, stack + stack_size, flags, &probe.fd, 0,
		    __builtin_thread_pointer(), (int *)&clear_tid) < 0) _exit(9);
		while (atomic_load(&clear_tid) != 0) syscall(SYS_sched_yield);
		if (fcntl(probe.fd, F_GETFD) != 0 || !(fcntl(probe.fd, F_GETFL) & O_APPEND)
		    || unshare(CLONE_FILES) != 0 || close(probe.fd) != 0 || close(probe.second) != 0
		    || munmap(stack, stack_size) != 0 || rmdir("/files-private") != 0 || unlink("/files-lock") != 0) _exit(10);
		_exit(0);
	}
	return child_ok(child) ? 0 : -1;
}

static int check_from_private_table(int fd, short type, pid_t pid)
{
	pid_t child = fork();
	if (child == 0) _exit(conflict_is(fd, type, pid) ? 0 : 1);
	return child_ok(child);
}

/* exec image 用固定标准 descriptor ABI，父进程不依赖 stdout 的时序断言成功。 */
int verify_file_table_exec_image(int shared)
{
	errno = 0;
	if (fcntl(30, F_GETFD) != 0 || fcntl(31, F_GETFD) != -1 || errno != EBADF
	    || !conflict_is(30, shared ? F_WRLCK : F_UNLCK, getpid())) return 92;
	char done;
	return write(33, "E", 1) == 1 && read(34, &done, 1) == 1 ? 0 : 93;
}

struct exec_probe { int shared, vm; };
static int exec_table(void *argument)
{
	struct exec_probe *probe = argument;
	stack_t current;
	if (sigaltstack(0, &current) != 0 || (!!(current.ss_flags & SS_DISABLE) != probe->vm)) return 94;
	if (lock_file(30, F_SETLK, F_WRLCK) != 0) return 95;
	execl("/bin/init", "init", probe->shared ? "file-table-exec-shared" : "file-table-exec-unique", (char *)0);
	return 96;
}

static int map_descriptor(int source, int destination)
{
	if (source < 0 || dup2(source, destination) != destination) return -1;
	return close(source);
}

static int exec_table_case(int shared, int vm)
{
	pid_t test = fork();
	if (test == 0) {
		if (map_descriptor(open("/files-exec-lock", O_RDWR | O_CREAT | O_TRUNC, 0600), 30) != 0
		    || map_descriptor(open("/dev/null", O_RDONLY), 31) != 0 || fcntl(31, F_SETFD, FD_CLOEXEC) != 0) _exit(1);
		int ready[2], finish[2];
		if (pipe(ready) != 0 || pipe(finish) != 0
		    || map_descriptor(ready[0], 32) != 0 || map_descriptor(ready[1], 33) != 0
		    || map_descriptor(finish[0], 34) != 0 || map_descriptor(finish[1], 35) != 0) _exit(2);
		char *stack = mmap(0, stack_size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
		char *alternate = mmap(0, stack_size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
		stack_t registration = { .ss_sp = alternate, .ss_size = stack_size };
		if (stack == MAP_FAILED || alternate == MAP_FAILED || sigaltstack(&registration, 0) != 0) _exit(3);
		struct exec_probe probe = {shared, vm};
		int flags = SIGCHLD | (shared ? CLONE_FILES : 0) | (vm ? CLONE_VM : 0);
		pid_t child = clone(exec_table, stack + stack_size, flags, &probe);
		char marker;
		if (child < 0 || read(32, &marker, 1) != 1 || marker != 'E') _exit(4);
		if (!check_from_private_table(30, F_WRLCK, child) || fcntl(31, F_GETFD) != FD_CLOEXEC) _exit(5);
		if (write(35, "D", 1) != 1 || !child_ok(child)) _exit(6);
		/* 共享旧表的锁归父持有，不应因新exec表exit而消失；独占表则在child exit清空。 */
		if (!check_from_private_table(30, shared ? F_WRLCK : F_UNLCK, child)) _exit(7);
		if (close(30) != 0 || map_descriptor(open("/files-exec-lock", O_RDWR), 30) != 0
		    || !check_from_private_table(30, F_UNLCK, 0)) _exit(8);
		_exit(0);
	}
	return child_ok(test) ? 0 : -1;
}

/* VFORK 的完成等待与资源共享独立；VM|VFORK 按 Linux 保留 altstack。 */
static int vfork_resources(void *argument)
{
	stack_t current;
	int fd = *(int *)argument;
	if (sigaltstack(0, &current) != 0 || (current.ss_flags & SS_DISABLE)
	    || chdir("/files-vfork") != 0 || umask(077) != 022 || close(fd) != 0) return 97;
	return 0;
}

static int vfork_resource_case(void)
{
	pid_t test = fork();
	if (test == 0) {
		if (mkdir("/files-vfork", 0755) != 0) _exit(1);
		char *stack = mmap(0, stack_size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
		char *alternate = mmap(0, stack_size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
		stack_t registration = { .ss_sp = alternate, .ss_size = stack_size };
		int fd = open("/dev/null", O_RDONLY);
		if (stack == MAP_FAILED || alternate == MAP_FAILED || fd < 0 || sigaltstack(&registration, 0) != 0) _exit(2);
		umask(022);
		pid_t child = clone(vfork_resources, stack + stack_size, SIGCHLD | CLONE_VM | CLONE_VFORK | CLONE_FS | CLONE_FILES, &fd);
		char cwd[64];
		errno = 0;
		if (!child_ok(child) || fcntl(fd, F_GETFD) != -1 || errno != EBADF
		    || !getcwd(cwd, sizeof cwd) || strcmp(cwd, "/files-vfork") != 0 || umask(022) != 077
		    || chdir("/") != 0 || rmdir("/files-vfork") != 0) _exit(3);
		_exit(0);
	}
	return child_ok(test) ? 0 : -1;
}

struct close_probe { atomic_int phase; int fd; pid_t tid; };
static void *wait_for_closed_fd(void *argument)
{
	struct close_probe *probe = argument;
	probe->tid = syscall(SYS_gettid);
	atomic_store(&probe->phase, 1);
	errno = 0;
	int result = lock_file(probe->fd, F_SETLKW, F_WRLCK);
	return result == -1 && errno == EBADF ? argument : 0;
}

static int wait_until_lock_blocked(pid_t tid)
{
	char path[96], bytes[512];
	snprintf(path, sizeof path, "/files-proc/%ld/task/%ld/status", (long)getpid(), (long)tid);
	struct timespec pause = { .tv_nsec = 1000000 };
	for (int attempt = 0; attempt < 2000; attempt++) {
		int fd = open(path, O_RDONLY);
		if (fd < 0) return 0;
		ssize_t count = read(fd, bytes, sizeof bytes - 1); close(fd);
		if (count > 0) {
			bytes[count] = 0;
			if (strstr(bytes, "State:\tS")) return 1;
		}
		nanosleep(&pause, 0);
	}
	return 0;
}

static int close_wait_race(int replace)
{
	pid_t test = fork();
	if (test == 0) {
		int file = open("/files-race-lock", O_RDWR | O_CREAT | O_TRUNC, 0600);
		int ready[2], release[2];
		if (file < 0 || pipe(ready) != 0 || pipe(release) != 0) _exit(1);
		pid_t holder = fork();
		if (holder == 0) {
			char signal;
			if (lock_file(file, F_SETLK, F_WRLCK) != 0 || write(ready[1], "L", 1) != 1
			    || read(release[0], &signal, 1) != 1 || close(file) != 0) _exit(1);
			_exit(0);
		}
		char signal;
		if (holder < 0 || read(ready[0], &signal, 1) != 1) _exit(2);
		struct close_probe probe = { .fd = file };
		pthread_t worker;
		if (pthread_create(&worker, 0, wait_for_closed_fd, &probe) != 0) _exit(3);
		wait_phase(&probe.phase, 1);
		if (!wait_until_lock_blocked(probe.tid)) _exit(4);
		if (replace) {
			int other = open("/dev/null", O_RDONLY);
			if (other < 0 || dup2(other, file) != file || close(other) != 0) _exit(5);
		} else if (close(file) != 0) _exit(6);
		if (write(release[1], "U", 1) != 1 || !child_ok(holder)) _exit(7);
		void *joined;
		if (pthread_join(worker, &joined) != 0 || joined != &probe) _exit(8);
		int check = open("/files-race-lock", O_RDWR);
		if (check < 0 || !check_from_private_table(check, F_UNLCK, 0)) _exit(9);
		_exit(0);
	}
	return child_ok(test) ? 0 : -1;
}

int verify_file_table(void)
{
	if (mkdir("/files-proc", 0755) != 0 || mount("proc", "/files-proc", "proc", 0, 0) != 0) return 1;
	if (sharing_and_unshare() != 0) return 2;
	for (int shared = 0; shared <= 1; shared++)
		for (int vm = 0; vm <= 1; vm++)
			if (exec_table_case(shared, vm) != 0) return 3 + shared * 2 + vm;
	if (vfork_resource_case() != 0) return 9;
	if (close_wait_race(0) != 0 || close_wait_race(1) != 0) return 7;
	if (unlink("/files-exec-lock") != 0 || unlink("/files-race-lock") != 0
	    || umount("/files-proc") != 0 || rmdir("/files-proc") != 0) return 8;
	puts("LiteOS musl file table sharing exec and POSIX close race ok");
	return 0;
}
