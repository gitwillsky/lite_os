/*
 * 调度延迟诊断程序：量化“CPU 刚空下来、另一个 CPU 上有任务在排队”时的等待，以及它在突发负载里造成的
 * 吞吐损失。它不判定通过与否，只输出数据；解读与理想基线在宿主侧的 diagnose_scheduler_latency.py。
 *
 *   lag TRIALS            实验 L：C 退出使 CPU1 空闲的瞬间，排队在 CPU0 上的 B 要多久才出现在 CPU1。
 *   make CPUS JOBS TRIALS 实验 M：一次启动 JOBS 个长度随机的 CPU 密集任务，输出每次的总耗时。
 *   lengths SEED JOBS     打印 make 模式用某个种子生成的任务长度，供宿主核对随机数生成器。
 *
 * 输出行前缀：SCHED_LAT_TRIAL 是逐次数据，SCHED_LAT_RESULT 是摘要，SCHED_LAT_DONE 标记全部输出完成。
 */
#define _GNU_SOURCE
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define MAX_JOBS 256

static uint64_t now_ns(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (uint64_t)t.tv_sec * 1000000000ull + (uint64_t)t.tv_nsec;
}

static uint64_t thread_cpu_ns(void) {
    struct timespec t;
    clock_gettime(CLOCK_THREAD_CPUTIME_ID, &t);
    return (uint64_t)t.tv_sec * 1000000000ull + (uint64_t)t.tv_nsec;
}

static void fail(const char *step) {
    printf("SCHED_LAT_RESULT error: %s\n", step);
    printf("SCHED_LAT_DONE\n");
    _exit(2);
}

/* 把 pid 的 affinity 设为 {a} 或 {a, b}；b 为负数表示只有 a。 */
static void pin(pid_t pid, int a, int b) {
    cpu_set_t set;
    CPU_ZERO(&set);
    CPU_SET(a, &set);
    if (b >= 0) CPU_SET(b, &set);
    if (sched_setaffinity(pid, sizeof set, &set) != 0) fail("sched_setaffinity");
}

static void *shared_page(void) {
    void *page = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (page == MAP_FAILED) fail("mmap");
    memset(page, 0, 4096);
    return page;
}

static int compare_u64(const void *left, const void *right) {
    uint64_t a = *(const uint64_t *)left, b = *(const uint64_t *)right;
    return a < b ? -1 : a > b;
}

/* ---- 实验 L ---------------------------------------------------------------------------- */

struct lag_shared {
    volatile uint64_t t_idle; /* C 退出前写入：CPU1 变空闲的时刻。 */
    volatile uint64_t t_seen; /* B 写入：从 t_idle 到首次在 CPU1 上被观测到的纳秒数。 */
    volatile int stop;
};

/* 返回 CPU1 变空闲到排队任务 B 首次在 CPU1 上被观测到的纳秒数；超时返回 UINT64_MAX。 */
static uint64_t lag_trial(unsigned jitter_us) {
    struct lag_shared *shared = shared_page();
    pin(0, 0, -1); /* 子进程继承 CPU0 */
    pid_t a = fork();
    if (a == 0) { /* A：占着 CPU0 的任务 */
        while (!shared->stop) {}
        _exit(0);
    }
    pid_t b = fork();
    if (b == 0) { /* B：排队在 CPU0 上，等 C 放宽它的 affinity */
        for (;;) {
            if (shared->stop) _exit(0);
            uint64_t idle = shared->t_idle;
            if (idle != 0) {
                uint64_t now = now_ns();
                if (sched_getcpu() == 1) {
                    shared->t_seen = now - idle;
                    shared->stop = 1;
                    _exit(0);
                }
                if (now - idle > 500000000ull) { /* 迁移不会发生就放弃，避免挂死 */
                    shared->t_seen = UINT64_MAX;
                    shared->stop = 1;
                    _exit(0);
                }
            }
        }
    }
    pid_t c = fork();
    if (c == 0) { /* C：在 CPU1 上跑一会儿，退出使 CPU1 空闲 */
        pin(0, 1, -1);
        uint64_t end = now_ns() + 150000000ull + (uint64_t)jitter_us * 1000ull;
        while (now_ns() < end) {}
        shared->t_idle = now_ns();
        pin(b, 0, 1); /* 此后 B 才被允许去 CPU1；没有任何调度器钩子会因此触发迁移 */
        _exit(0);
    }
    /* 父进程只阻塞等待，不抢 CPU0；放到 CPU1 上是因为 C 退出后它在那里醒来只占几微秒。 */
    pin(0, 1, -1);
    int status;
    waitpid(c, &status, 0);
    waitpid(b, &status, 0);
    waitpid(a, &status, 0);
    uint64_t seen = shared->t_seen;
    munmap(shared, 4096);
    return seen;
}

