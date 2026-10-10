#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/capability.h>
#include <linux/filter.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/auxv.h>
#include <sys/sendfile.h>
#include <sys/resource.h>
#include <grp.h>
#include <sys/mount.h>
#include <sys/prctl.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <sys/xattr.h>
#include <unistd.h>

static const uint64_t all_caps = (UINT64_C(1) << (CAP_LAST_CAP + 1)) - 1;
static uint64_t bit(int capability) { return UINT64_C(1) << capability; }
struct sets { uint64_t effective, permitted, inheritable; };
static int caps_get(pid_t pid, struct sets *sets)
{
	struct __user_cap_header_struct header = { _LINUX_CAPABILITY_VERSION_3, pid };
	struct __user_cap_data_struct data[2];
	if (syscall(SYS_capget, &header, data)) return -1;
	sets->effective = data[0].effective | ((uint64_t)data[1].effective << 32);
	sets->permitted = data[0].permitted | ((uint64_t)data[1].permitted << 32);
	sets->inheritable = data[0].inheritable | ((uint64_t)data[1].inheritable << 32);
	return 0;
}
static int caps_set(struct sets sets)
{
	struct __user_cap_header_struct header = { _LINUX_CAPABILITY_VERSION_3, 0 };
	struct __user_cap_data_struct data[2];
	for (int word = 0; word < 2; word++) {
		data[word].effective = sets.effective >> (word * 32);
		data[word].permitted = sets.permitted >> (word * 32);
		data[word].inheritable = sets.inheritable >> (word * 32);
	}
	return syscall(SYS_capset, &header, data);
}
static int child_ok(pid_t child)
{
	int status = -1;
	if (child > 0 && waitpid(child, &status, 0) == child && WIFEXITED(status) && !WEXITSTATUS(status)) return 1;
	printf("capabilities child status=%d\n", status);
	return 0;
}
static _Noreturn void fail(int step) { dprintf(2, "capabilities fail step=%d\n", step); _exit(step); }

