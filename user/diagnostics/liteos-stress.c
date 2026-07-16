#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define MIB (1024UL * 1024UL)
#define PAGE_BYTES 4096UL
#define MAX_THREADS 64UL
#define MAX_MIB 96UL

struct cpu_job {
	uint64_t iterations;
	uint64_t seed;
	uint64_t result;
};

static uint64_t mix(uint64_t value)
{
	value += UINT64_C(0x9e3779b97f4a7c15);
	value = (value ^ (value >> 30)) * UINT64_C(0xbf58476d1ce4e5b9);
	value = (value ^ (value >> 27)) * UINT64_C(0x94d049bb133111eb);
	return value ^ (value >> 31);
}

static unsigned long parse_value(const char *text, unsigned long fallback,
				 unsigned long maximum)
{
	char *end = NULL;
	unsigned long value;

	if (text == NULL)
		return fallback;
	errno = 0;
	value = strtoul(text, &end, 10);
	if (errno != 0 || end == text || *end != '\0' || value == 0 || value > maximum) {
		fprintf(stderr, "invalid value: %s (expected 1..%lu)\n", text, maximum);
		exit(2);
	}
	return value;
}

static const char *program_name(const char *path)
{
	const char *slash = strrchr(path, '/');

	return slash == NULL ? path : slash + 1;
}

static double elapsed_seconds(const struct timespec *start, const struct timespec *end)
{
	return (double)(end->tv_sec - start->tv_sec) +
	       (double)(end->tv_nsec - start->tv_nsec) / 1000000000.0;
}

static void *cpu_worker(void *argument)
{
	struct cpu_job *job = argument;
	uint64_t value = job->seed;

	for (uint64_t index = 0; index < job->iterations; ++index)
		value = mix(value ^ index);
	job->result = value;
	return NULL;
}

static int run_cputest(int argc, char **argv)
{
	long online = sysconf(_SC_NPROCESSORS_ONLN);
	unsigned long default_threads = online > 0 ? (unsigned long)online : 1;
	unsigned long threads = parse_value(argc > 1 ? argv[1] : NULL,
					    default_threads, MAX_THREADS);
	unsigned long millions = parse_value(argc > 2 ? argv[2] : NULL, 2, 1000);
	pthread_t workers[MAX_THREADS];
	struct cpu_job jobs[MAX_THREADS];
	struct timespec start;
	struct timespec end;
	uint64_t checksum = 0;

	if (clock_gettime(CLOCK_MONOTONIC, &start) != 0) {
		perror("cputest: clock_gettime");
		return 1;
	}
	for (unsigned long index = 0; index < threads; ++index) {
		jobs[index] = (struct cpu_job) {
			.iterations = millions * UINT64_C(1000000),
			.seed = mix(index + 1),
			.result = 0,
		};
		int error = pthread_create(&workers[index], NULL, cpu_worker, &jobs[index]);
		if (error != 0) {
			errno = error;
			perror("cputest: pthread_create");
			return 1;
		}
	}
	for (unsigned long index = 0; index < threads; ++index) {
		int error = pthread_join(workers[index], NULL);
		if (error != 0) {
			errno = error;
			perror("cputest: pthread_join");
			return 1;
		}
		checksum ^= jobs[index].result;
	}
	if (clock_gettime(CLOCK_MONOTONIC, &end) != 0) {
		perror("cputest: clock_gettime");
		return 1;
	}
	printf("cputest ok: %lu threads, %lu M iterations/thread, %.3f s, checksum=%" PRIx64 "\n",
	       threads, millions, elapsed_seconds(&start, &end), checksum);
	return 0;
}

static void fill_pages(uint8_t *memory, size_t bytes, uint64_t salt)
{
	for (size_t offset = 0; offset < bytes; offset += PAGE_BYTES)
		*(uint64_t *)(memory + offset) = mix(offset / PAGE_BYTES + salt);
}

static int verify_pages(const uint8_t *memory, size_t bytes, uint64_t salt)
{
	for (size_t offset = 0; offset < bytes; offset += PAGE_BYTES) {
		uint64_t expected = mix(offset / PAGE_BYTES + salt);
		if (*(const uint64_t *)(memory + offset) != expected) {
			fprintf(stderr, "page mismatch at offset %zu\n", offset);
			return -1;
		}
	}
	return 0;
}

