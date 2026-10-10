#include <fcntl.h>
#include <string.h>
#include <unistd.h>
#include <stdlib.h>
#include <sys/prctl.h>
int main(int argc, char **argv)
{
    if (argc == 3 && !strcmp(argv[1], "fs-credentials-dynamic"))
        return prctl(PR_GET_DUMPABLE,0,0,0,0) == atoi(argv[2]) ? 0 : 247;
    char cwd[32], byte;
    int fd = open("/target", O_RDONLY);
    int okay = fd >= 0 && read(fd, &byte, 1) == 1 && byte == 'J'
        && getcwd(cwd, sizeof cwd) && strcmp(cwd, "/") == 0;
    if (fd >= 0) close(fd);
    return okay ? 0 : 134;
}