static int version_and_order(void)
{
	char *pages = mmap(0, 8192, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	if (pages == MAP_FAILED || mprotect(pages + 4096, 4096, PROT_NONE)) return 0;
	uint32_t *version = (void *)(pages + 4092);
	*version = 0;
	/* Linux NULL-data discovery reads only version, writes preferred version, and succeeds. */
	if (syscall(SYS_capget, version, 0) || *version != _LINUX_CAPABILITY_VERSION_3 || syscall(SYS_capget, version, 0)) return 0;
	*version = 0; errno = 0;
	if (syscall(SYS_capset, version, 0) != -1 || errno != EINVAL || *version != _LINUX_CAPABILITY_VERSION_3) return 0;
	errno = 0;
	if (syscall(SYS_capset, version, 0) != -1 || errno != EFAULT) return 0;
	munmap(pages, 8192);
	struct __user_cap_header_struct header = { _LINUX_CAPABILITY_VERSION_3, -1 };
	errno = 0;
	if (syscall(SYS_capset, &header, 1) != -1 || errno != EPERM) return 0;
	struct __user_cap_data_struct data[2];
	errno = 0;
	if (syscall(SYS_capget, &header, data) != -1 || errno != EINVAL) return 0;
	header.version = _LINUX_CAPABILITY_VERSION_1; header.pid = 0;
	memset(data, 0xa5, sizeof data);
	if (syscall(SYS_capget, &header, data) || data[0].effective != UINT32_MAX || data[1].effective != 0xa5a5a5a5) return 0;
	return 1;
}

static int privilege_drop(void)
{
	pid_t child = fork();
	if (child == 0) {
		uint64_t denied = bit(CAP_SYS_BOOT) | bit(CAP_SYS_ADMIN) | bit(CAP_SETUID) | bit(CAP_SETGID)
		    | bit(CAP_NET_RAW) | bit(CAP_DAC_OVERRIDE) | bit(CAP_DAC_READ_SEARCH);
		if (caps_set((struct sets){ all_caps & ~denied, all_caps, 0 })) fail(61);
		errno = 0;
		if (syscall(SYS_reboot, 0, 0, 0, 0) != -1 || errno != EPERM) fail(62);
		errno = 0;
		if (syscall(SYS_swapon, 1, 0) != -1 || errno != EPERM || mount(0,0,0,0,0) != -1 || errno != EPERM) fail(63);
		errno = 0;
		if (syscall(SYS_setuid, 1000) != -1 || errno != EPERM || syscall(SYS_setgid, 1000) != -1 || errno != EPERM) fail(64);
		errno = 0;
		if (socket(AF_PACKET, SOCK_DGRAM, 0) != -1 || errno != EPERM) fail(65);
		errno = 0;
		if (open("/caps-secret", O_RDONLY) != -1 || errno != EACCES) fail(66);
		errno = 0;
		struct sock_fprog program = { .len = 1, .filter = (void *)1 };
		if (syscall(SYS_seccomp, 1, 0, &program) != -1 || errno != EACCES) fail(67);
		/* UID still 0; restoring effective from retained permitted is valid capset. */
		if (geteuid() || caps_set((struct sets){ all_caps, all_caps, 0 }) || open("/caps-secret",O_RDONLY) < 0) fail(68);
		_exit(0);
	}
	return child_ok(child);
}

struct thread_probe { atomic_int phase, tid; };
static void *private_caps(void *argument)
{
	struct thread_probe *probe = argument;
	if (caps_set((struct sets){ all_caps & ~bit(CAP_SYS_BOOT), all_caps, 0 })) return 0;
	atomic_store(&probe->tid, syscall(SYS_gettid));
	atomic_store(&probe->phase, 1);
	while (atomic_load(&probe->phase) != 2) sched_yield();
	return argument;
}
static int thread_owner(void)
{
	struct thread_probe probe = { .phase = 0, .tid = 0 };
	pthread_t thread;
	if (pthread_create(&thread, 0, private_caps, &probe)) return 0;
	while (!atomic_load(&probe.phase)) sched_yield();
	struct sets caller, worker;
	if (caps_get(0,&caller) || caller.effective != all_caps || caps_get(atomic_load(&probe.tid),&worker)
	    || worker.effective != (all_caps & ~bit(CAP_SYS_BOOT))) return 0;
	atomic_store(&probe.phase, 2);
	void *joined;
	return pthread_join(thread, &joined) == 0 && joined == &probe;
}

int verify_capabilities_image(const char *mode)
{
	struct sets sets;
	if (caps_get(0, &sets)) return 69;
	if (getauxval(AT_UID) != getuid() || getauxval(AT_EUID) != geteuid()
	    || getauxval(AT_GID) != getgid() || getauxval(AT_EGID) != getegid()) return 79;
	uint64_t expected = !strcmp(mode,"ambient") || !strcmp(mode,"sgid-group") ? bit(CAP_NET_BIND_SERVICE)
	    : !strcmp(mode,"nnp") || !strcmp(mode,"promoted") ? bit(CAP_KILL)
	    : !strcmp(mode,"root-regain") ? all_caps
	    : !strcmp(mode,"bounding") ? all_caps & ~bit(CAP_SYS_BOOT) : 0;
	if (sets.effective != expected || sets.permitted != expected || prctl(PR_GET_KEEPCAPS)) return 70;
	if (!strcmp(mode,"ambient") && (geteuid() != 1000 || prctl(PR_CAP_AMBIENT,PR_CAP_AMBIENT_IS_SET,CAP_NET_BIND_SERVICE,0,0) != 1)) return 71;
	int death = 0;
	if (prctl(PR_GET_PDEATHSIG,&death)) return 80;
	if (!strcmp(mode,"nnp-ids") && (geteuid()!=1000 || !getauxval(AT_SECURE) || death)) return 81;
	if ((!strcmp(mode,"same-suid") || !strcmp(mode,"sgid-no-x"))
	    && (geteuid()!=1000 || getegid()!=1000 || getauxval(AT_SECURE) || death!=SIGUSR1)) return 82;
	if (!strcmp(mode,"sgid-group") && (geteuid()!=1000 || getegid()!=2000 || !getauxval(AT_SECURE) || death
	    || prctl(PR_CAP_AMBIENT,PR_CAP_AMBIENT_IS_SET,CAP_NET_BIND_SERVICE,0,0)!=1)) return 83;
	if (!strcmp(mode,"root-regain") && (getauxval(AT_SECURE) || death)) return 84;
	if (!strcmp(mode,"promoted")) {
		char finish;
		if (getpid()!=syscall(SYS_gettid) || write(33,"R",1)!=1 || read(34,&finish,1)!=1) return 101;
	}
	return 0;
}
static int exec_sets(void)
{
	const char *modes[] = { "ambient", "nnp", "bounding" };
	for (int mode = 0; mode < 3; mode++) {
		pid_t child = fork();
		if (child == 0) {
			if (mode == 0) {
				uint64_t keep = bit(CAP_NET_BIND_SERVICE);
				if (prctl(PR_SET_KEEPCAPS,1) || caps_set((struct sets){all_caps,all_caps,keep})
				    || syscall(SYS_setresuid,1000,1000,1000) || caps_set((struct sets){0,keep,keep})
				    || prctl(PR_CAP_AMBIENT,PR_CAP_AMBIENT_RAISE,CAP_NET_BIND_SERVICE,0,0)) fail(72);
			} else if (mode == 1) {
				if (caps_set((struct sets){bit(CAP_KILL),bit(CAP_KILL),0}) || prctl(PR_SET_NO_NEW_PRIVS,1,0,0,0)) fail(73);
			} else if (prctl(PR_CAPBSET_DROP,CAP_SYS_BOOT)) fail(74);
			execl("/bin/init","init","capabilities-image",modes[mode],(char *)0);
			fail(75);
		}
		if (!child_ok(child)) return 0;
	}
	return 1;
}

static int copy_image(void)
{
	int input=open("/bin/init",O_RDONLY), output=open("/caps-exec-image",O_CREAT|O_TRUNC|O_WRONLY,0700);
	if (input<0 || output<0) return 0;
	char bytes[4096]; ssize_t count;
	while ((count=read(input,bytes,sizeof bytes))>0) if (write(output,bytes,count)!=count) return 0;
	int okay=count==0 && fchown(output,1000,2000)==0;
	close(input);close(output);return okay;
}

/* 标准vfs_cap_data revision2，小端字段；不借capset布局或私有capability ABI。 */
static const unsigned char file_bind_caps[20] = {
    1, 0, 0, 2, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0
};
int verify_file_capability_image(const char *mode)
{
    struct sets sets;
    if (caps_get(0, &sets) || getuid() != 1000 || getgid() != 1000) return 150;
    uint64_t permitted = !strcmp(mode, "nnp") ? 0 : bit(CAP_NET_BIND_SERVICE);
    uint64_t effective = !strcmp(mode, "aware") ? 0 : permitted;
    if (sets.permitted != permitted || sets.effective != effective || sets.inheritable
        || geteuid() != (!strcmp(mode, "mixed") ? 0 : 1000)
        || !getauxval(AT_SECURE) || getauxval(AT_EUID) != geteuid()) return 151;
    return 0;
}
static int file_capability_exec(void)
{
    if (!copy_image() || chown("/caps-exec-image", 0, 0)) return 0;
    const char *modes[] = { "effective", "aware", "nnp", "mixed", "bounding-denied" };
    for (int mode = 0; mode < 5; mode++) {
        unsigned char value[20]; memcpy(value, file_bind_caps, sizeof value);
        if (mode == 1) value[0] = 0;
        if (chmod("/caps-exec-image", mode == 3 ? 04755 : 0755)
            || setxattr("/caps-exec-image", "security.capability", value, sizeof value, 0)) return 0;
        pid_t child = fork();
        if (!child) {
            if (mode == 4 && prctl(PR_CAPBSET_DROP, CAP_NET_BIND_SERVICE)) fail(152);
            if (setgid(1000) || setuid(1000)) fail(153);
            if (mode == 2 && prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0)) fail(154);
            execl("/caps-exec-image", "init", "file-capability-image", modes[mode], (char *)0);
            if (mode != 4 || errno != EPERM) fail(155);
            struct sets unchanged;
            if (caps_get(0, &unchanged) || unchanged.permitted || unchanged.effective || geteuid() != 1000) fail(156);
            _exit(0);
        }
        if (!child_ok(child)) return 0;
    }
    return removexattr("/caps-exec-image", "security.capability") == 0;
}

