#include <dlfcn.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/random.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>

int main(void)
{
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
    puts("LITEOS_DLOPEN_42");
    return 0;
}
