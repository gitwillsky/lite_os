#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <grp.h>
#include <pthread.h>
#include <stdint.h>
#include <stdatomic.h>
#include <stdio.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/prctl.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

/* 只通过标准 Linux ABI 验证线程范围、单向状态、继承与 exec 提权抑制。 */
int verify_fs_context(void);
static void *set_thread_no_new_privs(void *argument)
{
	if (prctl(PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) != 0
	    || prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0
	    || prctl(PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) != 1) return 0;
	return argument;
}

static void *inherited_no_new_privs(void *argument)
{
	return prctl(PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) == 1 ? argument : 0;
}

static int copy_setid_executable(void)
{
	int input = open("/bin/init", O_RDONLY | O_CLOEXEC);
	if (input < 0) return -1;
	int output = open("/security-exec", O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC, 0755);
	if (output < 0) {
		close(input);
		return -1;
	}
	char buffer[4096];
	ssize_t count;
	int result = 0;
	while ((count = read(input, buffer, sizeof buffer)) > 0) {
		ssize_t offset = 0;
		while (offset < count) {
			ssize_t written = write(output, buffer + offset, count - offset);
			if (written <= 0) {
				result = -1;
				goto finished;
			}
			offset += written;
		}
	}
	if (count < 0 || fchmod(output, 06755) != 0) result = -1;
finished:
	if (close(output) != 0) result = -1;
	if (close(input) != 0) result = -1;
	return result;
}

int verify_exec_security_image(int no_new_privs)
{
	uid_t real_uid, effective_uid, saved_uid;
	gid_t real_gid, effective_gid, saved_gid;
	uid_t expected_uid = no_new_privs ? 1000 : 0;
	gid_t expected_gid = no_new_privs ? 1000 : 0;
	return prctl(PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) == no_new_privs
	    && getresuid(&real_uid, &effective_uid, &saved_uid) == 0
	    && getresgid(&real_gid, &effective_gid, &saved_gid) == 0
	    && real_uid == 1000 && effective_uid == expected_uid && saved_uid == expected_uid
	    && real_gid == 1000 && effective_gid == expected_gid && saved_gid == expected_gid ? 0 : 91;
}

struct credential_probe {
	atomic_int phase;
	pid_t tid;
};

struct leader_exit_probe {
	atomic_int phase;
	int ready, finish;
};

static int leader_exited_with_one_worker(pid_t pid)
{
	char path[96], bytes[2048];
	snprintf(path, sizeof path, "/thread-proc/%ld/status", (long)pid);
	int fd = open(path, O_RDONLY | O_CLOEXEC);
	if (fd < 0) return 0;
	ssize_t count = read(fd, bytes, sizeof bytes - 1); close(fd);
	if (count <= 0) return 0;
	bytes[count] = 0;
	/* Linux 仍计入未 release_task 的 zombie leader；等待实际退出状态而非错误的 live-only 计数。 */
	return strstr(bytes, "State:\tZ") && strstr(bytes, "Threads:\t2\n");
}

static int status_has_identity(pid_t pid, pid_t tid, unsigned int uid, unsigned int gid)
{
	char path[96], bytes[2048], expected[96];
	if (tid) snprintf(path, sizeof path, "/thread-proc/%ld/task/%ld/status", (long)pid, (long)tid);
	else snprintf(path, sizeof path, "/thread-proc/%ld/status", (long)pid);
	int fd = open(path, O_RDONLY | O_CLOEXEC);
	if (fd < 0) return 0;
	ssize_t count = read(fd, bytes, sizeof bytes - 1); close(fd);
	if (count <= 0) return 0;
	bytes[count] = 0;
	snprintf(expected, sizeof expected, "Uid:\t%u\t%u\t%u\t%u\n", uid, uid, uid, uid);
	if (!strstr(bytes, expected)) return 0;
	snprintf(expected, sizeof expected, "Gid:\t%u\t%u\t%u\t%u\n", gid, gid, gid, gid);
	return strstr(bytes, expected) != 0;
}

static void *inherited_thread_credentials(void *token)
{
	gid_t groups[4];
	return getuid() == 1000 && geteuid() == 1000 && getgid() == 1001 && getegid() == 1001
	    && getgroups(4, groups) == 4 && groups[0] == 3 && groups[1] == 4 && groups[2] == 41 && groups[3] == 41 ? token : 0;
}