static int xattr_operations(void)
{
    int fd = open("/caps-xattrs", O_CREAT | O_RDWR, 0666);
    unsigned char actual[32];
    if (fd < 0 || fchmod(fd, 0666) || fsetxattr(fd, "user.value", "old", 3, XATTR_CREATE)) return 0;
    errno = 0;
    if (fsetxattr(fd, "user.value", "new", 3, XATTR_CREATE | XATTR_REPLACE) != -1 || errno != EEXIST) return 0;
    errno = 0;
    if (fsetxattr(fd, "user.missing", "new", 3, XATTR_CREATE | XATTR_REPLACE) != -1 || errno != ENODATA) return 0;
    errno = 0;
    if (fgetxattr(fd, "user.value", actual, 2) != -1 || errno != ERANGE
        || fgetxattr(fd, "user.value", (void *)1, 0) != 3
        || fgetxattr(fd, "user.value", actual, sizeof actual) != 3 || memcmp(actual, "old", 3)) return 0;
    errno = 0;
    if (fsetxattr(fd, "user.value", (void *)1, 65537, 0) != -1 || errno != E2BIG) return 0;
    errno = 0;
    if (getxattr("/xattr-missing-path", "user.value", actual, sizeof actual) != -1 || errno != ENOENT) return 0;
    errno = 0;
    if (fgetxattr(-1, "", actual, sizeof actual) != -1 || errno != ERANGE) return 0;
    if (fsetxattr(fd, "user.empty", 0, 0, 0) || fgetxattr(fd, "user.empty", actual, sizeof actual) != 0
        || fsetxattr(fd, "trusted.admin", "hidden", 6, 0)) return 0;
    /* Linux asm-generic 463/464的标准xattr_args，不要求宿主musl headers先声明新版编号。 */
    struct { uint64_t value; uint32_t size, flags; } args = { (uintptr_t)"at", 2, 0 };
    _Static_assert(sizeof args == 16, "Linux xattr_args version0");
    if (syscall(463, fd, 0, AT_EMPTY_PATH, "user.at", &args, sizeof args)) return 0;
    args.value = (uintptr_t)actual; args.size = sizeof actual;
    if (syscall(464, fd, 0, AT_EMPTY_PATH, "user.at", &args, sizeof args) != 2 || memcmp(actual, "at", 2)) return 0;
    args.flags = 1; errno = 0;
    if (syscall(464, fd, 0, AT_EMPTY_PATH, "user.at", &args, sizeof args) != -1 || errno != EINVAL) return 0;
    pid_t child = fork();
    if (!child) {
        char names[256];
        if (setgid(1000) || setuid(1000)) fail(157);
        errno = 0;
        if (fgetxattr(fd, "trusted.admin", actual, sizeof actual) != -1 || errno != ENODATA) fail(158);
        ssize_t length = flistxattr(fd, names, sizeof names);
        if (length < 0) fail(159);
        for (ssize_t offset = 0; offset < length; offset += strlen(names + offset) + 1)
            if (!strcmp(names + offset, "trusted.admin")) fail(160);
        if (fsetxattr(fd, "security.capability", file_bind_caps, sizeof file_bind_caps, 0) != -1 || errno != EPERM) fail(161);
        _exit(0);
    }
    if (!child_ok(child)) return 0;
    /* 失败user-copy的已授权非零write仍须先提交killpriv。 */
    if (fsetxattr(fd, "security.capability", file_bind_caps, sizeof file_bind_caps, 0)) return 0;
    errno = 0;
    if (syscall(SYS_write, fd, (void *)1, 1) != -1 || errno != EFAULT) return 0;
    errno = 0;
    if (fgetxattr(fd, "security.capability", actual, sizeof actual) != -1 || errno != ENODATA) return 0;
    if (fsetxattr(fd, "security.capability", file_bind_caps, sizeof file_bind_caps, 0)
        || fchown(fd, -1, -1)) return 0;
    errno = 0;
    if (fgetxattr(fd, "security.capability", actual, sizeof actual) != -1 || errno != ENODATA) return 0;
    /* 真实 ext4 EA_INODE：ABI 最大值、短 buffer、替换、删除与最后 OFD 回收。 */
    unsigned char *large = malloc(65536), *copy = malloc(65536);
    if (!large || !copy) return 0;
    for (size_t index = 0; index < 65536; index++) large[index] = index % 251;
    if (fsetxattr(fd, "user.maximum", large, 65536, XATTR_CREATE)
        || fgetxattr(fd, "user.maximum", (void *)1, 0) != 65536
        || fgetxattr(fd, "user.maximum", copy, 65536) != 65536
        || memcmp(large, copy, 65536)) return 0;
    errno = 0;
    if (fgetxattr(fd, "user.maximum", copy, 65535) != -1 || errno != ERANGE) return 0;
    struct stat before, after;
    if (fstat(fd, &before) || fsetxattr(fd, "user.value", "old", 3, XATTR_REPLACE)
        || fstat(fd, &after) || before.st_ctim.tv_sec != after.st_ctim.tv_sec
        || before.st_ctim.tv_nsec != after.st_ctim.tv_nsec
        || fsetxattr(fd, "user.maximum", large, 8192, XATTR_REPLACE)
        || fgetxattr(fd, "user.maximum", copy, 65536) != 8192
        || memcmp(large, copy, 8192) || fremovexattr(fd, "user.maximum")) return 0;
    errno = 0;
    if (fremovexattr(fd, "user.maximum") != -1 || errno != ENODATA) return 0;
    free(copy); free(large);
    if (fremovexattr(fd, "user.value") || close(fd) || unlink("/caps-xattrs")) return 0;
    return 1;
}

