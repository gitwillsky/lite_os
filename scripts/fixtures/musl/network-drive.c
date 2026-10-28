/* 普通 syscall 消费真实 SMB 挂载；保护跨包读写、mmap、打开后 rename/unlink 与可执行映射寿命。 */
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>
#define CHECK(step, condition) do { if (!(condition)) {dprintf(2, "LITEOS_SMB_FAILED step=%d errno=%d\n", step, errno); exit(step);} } while (0)
int main(int argc, char **argv)
{
    CHECK(1, argc == 2 && getuid() != 0 && chdir(argv[1]) == 0);
    CHECK(2, mkdir("directory", 0700) == 0);
    int fd = open("directory/data", O_CREAT | O_EXCL | O_RDWR, 0600);
    CHECK(3, fd >= 0);
    size_t length = 1024 * 1024 + 37;
    unsigned char *input = malloc(length), *output = malloc(length);
    CHECK(4, input && output);
    for (size_t i = 0; i < length; i++) input[i] = (unsigned char)(i * 7 % 251);
    size_t done = 0;
    while (done < length) {ssize_t count = write(fd, input + done, length - done); CHECK(5, count > 0); done += (size_t)count;}
    CHECK(6, fsync(fd) == 0 && close(fd) == 0);
    fd = open("directory/data", O_RDWR);
    CHECK(7, fd >= 0);
    done = 0;
    while (done < length) {ssize_t count = pread(fd, output + done, length - done, (off_t)done); CHECK(8, count > 0); done += (size_t)count;}
    CHECK(9, memcmp(input, output, length) == 0 && pread(fd, output, 1, (off_t)length) == 0);
    unsigned char *mapping = mmap(NULL, length, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    CHECK(10, mapping != MAP_FAILED && mapping[length - 1] == input[length - 1]);
    memcpy(mapping + 4093, "MMAP-BOUNDARY", 13);
    CHECK(11, msync(mapping, length, MS_SYNC) == 0 && munmap(mapping, length) == 0);
    CHECK(12, pread(fd, output, 13, 4093) == 13 && memcmp(output, "MMAP-BOUNDARY", 13) == 0);
    CHECK(13, rename("directory/data", "directory/renamed") == 0);
    CHECK(14, ftruncate(fd, 123) == 0 && fsync(fd) == 0 && close(fd) == 0);
    struct stat info;
    CHECK(15, stat("directory/renamed", &info) == 0 && info.st_size == 123);
    fd = open("directory/renamed", O_RDONLY);
    CHECK(16, fd >= 0 && unlink("directory/renamed") == 0 && read(fd, output, 4) == 4 && memcmp(input, output, 4) == 0 && close(fd) == 0);
    CHECK(17, rmdir("directory") == 0);
    int source = open("/bin/true", O_RDONLY);
    fd = open("execute", O_CREAT | O_EXCL | O_WRONLY, 0700);
    CHECK(18, source >= 0 && fd >= 0);
    for (;;) {
        ssize_t count = read(source, input, length); CHECK(19, count >= 0); if (!count) break;
        done = 0; while (done < (size_t)count) {ssize_t written = write(fd, input + done, (size_t)count - done); CHECK(20, written > 0); done += (size_t)written;}
    }
    CHECK(21, close(source) == 0 && fsync(fd) == 0 && close(fd) == 0);
    pid_t child = fork(); CHECK(22, child >= 0);
    if (!child) {execl("./execute", "true", (char *)NULL); _exit(127);}
    int status;
    CHECK(23, waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0);
    CHECK(24, unlink("execute") == 0);
    free(input); free(output);
    puts("LITEOS_SMB_FILES_42");
    return 0;
}
