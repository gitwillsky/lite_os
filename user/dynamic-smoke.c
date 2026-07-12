#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <grp.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/random.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static int verify_shared_mapping(void)
{
    static const char persisted[] = "LITEOS_SHARED_PERSIST_45\n";
    unlink("/shared-persist");
    int fd = open("/shared-persist", O_CREAT | O_EXCL | O_RDWR, 0644);
    if (fd < 0 || ftruncate(fd, 4096) != 0) return 50;
    char *shared = mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (shared == MAP_FAILED) return 51;
    memcpy(shared, persisted, sizeof(persisted) - 1);
    char observed[sizeof(persisted)] = { 0 };
    if (lseek(fd, 0, SEEK_SET) != 0
        || read(fd, observed, sizeof(persisted) - 1) != sizeof(persisted) - 1
        || memcmp(observed, persisted, sizeof(persisted) - 1) != 0) return 52;
    static const char direct[] = "direct-45";
    if (lseek(fd, 64, SEEK_SET) != 64 || write(fd, direct, sizeof(direct)) != sizeof(direct)
        || memcmp(shared + 64, direct, sizeof(direct)) != 0) return 53;
    pid_t child = fork();
    if (child == 0) {
        memcpy(shared + 128, "fork-45", 8);
        _exit(0);
    }
    int status;
    if (child <= 0 || waitpid(child, &status, 0) != child || !WIFEXITED(status)
        || WEXITSTATUS(status) != 0 || memcmp(shared + 128, "fork-45", 8) != 0
        || msync(shared, 4096, MS_SYNC) != 0 || munmap(shared, 8192) != 0) return 54;
    child = fork();
    if (child == 0) {
        char *eof = mmap(NULL, 8192, PROT_READ, MAP_SHARED, fd, 0);
        if (eof == MAP_FAILED) _exit(1);
        volatile char byte = eof[4096];
        (void)byte;
        _exit(2);
    }
    if (child <= 0 || waitpid(child, &status, 0) != child
        || !WIFSIGNALED(status) || WTERMSIG(status) != SIGBUS || close(fd) != 0) return 55;
    puts("LITEOS_SHARED_MMAP_45");
    return 0;
}

static int shared_crash_loop(void)
{
    int fd = open("/shared-crash", O_CREAT | O_RDWR, 0644);
    if (fd < 0 || ftruncate(fd, 4096) != 0) return 1;
    unsigned long *value = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (value == MAP_FAILED) return 2;
    puts("LITEOS_SHARED_CRASH_ACTIVE_45");
    for (;;) {
        ++*value;
        if (msync(value, 4096, MS_SYNC) != 0) return 3;
    }
}

static int verify_credentials(const char *program)
{
    uid_t ruid, euid, suid;
    gid_t rgid, egid, sgid;
    gid_t groups[] = { 1000, 2000 };
    unlink("/credential-owner");
    unlink("/credential-root");
    if (getresuid(&ruid, &euid, &suid) != 0 || ruid != 0 || euid != 0 || suid != 0
        || getresgid(&rgid, &egid, &sgid) != 0 || rgid != 0 || egid != 0 || sgid != 0
        || setgroups(2, groups) != 0 || getgroups(0, NULL) != 2
        || umask(0027) != 0022) return 10;
    int owner = open("/credential-owner", O_CREAT | O_EXCL | O_RDWR, 0666);
    int root = open("/credential-root", O_CREAT | O_EXCL | O_RDWR, 0600);
    struct stat metadata;
    if (owner < 0 || root < 0 || fstat(owner, &metadata) != 0
        || (metadata.st_mode & 0777) != 0640 || fchmodat(AT_FDCWD, "/credential-owner", 0660, 0) != 0
        || fchownat(AT_FDCWD, "/credential-owner", 1000, 1000, 0) != 0
        || chmod(program, 04755) != 0 || close(owner) != 0 || close(root) != 0) return 11;
    pid_t child = fork();
    if (child == 0) {
        if (setresgid(1000, 1000, 1000) != 0 || setresuid(1000, 1000, 1000) != 0) _exit(21);
        if (getresuid(&ruid, &euid, &suid) != 0 || ruid != 1000 || euid != 1000 || suid != 1000) _exit(26);
        if (stat("/credential-root", &metadata) != 0 || metadata.st_uid != 0 || (metadata.st_mode & 0777) != 0600) _exit(27);
        if (open("/credential-owner", O_RDWR) < 0) _exit(22);
        errno = 0;
        if (open("/credential-root", O_RDONLY) != -1 || errno != EACCES) _exit(23);
        errno = 0;
        if (kill(getppid(), 0) != -1 || errno != EPERM) _exit(24);
        execl(program, program, "setid-probe", (char *)0);
        _exit(25);
    }
    int status;
    if (child <= 0 || waitpid(child, &status, 0) != child || !WIFEXITED(status)) return 12;
    if (WEXITSTATUS(status) != 0) return 40 + WEXITSTATUS(status);
    puts("LITEOS_CREDENTIALS_44");
    return 0;
}