static int memory_file_xattrs(void)
{
    if (mkdir("/caps-tmp", 0755) || mount("tmpfs", "/caps-tmp", "tmpfs", 0, "size=8m")) return 0;
    int descriptors[2] = { open("/caps-tmp/file", O_CREAT | O_RDWR, 0666), syscall(SYS_memfd_create, "xattrs", 0) };
    for (int item = 0; item < 2; item++) {
        int fd = descriptors[item];
        unsigned char actual[24];
        struct stat metadata;
        if (fd < 0 || fchmod(fd, 06777) || fsetxattr(fd, "user.keep", "data", 4, 0)
            || fsetxattr(fd, "security.capability", file_bind_caps, sizeof file_bind_caps, 0)
            || fgetxattr(fd, "security.capability", actual, sizeof actual) != 20
            || write(fd, "content", 7) != 7 || fstat(fd, &metadata) || (metadata.st_mode & 06000) != 06000) return 0;
        errno = 0;
        if (fgetxattr(fd, "security.capability", actual, sizeof actual) != -1 || errno != ENODATA
            || fgetxattr(fd, "user.keep", actual, sizeof actual) != 4 || memcmp(actual, "data", 4)) return 0;
        if (fsetxattr(fd, "security.capability", file_bind_caps, sizeof file_bind_caps, 0) || ftruncate(fd, 7)) return 0;
        errno = 0;
        if (fgetxattr(fd, "security.capability", actual, sizeof actual) != -1 || errno != ENODATA) return 0;
        if (fsetxattr(fd, "security.capability", file_bind_caps, sizeof file_bind_caps, 0) || fchown(fd, -1, -1)) return 0;
        errno = 0;
        if (fgetxattr(fd, "security.capability", actual, sizeof actual) != -1 || errno != ENODATA || close(fd)) return 0;
    }
    return unlink("/caps-tmp/file") == 0 && umount("/caps-tmp") == 0 && rmdir("/caps-tmp") == 0;
}
static int exec_identity_edges(void)
{
	if (!copy_image()) return 0;
	const char *modes[]={"root-regain","nnp-ids","same-suid","sgid-no-x","sgid-group"};
	for (int mode=0;mode<5;mode++) {
		if (chmod("/caps-exec-image",mode==4?02755:mode>=2?06745:0755)) return 0;
		pid_t child=fork();
		if (!child) {
			if (mode==1) {
				if (syscall(SYS_setresuid,1000,0,0) || caps_set((struct sets){0,0,0})
				    || prctl(PR_SET_NO_NEW_PRIVS,1,0,0,0)) fail(85);
			} else if (mode>=2) {
				uint64_t keep=bit(CAP_NET_BIND_SERVICE);
				gid_t group=2000;
				if (mode==4 && (setgroups(1,&group) || prctl(PR_SET_KEEPCAPS,1)
				    || caps_set((struct sets){all_caps,all_caps,keep}))) fail(86);
				if (syscall(SYS_setgid,1000) || syscall(SYS_setuid,1000)) fail(87);
				if (mode==4 && (caps_set((struct sets){0,keep,keep})
				    || prctl(PR_CAP_AMBIENT,PR_CAP_AMBIENT_RAISE,CAP_NET_BIND_SERVICE,0,0))) fail(88);
			} else if (caps_set((struct sets){0,0,0})) fail(89);
			if (prctl(PR_SET_PDEATHSIG,SIGUSR1)) fail(90);
			execl(mode>=2?"/caps-exec-image":"/bin/init","init","capabilities-image",modes[mode],(char *)0);
			fail(91);
		}
		if (!child_ok(child)) return 0;
	}
	return unlink("/caps-exec-image")==0;
}

