#include <dlfcn.h>
#include <stdio.h>
#include <string.h>
#include <sys/random.h>

int main(void)
{
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