int main(int argc, char **argv)
{
    if (argc == 2 && strcmp(argv[1], "shared-crash") == 0) return shared_crash_loop();
    if (argc == 2 && strcmp(argv[1], "setid-probe") == 0) {
        uid_t real, effective, saved;
        return getresuid(&real, &effective, &saved) != 0 || real != 1000 || effective != 0 || saved != 0;
    }
    static const char payload[] = "link-abi";
    struct stat source_stat;
    struct stat hard_stat;
    struct stat symlink_stat;
    char link_target[32];
    unlink("/abi-source");
    unlink("/abi-hard");
    unlink("/abi-empty");
    unlink("/abi-symlink");
    unlink("/abi-symlink-hard");
    unlink("/abi-follow-hard");
    int source = open("/abi-source", O_CREAT | O_EXCL | O_RDWR, 0644);
    if (source < 0
        || write(source, payload, sizeof(payload)) != sizeof(payload)
        || linkat(AT_FDCWD, "/abi-source", AT_FDCWD, "/abi-hard", 0) != 0
        || linkat(source, "", AT_FDCWD, "/abi-empty", AT_EMPTY_PATH) != 0
        || symlinkat("abi-source", AT_FDCWD, "/abi-symlink") != 0
        || linkat(AT_FDCWD, "/abi-symlink", AT_FDCWD, "/abi-symlink-hard", 0) != 0
        || linkat(AT_FDCWD, "/abi-symlink", AT_FDCWD, "/abi-follow-hard", AT_SYMLINK_FOLLOW) != 0
        || fstat(source, &source_stat) != 0
        || stat("/abi-hard", &hard_stat) != 0
        || lstat("/abi-symlink-hard", &symlink_stat) != 0
        || source_stat.st_ino != hard_stat.st_ino
        || source_stat.st_nlink != 4
        || !S_ISLNK(symlink_stat.st_mode)
        || symlink_stat.st_nlink != 2
        || readlink("/abi-symlink-hard", link_target, sizeof(link_target)) != 10
        || memcmp(link_target, "abi-source", 10) != 0
        || close(source) != 0) {
        return 6;
    }
    struct timeval direct_time;
    struct timespec realtime;
    int timezone[2] = { -1, -1 };
    if (syscall(SYS_gettimeofday, &direct_time, timezone) != 0
        || clock_gettime(CLOCK_REALTIME, &realtime) != 0
        || direct_time.tv_usec < 0 || direct_time.tv_usec >= 1000000
        || realtime.tv_sec < direct_time.tv_sec
        || realtime.tv_sec > direct_time.tv_sec + 1
        || timezone[0] != 0 || timezone[1] != 0) {
        return 5;
    }
    struct statfs by_path;
    struct statfs by_descriptor;
    struct statfs pipe_statistics;
    int pipe_descriptors[2];
    int root = open("/", O_RDONLY);
    if (root < 0
        || statfs("/", &by_path) != 0
        || fstatfs(root, &by_descriptor) != 0
        || pipe(pipe_descriptors) != 0
        || fstatfs(pipe_descriptors[0], &pipe_statistics) != 0
        || close(root) != 0
        || close(pipe_descriptors[0]) != 0
        || close(pipe_descriptors[1]) != 0
        || by_path.f_type != 0xef53
        || by_path.f_blocks == 0
        || by_path.f_bfree > by_path.f_blocks
        || by_path.f_bavail > by_path.f_bfree
        || by_path.f_blocks != by_descriptor.f_blocks
        || by_path.f_bfree != by_descriptor.f_bfree
        || pipe_statistics.f_type != 0x50495045) {
        return 4;
    }
    unsigned char first[16];
    unsigned char second[16];
    if (getrandom(first, sizeof(first), 0) != sizeof(first)
        || getrandom(second, sizeof(second), GRND_NONBLOCK) != sizeof(second)
        || memcmp(first, second, sizeof(first)) == 0) {
        return 3;
    }
    void *handle = dlopen("/usr/lib/libliteos-smoke.so", RTLD_NOW | RTLD_LOCAL);
    if (handle == NULL) {
        puts(dlerror());
        return 1;
    }
    int (*value)(void) = (int (*)(void))dlsym(handle, "liteos_dynamic_value");
    if (value == NULL || value() != 42 || dlclose(handle) != 0) {
        return 2;
    }
    int credential_result = verify_credentials(argv[0]);
    if (credential_result != 0) return credential_result;
    int shared_result = verify_shared_mapping();
    if (shared_result != 0) return shared_result;
    puts("LITEOS_DLOPEN_42");
    return 0;
}