/* The FD is already writable: killpriv must work even for a different inode owner. */
static int content_privilege_edges(const char *path)
{
	int fd=open(path,O_CREAT|O_RDWR|O_TRUNC,0666);
	int input=open("/bin/init",O_RDONLY);
	if (fd<0 || input<0 || write(fd,"x",1)!=1 || fchown(fd,2000,0)) return 0;
	for (int operation=0;operation<10;operation++) {
		mode_t mode=operation==6?02666:06777;
		if (fchmod(fd,mode)) return 0;
		pid_t child=fork();
		if (!child) {
			if (operation!=5 && caps_set((struct sets){all_caps&~bit(CAP_FSETID),all_caps,0})) fail(92);
			if (operation==7 && syscall(SYS_setuid,1000)) fail(93);
			struct stat status;
			if (operation==0) { if (write(fd,"",0)) fail(94); }
			else if (operation==1 || operation==5 || operation==6 || operation==7) {
				if (pwrite(fd,"y",1,0)!=1) fail(95);
			} else if (operation==2) { if (ftruncate(fd,1)) fail(96); }
			else if (operation==3) {
				int flags=fcntl(fd,F_GETFL); if (fcntl(fd,F_SETFL,flags|O_APPEND) || write(fd,"z",1)!=1) fail(97);
			} else if (operation==4) { off_t offset=0;if (sendfile(fd,input,&offset,1)!=1) fail(98); }
			else if (operation==8) { if (syscall(SYS_fallocate,fd,0,0,8192)) fail(114); }
			else {
				void *bad=mmap(0,4096,PROT_NONE,MAP_PRIVATE|MAP_ANONYMOUS,-1,0);errno=0;
				if (bad==MAP_FAILED || write(fd,bad,1)!=-1 || errno!=EFAULT) fail(115);
				munmap(bad,4096);
			}
			if (fstat(fd,&status)) fail(99);
			mode_t expected=operation==0||operation==5?06000:operation==6?02000:0;
			if ((status.st_mode&06000)!=expected) fail(100);
			_exit(0);
		}
		if (!child_ok(child)) return 0;
		if (fcntl(fd,F_SETFL,fcntl(fd,F_GETFL)&~O_APPEND) || ftruncate(fd,1)) return 0;
	}
	close(input);close(fd);return unlink(path)==0;
}
static int content_privileges(void)
{
	if (!content_privilege_edges("/caps-write-ext4")) return 0;
	if (mkdir("/caps-tmp",0755) || mount("tmpfs","/caps-tmp","tmpfs",0,"size=1m")) return 0;
	if (!content_privilege_edges("/caps-tmp/write")) return 0;
	return umount("/caps-tmp")==0 && rmdir("/caps-tmp")==0;
}