static void *raw_thread_credentials(void *argument)
{
	struct credential_probe *probe = argument;
	gid_t groups[] = {41, 3, 41, 4};
	long page = sysconf(_SC_PAGESIZE);
	char *boundary = mmap(0, page * 2, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	if (boundary == MAP_FAILED || mprotect(boundary + page, page, PROT_NONE) != 0) return 0;
	*(gid_t *)(boundary + page - sizeof(gid_t)) = (gid_t)-1;
	errno = 0;
	if (syscall(SYS_setgroups, 2, boundary + page - sizeof(gid_t)) != -1 || errno != EINVAL
	    || getgroups(0, 0) != 0 || munmap(boundary, page * 2) != 0) return 0;
	errno = 0;
	if (syscall(SYS_setuid, (uid_t)-1) != -1 || errno != EINVAL) return 0;
	if (syscall(SYS_setgroups, 4, groups) != 0 || syscall(SYS_setgid, 1001) != 0 || syscall(SYS_setuid, 1000) != 0) return 0;
	umask(077);
	probe->tid = syscall(SYS_gettid);
	atomic_store(&probe->phase, 1);
	struct timespec pause = { .tv_nsec = 1000000 };
	while (atomic_load(&probe->phase) != 2) nanosleep(&pause, 0);
	int fd = open("/thread-group-secret", O_RDONLY | O_CLOEXEC);
	char byte;
	if (fd < 0 || read(fd, &byte, 1) != 1 || byte != 'g' || close(fd) != 0) return 0;
	gid_t observed[4];
	if (getgroups(4, observed) != 4 || observed[0] != 3 || observed[1] != 4 || observed[2] != 41 || observed[3] != 41) return 0;
	struct rlimit previous, one;
	/* signal 同组免身份检查；显式 leader prlimit 必须核对 leader 身份。 */
	errno = 0;
	if (kill(getpid(), 0) != 0 || syscall(SYS_prlimit64, getpid(), RLIMIT_NPROC, 0, &previous) != -1 || errno != EPERM
	    || syscall(SYS_prlimit64, probe->tid, RLIMIT_NPROC, 0, &previous) != 0) return 0;
	if (getrlimit(RLIMIT_NPROC, &previous) != 0) return 0;
	one = previous; one.rlim_cur = 1;
	if (setrlimit(RLIMIT_NPROC, &one) != 0) return 0;
	errno = 0;
	if (fork() != -1 || errno != EAGAIN) return 0;
	pthread_t nested;
	if (pthread_create(&nested, 0, inherited_thread_credentials, argument) != EAGAIN) return 0;
	if (setrlimit(RLIMIT_NPROC, &previous) != 0) return 0;
	void *joined;
	if (pthread_create(&nested, 0, inherited_thread_credentials, argument) != 0 || pthread_join(nested, &joined) != 0 || joined != argument) return 0;
	pid_t child = fork();
	if (child == 0) _exit(inherited_thread_credentials(argument) == argument ? 0 : 1);
	int status;
	if (child < 0 || waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status) != 0) return 0;
	return argument;
}

static int verify_raw_thread_credential_isolation(void)
{
	pid_t child = fork();
	if (child == 0) {
		if (mkdir("/thread-proc", 0700) != 0 || mount("proc", "/thread-proc", "proc", 0, 0) != 0) _exit(1);
		int fd = open("/thread-group-secret", O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC, 0640);
		if (fd < 0 || write(fd, "g", 1) != 1 || fchown(fd, 2000, 41) != 0 || close(fd) != 0) _exit(2);
		struct credential_probe probe = {0};
		pthread_t worker;
		if (pthread_create(&worker, 0, raw_thread_credentials, &probe) != 0) _exit(3);
		struct timespec pause = { .tv_nsec = 1000000 };
		for (int attempt = 0; atomic_load(&probe.phase) != 1; attempt++) { if (attempt == 1000) _exit(4); nanosleep(&pause, 0); }
		if (getuid() != 0 || geteuid() != 0 || getgid() != 0 || getgroups(0, 0) != 0
		    || umask(022) != 077 || !status_has_identity(getpid(), probe.tid, 1000, 1001)
		    || !status_has_identity(getpid(), 0, 0, 0)) _exit(5);
		atomic_store(&probe.phase, 2);
		void *joined;
		if (pthread_join(worker, &joined) != 0 || joined != &probe) _exit(6);
		if (unlink("/thread-group-secret") != 0 || umount("/thread-proc") != 0 || rmdir("/thread-proc") != 0) _exit(7);
		_exit(0);
	}
	int status;
	if (child <= 0 || waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status) != 0) {
		printf("thread credentials child status=%d\n", child > 0 ? status : -1); return -1;
	}
	printf("LiteOS musl raw thread credentials and NPROC ok\n");
	return 0;
}

