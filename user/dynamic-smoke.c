#include <dlfcn.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/random.h>
#include <sys/statfs.h>
#include <unistd.h>

int main(void)
{
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
