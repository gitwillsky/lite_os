#include <stdint.h>
#include <stdlib.h>
#include <time.h>
#include <unistd.h>

int main(int argc, char **argv, char **envp)
{
	static const char message[] = "LiteOS musl static ok\n";
	struct timespec now;
	void *allocation;

	if (argc != 1 || !argv || !argv[0] || !envp || envp[0]) return 1;
	if (sysconf(_SC_PAGESIZE) != 4096 || getpid() <= 0) return 2;
	allocation = malloc(64);
	if (!allocation) return 3;
	*(volatile uint64_t *)allocation = UINT64_C(0x4c6974654f53);
	free(allocation);
	if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) return 4;
	if (write(STDOUT_FILENO, message, sizeof message - 1) != sizeof message - 1) return 5;
	return 0;
}