static void *libc_synchronized_credentials(void *argument)
{
	atomic_int *phase = argument;
	atomic_store(phase, 1);
	struct timespec pause = { .tv_nsec = 1000000 };
	while (atomic_load(phase) != 2) nanosleep(&pause, 0);
	gid_t groups[2];
	return getuid() == 1000 && geteuid() == 1000 && getgid() == 1001 && getgroups(2, groups) == 2 && groups[0] == 17 && groups[1] == 31 ? argument : 0;
}

static int verify_libc_credential_synchronization(void)
{
	pid_t child = fork();
	if (child == 0) {
		atomic_int phase = 0;
		pthread_t worker;
		if (pthread_create(&worker, 0, libc_synchronized_credentials, &phase) != 0) _exit(1);
		struct timespec pause = { .tv_nsec = 1000000 };
		while (atomic_load(&phase) != 1) nanosleep(&pause, 0);
		gid_t groups[] = {31, 17};
		if (setgroups(2, groups) != 0 || setgid(1001) != 0 || setuid(1000) != 0) _exit(2);
		atomic_store(&phase, 2);
		void *joined;
		if (pthread_join(worker, &joined) != 0 || joined != &phase) _exit(3);
		_exit(0);
	}
	int status;
	if (child <= 0 || waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status) != 0) return -1;
	printf("LiteOS musl libc thread credential synchronization ok\n");
	return 0;
}

static void *survive_credential_leader(void *argument)
{
	struct leader_exit_probe *probe = argument;
	if (syscall(SYS_setgid, 1000) != 0 || syscall(SYS_setuid, 1000) != 0) _exit(1);
	atomic_store(&probe->phase, 1);
	struct timespec pause = { .tv_nsec = 1000000 };
	for (int attempt = 0; !leader_exited_with_one_worker(getpid()); attempt++) {
		if (attempt == 1000) _exit(2);
		nanosleep(&pause, 0);
	}
	char path[96], bytes[128];
	const char *links[] = { "exe", "root", "cwd" };
	for (size_t i = 0; i < sizeof links / sizeof links[0]; ++i) {
		snprintf(path, sizeof path, "/thread-proc/%ld/%s", (long)getpid(), links[i]);
		errno = 0;
		if (readlink(path, bytes, sizeof bytes) != -1 || errno != ENOENT) _exit(7);
	}
	pid_t tid = syscall(SYS_gettid);
	if (!status_has_identity(getpid(), 0, 0, 0) || !status_has_identity(getpid(), tid, 1000, 1000)
	    || write(probe->ready, &tid, sizeof tid) != sizeof tid) _exit(3);
	char byte;
	if (read(probe->finish, &byte, 1) != 1) _exit(4);
	_exit(0);
}

static int verify_credential_leader_retirement(void)
{
	if (mkdir("/thread-proc", 0755) != 0 || mount("proc", "/thread-proc", "proc", 0, 0) != 0) {
		dprintf(2, "credential retirement mount errno=%d\n", errno); return -1;
	}
	int ready[2], finish[2];
	if (pipe(ready) != 0 || pipe(finish) != 0) return -1;
	pid_t child = fork();
	if (child == 0) {
		close(ready[0]); close(finish[1]);
		struct leader_exit_probe probe = { .ready = ready[1], .finish = finish[0] };
		pthread_t worker;
		if (pthread_create(&worker, 0, survive_credential_leader, &probe) != 0) _exit(5);
		struct timespec pause = { .tv_nsec = 1000000 };
		while (atomic_load(&probe.phase) != 1) nanosleep(&pause, 0);
		/* 仅退出 leader；存活 Thread 继续使用 stack 上的 probe，旧 stack VMA仍属于 mm。 */
		syscall(SYS_exit, 0);
		_exit(6);
	}
	close(ready[1]); close(finish[0]);
	pid_t tid;
	if (child < 0 || read(ready[0], &tid, sizeof tid) != sizeof tid) {
		int status = -1; if (child > 0) waitpid(child, &status, 0);
		dprintf(2, "credential retirement worker status=%d errno=%d\n", status, errno); return -1;
	}
	pid_t checker = fork();
	if (checker == 0) {
		if (setgid(1000) != 0 || setuid(1000) != 0) _exit(1);
		errno = 0;
		if (kill(child, 0) != -1 || errno != EPERM) _exit(2);
		struct rlimit limits;
		errno = 0;
		if (syscall(SYS_prlimit64, child, RLIMIT_NPROC, 0, &limits) != -1 || errno != EPERM) _exit(3);
		if (syscall(SYS_tgkill, child, tid, 0) != 0) _exit(4);
		char path[96], bytes[128];
		const char *links[] = { "exe", "root", "cwd" };
		for (size_t i = 0; i < sizeof links / sizeof links[0]; ++i) {
			snprintf(path, sizeof path, "/thread-proc/%ld/%s", (long)child, links[i]);
			errno = 0;
			ssize_t length = readlink(path, bytes, sizeof bytes);
			if (length != -1 || errno != EACCES) {
				dprintf(2, "credential retirement %s length=%zd errno=%d\n", links[i], length, errno); _exit(5);
			}
		}
		_exit(0);
	}
	int status;
	if (checker < 0 || waitpid(checker, &status, 0) != checker || !WIFEXITED(status) || WEXITSTATUS(status) != 0) {
		dprintf(2, "credential retirement checker status=%d errno=%d\n", checker > 0 ? status : -1, errno); return -1;
	}
	if (write(finish[1], "x", 1) != 1 || waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status) != 0) return -1;
	close(ready[0]); close(finish[1]);
	if (umount("/thread-proc") != 0 || rmdir("/thread-proc") != 0) {
		dprintf(2, "credential retirement unmount errno=%d\n", errno); return -1;
	}
	printf("LiteOS musl credential leader retirement ok\n");
	return 0;
}