static void run_lag(int trials) {
    uint64_t *lags = calloc((size_t)trials, sizeof *lags);
    int count = 0, timeouts = 0;
    for (int i = 0; i < trials; i++) {
        /* 0..9.7 ms 的相位抖动：让事件落在 10 ms tick 周期的不同位置。 */
        uint64_t lag = lag_trial((unsigned)((i * 977) % 10000));
        if (lag == UINT64_MAX) timeouts++;
        else lags[count++] = lag;
    }
    qsort(lags, (size_t)count, sizeof *lags, compare_u64);
    uint64_t sum = 0;
    for (int i = 0; i < count; i++) sum += lags[i];
    printf("SCHED_LAT_RESULT lag n=%d timeouts=%d min_us=%llu p50_us=%llu p90_us=%llu max_us=%llu mean_us=%llu\n",
           count, timeouts, count ? lags[0] / 1000 : 0, count ? lags[count / 2] / 1000 : 0,
           count ? lags[(count * 9) / 10] / 1000 : 0, count ? lags[count - 1] / 1000 : 0,
           count ? sum / count / 1000 : 0);
    free(lags);
}

/* ---- 实验 M ---------------------------------------------------------------------------- */

static uint32_t next_random(uint32_t *state) {
    *state = *state * 1664525u + 1013904223u;
    return *state >> 8;
}

/* 与宿主侧 diagnose_scheduler_latency.job_lengths_ms 完全一致：20..119 ms 的 CPU 时间。 */
static void job_lengths(uint32_t seed, int jobs, uint64_t *length_ns) {
    uint32_t state = seed;
    for (int job = 0; job < jobs; job++)
        length_ns[job] = (20 + next_random(&state) % 100) * 1000000ull;
}

struct make_shared {
    volatile uint64_t finish[MAX_JOBS];
};

/* 一次启动 jobs 个任务并等它们全部结束，返回总耗时（微秒）。 */
static uint64_t make_trial(int jobs, uint32_t seed) {
    struct make_shared *shared = shared_page();
    uint64_t length[MAX_JOBS];
    job_lengths(seed, jobs, length);
    uint64_t start = now_ns();
    for (int job = 0; job < jobs; job++) {
        if (fork() == 0) {
            while (thread_cpu_ns() < length[job]) {}
            shared->finish[job] = now_ns();
            _exit(0);
        }
    }
    int status;
    for (int job = 0; job < jobs; job++) wait(&status);
    uint64_t last = 0;
    for (int job = 0; job < jobs; job++)
        if (shared->finish[job] > last) last = shared->finish[job];
    munmap(shared, 4096);
    return (last - start) / 1000;
}

static uint32_t trial_seed(int trial) {
    return 12345u + (uint32_t)trial * 7919u;
}

static void run_make(int cpus, int jobs, int trials) {
    for (int trial = 0; trial < trials; trial++) {
        printf("SCHED_LAT_TRIAL make cpus=%d jobs=%d seed=%u makespan_us=%llu\n", cpus, jobs,
               trial_seed(trial), make_trial(jobs, trial_seed(trial)));
    }
    printf("SCHED_LAT_RESULT make cpus=%d jobs=%d trials=%d\n", cpus, jobs, trials);
}

static void run_lengths(uint32_t seed, int jobs) {
    uint64_t length[MAX_JOBS];
    job_lengths(seed, jobs, length);
    printf("SCHED_LAT_RESULT lengths seed=%u jobs=%d ms=", seed, jobs);
    for (int job = 0; job < jobs; job++) printf("%s%llu", job ? "," : "", length[job] / 1000000ull);
    printf("\n");
}

int main(int argc, char **argv) {
    int jobs = argc >= 4 ? atoi(argv[3]) : 0;
    if (argc >= 3 && strcmp(argv[1], "lag") == 0) {
        run_lag(atoi(argv[2]));
    } else if (argc >= 5 && strcmp(argv[1], "make") == 0 && jobs > 0 && jobs <= MAX_JOBS) {
        run_make(atoi(argv[2]), jobs, atoi(argv[4]));
    } else if (argc >= 4 && strcmp(argv[1], "lengths") == 0 && jobs > 0 && jobs <= MAX_JOBS) {
        run_lengths((uint32_t)strtoul(argv[2], NULL, 10), jobs);
    } else {
        printf("SCHED_LAT_RESULT usage: lag TRIALS | make CPUS JOBS TRIALS | lengths SEED JOBS\n");
    }
    printf("SCHED_LAT_DONE\n");
    return 0;
}