static void *promoted_worker(void *unused)
{
	(void)unused;
	char path[96], text[2048];
	snprintf(path,sizeof path,"/caps-proc/%d/status",getpid());
	for (int attempt=0;;attempt++) {
		int fd=open(path,O_RDONLY); if (fd<0 || attempt==2000) fail(102);
		ssize_t count=read(fd,text,sizeof text-1);close(fd);
		if (count<0) fail(103);text[count]=0;
		if (strstr(text,"State:\tZ")) break;
		struct timespec delay={.tv_nsec=1000000};nanosleep(&delay,0);
	}
	/* zombie leader 仍属于 Linux nr_threads；恰有一个存活 worker 时状态计数为 2。 */
	if (!strstr(text,"Threads:\t2\n")) fail(102);
	pid_t old_tid=syscall(SYS_gettid);
	uint64_t keep=bit(CAP_KILL);
	if (prctl(PR_SET_SECUREBITS,3) || caps_set((struct sets){keep,keep,keep})
	    || prctl(PR_CAP_AMBIENT,PR_CAP_AMBIENT_RAISE,CAP_KILL,0,0)
	    || write(33,&old_tid,sizeof old_tid)!=sizeof old_tid) fail(104);
	execl("/bin/init","init","capabilities-image","promoted",(char *)0);fail(105);
}
static int promoted_owner(void)
{
	int ready[2], finish[2];
	if (pipe(ready) || pipe(finish)) return 0;
	pid_t child=fork();
	if (!child) {
		pthread_t worker;
		if (dup2(ready[1],33)<0 || dup2(finish[0],34)<0 || pthread_create(&worker,0,promoted_worker,0)) fail(106);
		syscall(SYS_exit,0);fail(107);
	}
	pid_t old_tid;char marker;struct sets sets;
	if (child<0 || read(ready[0],&old_tid,sizeof old_tid)!=sizeof old_tid || read(ready[0],&marker,1)!=1
	    || marker!='R' || caps_get(child,&sets) || sets.effective!=bit(CAP_KILL) || sets.permitted!=bit(CAP_KILL)) return 0;
	errno=0;
	if (caps_get(old_tid,&sets)!=-1 || errno!=ESRCH || write(finish[1],"G",1)!=1) return 0;
	int okay=child_ok(child);
	close(ready[0]);close(ready[1]);close(finish[0]);close(finish[1]);return okay;
}
static int cross_process_authorization(void)
{
	pid_t probe=fork();
	if (!probe) {
		int finish[2];if (pipe(finish)) fail(108);
		pid_t target=fork();
		if (!target) {char done;if (read(finish[0],&done,1)!=1) fail(109);_exit(0);}
		if (target<0 || caps_set((struct sets){0,0,0})) fail(110);
		errno=0;if (setpriority(PRIO_PROCESS,target,5)!=-1 || errno!=EPERM) fail(111);
		char path[96],destination[128];snprintf(path,sizeof path,"/caps-proc/%d/exe",target);
		errno=0;if (readlink(path,destination,sizeof destination)!=-1 || errno!=EACCES) fail(112);
		if (write(finish[1],"G",1)!=1 || !child_ok(target)) fail(113);
		_exit(0);
	}
	return child_ok(probe);
}