int verify_process_security(void)
{
	if (verify_fs_context() != 0) return 10;
	if (verify_raw_thread_credential_isolation() != 0 || verify_libc_credential_synchronization() != 0
	    || verify_credential_leader_retirement() != 0) return 9;
	if (prctl(PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) != 0) return 1;
	for (int extra = 0; extra < 4; extra++) {
		unsigned long args[4] = { 0, 0, 0, 0 };
		args[extra] = 1;
		errno = 0;
		if (prctl(PR_GET_NO_NEW_PRIVS, args[0], args[1], args[2], args[3]) != -1
		    || errno != EINVAL) return 2;
	}
	errno = 0;
	if (prctl(PR_SET_NO_NEW_PRIVS, 0, 0, 0, 0) != -1 || errno != EINVAL) return 3;
	pthread_t thread;
	void *joined;
	int token;
	if (pthread_create(&thread, 0, set_thread_no_new_privs, &token) != 0
	    || pthread_join(thread, &joined) != 0 || joined != &token
	    || prctl(PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) != 0) return 4;
	if (copy_setid_executable() != 0) {
		unlink("/security-exec");
		return 5;
	}
	/* 正常 set-ID 必须确实生效；否则 loader 忽略所有 set-ID 时 NNP 用例也会假通过。 */
	pid_t control = fork();
	if (control == 0) {
		if (setgid(1000) != 0 || setuid(1000) != 0) _exit(8);
		execl("/security-exec", "security-exec", "set-id-child", (char *)0);
		_exit(9);
	}
	int control_status;
	if (control < 0 || waitpid(control, &control_status, 0) != control
	    || !WIFEXITED(control_status) || WEXITSTATUS(control_status) != 0) {
		unlink("/security-exec");
		return 8;
	}
	pid_t child = fork();
	if (child == 0) {
		if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0
		    || prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0) _exit(1);
		errno = 0;
		if (prctl(PR_SET_NO_NEW_PRIVS, 0, 0, 0, 0) != -1 || errno != EINVAL) _exit(2);
		if (pthread_create(&thread, 0, inherited_no_new_privs, &token) != 0
		    || pthread_join(thread, &joined) != 0 || joined != &token) _exit(3);
		pid_t grandchild = fork();
		if (grandchild == 0) _exit(prctl(PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) == 1 ? 0 : 1);
		int status;
		if (grandchild < 0 || waitpid(grandchild, &status, 0) != grandchild
		    || !WIFEXITED(status) || WEXITSTATUS(status) != 0) _exit(4);
		if (setgid(1000) != 0 || setuid(1000) != 0) _exit(5);
		/* 普通 UID 的 reset 必须在 firmware seam 之前失败；否则 guest 会提前关机。 */
		errno = 0;
		if (syscall(SYS_reboot, 0xfee1deadUL, 0x28121969UL, 0x4321fedcUL, 0) != -1
		    || errno != EPERM) _exit(6);
		execl("/security-exec", "security-exec", "no-new-privs-child", (char *)0);
		_exit(7);
	}
	int status;
	int result = child < 0 || waitpid(child, &status, 0) != child
	    || !WIFEXITED(status) || WEXITSTATUS(status) != 0
	    || prctl(PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) != 0;
	if (unlink("/security-exec") != 0) return 7;
	return result ? 6 : 0;
}
