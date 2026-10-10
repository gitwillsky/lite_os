#define _GNU_SOURCE
#include <errno.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

/* 实际 mmap 安装事务必须区分内核选址的 hint 与不可移动的 fixed 请求。
 * 页尾 canary 同时证明非整页长度按完整两页映射，fixed replacement 不损坏相邻页。 */
int verify_mapping_placement(void)
{
    const size_t page = 4096;
    unsigned char *original = mmap(NULL, page + 1, PROT_READ | PROT_WRITE,
                                  MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (original == MAP_FAILED) return 1;
    original[0] = 0x53;
    original[page] = 0xa7;
    unsigned char *fallback = mmap(original + 1, page, PROT_READ | PROT_WRITE,
                                  MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    int failed = 0;
    if (fallback == MAP_FAILED || fallback == original) failed = 2;
    errno = 0;
    if (!failed && (mmap(original, page, PROT_READ | PROT_WRITE,
                        MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE, -1, 0)
                    != MAP_FAILED || errno != EEXIST
                    || original[0] != 0x53 || original[page] != 0xa7)) failed = 3;
    errno = 0;
    if (!failed && (mmap(NULL, page, PROT_READ | PROT_WRITE,
                        MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0)
                    != MAP_FAILED || errno != EINVAL)) failed = 4;
    if (!failed && (mmap(original, page, PROT_READ | PROT_WRITE,
                        MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0)
                    != original || original[0] != 0 || original[page] != 0xa7)) failed = 5;
    if (fallback != MAP_FAILED && munmap(fallback, page)) failed = 6;
    if (munmap(original, page * 2)) failed = 7;
    if (failed) printf("LiteOS musl mmap placement failed step=%d\n", failed);
    else puts("LiteOS musl mmap placement ok");
    return failed;
}

/* More simultaneously live, actually activated roots than an 8-bit hardware ASID namespace.
 * Each child warms the same virtual address with distinct private data before waiting. */
int verify_address_space_rollover(void)
{
    enum { CHILDREN = 272 };
    const uint64_t parent_value = UINT64_C(0x5a31cafe81927364);
    volatile uint64_t *page = mmap(0, 4096, PROT_READ | PROT_WRITE,
                                 MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    int ready[2] = { -1, -1 }, resume[2] = { -1, -1 };
    pid_t children[CHILDREN];
    int count = 0, failed = 0;
    cpu_set_t possible;
    if (page == MAP_FAILED) return 1;
    *page = parent_value;
    if (pipe(ready) != 0 || pipe(resume) != 0
        || sched_getaffinity(0, sizeof possible, &possible) != 0) {
        failed = 2;
        goto cleanup;
    }
    for (; count < CHILDREN; count++) {
        pid_t child = fork();
        if (child < 0) { failed = 3; goto cleanup; }
        if (child == 0) {
            close(ready[0]);
            close(resume[1]);
            cpu_set_t selected;
            CPU_ZERO(&selected);
            CPU_SET((count % 2 && CPU_ISSET(1, &possible)) ? 1 : 0, &selected);
            if (sched_setaffinity(0, sizeof selected, &selected) != 0) _exit(40);
            const pid_t own_pid = getpid();
            const uint64_t canary = parent_value ^ ((uint64_t)count + 1);
            *page = canary;
            if (write(ready[1], "R", 1) != 1) _exit(41);
            close(ready[1]);
            char byte;
            if (read(resume[0], &byte, 1) != 1) _exit(42);
            close(resume[0]);
            for (int iteration = 0; iteration < 128; iteration++) {
                if (*page != canary || getpid() != own_pid || sched_yield() != 0) _exit(43);
                *page = canary;
            }
            _exit(0);
        }
        children[count] = child;
        if ((count + 1) % 64 == 0) printf("LiteOS musl ASID roots=%d\n", count + 1);
    }
    close(ready[1]); ready[1] = -1;
    close(resume[0]); resume[0] = -1;
    for (int index = 0; index < CHILDREN; index++) {
        char byte;
        if (read(ready[0], &byte, 1) != 1 || byte != 'R') { failed = 4; goto cleanup; }
    }
    /* Exercise the real 16-bit hardware limit without keeping 65K processes resident.
     * The 272 original roots keep their warmed private pages while new roots activate/retire. */
    for (int generation_root = 0; generation_root < 65536; generation_root++) {
        pid_t transient = fork();
        if (transient == 0) {
            const uint64_t canary = parent_value ^ (UINT64_C(0x100000000) + generation_root);
            if (*page != parent_value) _exit(44);
            *page = canary;
            if (sched_yield() != 0 || *page != canary) _exit(45);
            _exit(0);
        }
        int status;
        if (transient < 0 || waitpid(transient, &status, 0) != transient
            || !WIFEXITED(status) || WEXITSTATUS(status) != 0 || *page != parent_value) {
            failed = 8;
            goto cleanup;
        }
        if ((generation_root + 1) % 1024 == 0)
            printf("LiteOS musl ASID generation roots=%d\n", generation_root + 1);
    }
    char release[CHILDREN];
    for (int index = 0; index < CHILDREN; index++) release[index] = 'G';
    if (write(resume[1], release, sizeof release) != sizeof release) { failed = 5; goto cleanup; }
    for (int index = 0; index < CHILDREN; index++) {
        int status;
        if (waitpid(children[index], &status, 0) != children[index]
            || !WIFEXITED(status) || WEXITSTATUS(status) != 0) failed = 6;
        children[index] = 0;
    }
    if (*page != parent_value) failed = 7;
cleanup:
    for (int index = 0; index < count; index++) {
        if (children[index] > 0) kill(children[index], SIGKILL);
    }
    for (int index = 0; index < count; index++) {
        if (children[index] > 0) waitpid(children[index], 0, 0);
    }
    for (int index = 0; index < 2; index++) {
        if (ready[index] >= 0) close(ready[index]);
        if (resume[index] >= 0) close(resume[index]);
    }
    munmap((void *)page, 4096);
    if (failed) printf("LiteOS musl ASID failed step=%d roots=%d\n", failed, count);
    else puts("LiteOS musl ASID ok");
    return failed;
}