static int zombie_owner(void)
{
	int ready[2];
	if (pipe(ready)) return 0;
	pid_t child = fork();
	if (child == 0) {
		if (caps_set((struct sets){ bit(CAP_KILL),bit(CAP_KILL),0 }) || write(ready[1],"Z",1) != 1) fail(76);
		_exit(0);
	}
	char marker;
	if (child < 0 || read(ready[0], &marker, 1) != 1) return 0;
	struct timespec delay = { .tv_nsec = 30000000 };
	nanosleep(&delay,0);
	struct sets sets;
	if (caps_get(child,&sets) || sets.effective != bit(CAP_KILL) || !child_ok(child)) return 0;
	errno = 0;
	int okay = caps_get(child,&sets) == -1 && errno == ESRCH;
	close(ready[0]); close(ready[1]);
	return okay;
}

int verify_capabilities(void)
{
	int fd = open("/caps-secret",O_CREAT|O_RDWR,0600);
	if (fd < 0 || fchown(fd,2000,2000) || fchmod(fd,0)) return 1;
	close(fd);
	if (!version_and_order()) return 2;
	if (!privilege_drop()) return 3;
	if (!thread_owner()) return 4;
	if (!exec_sets()) return 5;
	if (!zombie_owner()) return 6;
	if (!exec_identity_edges()) return 8;
	if (!content_privileges()) return 9;
	if (!xattr_operations()) return 14;
	if (!file_capability_exec()) return 15;
	if (!memory_file_xattrs()) return 16;
	if (mkdir("/caps-proc",0755) || mount("proc","/caps-proc","proc",0,0)) return 10;
	if (!promoted_owner()) return 11;
	if (!cross_process_authorization()) return 12;
	if (umount("/caps-proc") || rmdir("/caps-proc")) return 13;
	if (unlink("/caps-secret")) return 7;
	puts("LiteOS musl capabilities thread ownership UID exec and zombie query ok");
	return 0;
}