static int verify_cow_pages(const uint8_t *memory, size_t bytes, uint64_t salt)
{
	for (size_t offset = 0; offset < bytes; offset += PAGE_BYTES) {
		uint64_t expected = mix(offset / PAGE_BYTES + salt);
		if ((offset / PAGE_BYTES) % 2 == 0)
			expected ^= UINT64_MAX;
		if (*(const uint64_t *)(memory + offset) != expected)
			return -1;
	}
	return 0;
}

static int run_memtest(int argc, char **argv)
{
	unsigned long mib = parse_value(argc > 1 ? argv[1] : NULL, 48, MAX_MIB);
	size_t bytes = mib * MIB;
	uint8_t *memory = mmap(NULL, bytes, PROT_READ | PROT_WRITE,
			       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);

	if (memory == MAP_FAILED) {
		perror("memtest: mmap");
		return 1;
	}
	fill_pages(memory, bytes, 17);
	if (verify_pages(memory, bytes, 17) != 0)
		return 1;

	pid_t child = fork();
	if (child < 0) {
		perror("memtest: fork");
		return 1;
	}
	if (child == 0) {
		for (size_t offset = 0; offset < bytes; offset += PAGE_BYTES * 2)
			*(uint64_t *)(memory + offset) ^= UINT64_MAX;
		_exit(verify_cow_pages(memory, bytes, 17) == 0 ? 0 : 1);
	}
	int status = 0;
	if (waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status) != 0) {
		fprintf(stderr, "memtest: child failed\n");
		return 1;
	}
	if (verify_pages(memory, bytes, 17) != 0 || munmap(memory, bytes) != 0) {
		perror("memtest: parent verification");
		return 1;
	}
	printf("memtest ok: %lu MiB anonymous fault/write/verify and fork/COW\n", mib);
	return 0;
}

static int run_cachetest(int argc, char **argv)
{
	const char *path = "/tmp/liteos-cachetest.bin";
	unsigned long mib = parse_value(argc > 1 ? argv[1] : NULL, 40, 80);
	size_t bytes = mib * MIB;
	int descriptor = open(path, O_CREAT | O_TRUNC | O_RDWR, 0600);

	if (descriptor < 0 || ftruncate(descriptor, (off_t)bytes) != 0) {
		perror("cachetest: create");
		return 1;
	}
	uint8_t *mapping = mmap(NULL, bytes, PROT_READ | PROT_WRITE, MAP_SHARED, descriptor, 0);
	if (mapping == MAP_FAILED) {
		perror("cachetest: mmap shared");
		return 1;
	}
	fill_pages(mapping, bytes, 29);
	if (munmap(mapping, bytes) != 0) {
		perror("cachetest: munmap shared");
		return 1;
	}

	/* 1. 不调用 sync/fsync，让 dirty throttle/direct reclaim 自己保证进展。
	 * 2. 匿名压力迫使 cache 写回与回收，随后重新映射验证数据 owner 未丢失。 */
	size_t pressure_bytes = 32 * MIB;
	uint8_t *pressure = mmap(NULL, pressure_bytes, PROT_READ | PROT_WRITE,
				 MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	if (pressure == MAP_FAILED) {
		perror("cachetest: pressure mmap");
		return 1;
	}
	fill_pages(pressure, pressure_bytes, 41);
	mapping = mmap(NULL, bytes, PROT_READ, MAP_SHARED, descriptor, 0);
	if (mapping == MAP_FAILED || verify_pages(mapping, bytes, 29) != 0) {
		perror("cachetest: verify mmap");
		return 1;
	}
	if (munmap(mapping, bytes) != 0 || munmap(pressure, pressure_bytes) != 0 ||
	    close(descriptor) != 0 || unlink(path) != 0) {
		perror("cachetest: cleanup");
		return 1;
	}
	printf("cachetest ok: %lu MiB MAP_SHARED dirty + 32 MiB reclaim pressure\n", mib);
	return 0;
}

static void usage(const char *name)
{
	fprintf(stderr,
		"usage: %s {cputest [threads [M-iterations]]|memtest [MiB]|cachetest [MiB]}\n",
		name);
}

int main(int argc, char **argv)
{
	const char *name = program_name(argv[0]);

	if (strcmp(name, "liteos-stress") == 0) {
		if (argc < 2) {
			usage(name);
			return 2;
		}
		name = argv[1];
		--argc;
		++argv;
	}
	if (strcmp(name, "cputest") == 0)
		return run_cputest(argc, argv);
	if (strcmp(name, "memtest") == 0)
		return run_memtest(argc, argv);
	if (strcmp(name, "cachetest") == 0)
		return run_cachetest(argc, argv);
	usage(program_name(argv[0]));
	return 2;
}
