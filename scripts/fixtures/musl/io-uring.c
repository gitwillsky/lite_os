#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/io_uring.h>
#include <poll.h>
#include <stdio.h>
#include <signal.h>
#include <stdint.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <sys/uio.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

/* io_uring：setup/mmap 布局、NOP 提交与完成、批量/溢出顺序、init 失败语义、poll/epoll、阻塞等待与
 * 唤醒、signal 中断、probe 与错误码。返回检查点编号（0 表示全部通过），musl-smoke 把它放进退出码。 */
#define CHECK(n, condition) do { if (!(condition)) return (n); } while (0)

struct ring {
	int fd;
	unsigned sq_n, cq_n, sq_mask, cq_mask, tail;
	uint8_t *rings, *sqes;
	size_t rings_len, sqes_len;
	uint32_t *sq_head, *sq_tail, *sq_flags, *sq_dropped, *sq_array;
	uint32_t *cq_head, *cq_tail, *cq_overflow;
	struct io_uring_cqe *cqes;
	struct io_uring_params params;
};

static long uring_setup(unsigned entries, struct io_uring_params *params)
{
	return syscall(__NR_io_uring_setup, entries, params);
}

static long uring_enter(int fd, unsigned submit, unsigned min, unsigned flags, const void *arg, size_t size)
{
	return syscall(__NR_io_uring_enter, fd, submit, min, flags, arg, size);
}

static long uring_register(int fd, unsigned opcode, void *arg, unsigned count)
{
	return syscall(__NR_io_uring_register, fd, opcode, arg, count);
}

static int map_ring(struct ring *ring, unsigned entries, unsigned flags, unsigned cq_entries)
{
	memset(ring, 0, sizeof *ring);
	ring->params.flags = flags;
	ring->params.cq_entries = cq_entries;
	ring->fd = (int)uring_setup(entries, &ring->params);
	if (ring->fd < 0) {
		dprintf(2, "io_uring: setup failed errno=%d\n", errno);
		return -1;
	}
	struct io_uring_params *p = &ring->params;
	ring->sq_n = p->sq_entries;
	ring->cq_n = p->cq_entries;
	ring->sq_mask = ring->sq_n - 1;
	ring->cq_mask = ring->cq_n - 1;
	size_t sq_end = p->sq_off.array + ring->sq_n * sizeof(uint32_t);
	size_t cq_end = p->cq_off.cqes + ring->cq_n * sizeof(struct io_uring_cqe);
	ring->rings_len = sq_end > cq_end ? sq_end : cq_end;
	ring->sqes_len = ring->sq_n * sizeof(struct io_uring_sqe);
	ring->rings = mmap(NULL, ring->rings_len, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_POPULATE,
	                   ring->fd, IORING_OFF_SQ_RING);
	if (ring->rings == MAP_FAILED) {
		dprintf(2, "io_uring: ring mmap failed errno=%d len=%zu\n", errno, ring->rings_len);
		return -2;
	}
	ring->sqes = mmap(NULL, ring->sqes_len, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_POPULATE,
	                  ring->fd, IORING_OFF_SQES);
	if (ring->sqes == MAP_FAILED) {
		dprintf(2, "io_uring: sqes mmap failed errno=%d len=%zu\n", errno, ring->sqes_len);
		return -3;
	}
	ring->sq_head = (uint32_t *)(ring->rings + p->sq_off.head);
	ring->sq_tail = (uint32_t *)(ring->rings + p->sq_off.tail);
	ring->sq_flags = (uint32_t *)(ring->rings + p->sq_off.flags);
	ring->sq_dropped = (uint32_t *)(ring->rings + p->sq_off.dropped);
	ring->sq_array = (uint32_t *)(ring->rings + p->sq_off.array);
	ring->cq_head = (uint32_t *)(ring->rings + p->cq_off.head);
	ring->cq_tail = (uint32_t *)(ring->rings + p->cq_off.tail);
	ring->cq_overflow = (uint32_t *)(ring->rings + p->cq_off.overflow);
	ring->cqes = (struct io_uring_cqe *)(ring->rings + p->cq_off.cqes);
	return 0;
}

static void unmap_ring(struct ring *ring)
{
	munmap(ring->rings, ring->rings_len);
	munmap(ring->sqes, ring->sqes_len);
	close(ring->fd);
}

static struct io_uring_sqe *next_sqe(struct ring *ring)
{
	unsigned slot = ring->tail & ring->sq_mask;
	struct io_uring_sqe *sqe = (struct io_uring_sqe *)ring->sqes + slot;
	memset(sqe, 0, sizeof *sqe);
	ring->sq_array[slot] = slot;
	ring->tail++;
	return sqe;
}

static void publish(struct ring *ring)
{
	__atomic_store_n(ring->sq_tail, ring->tail, __ATOMIC_RELEASE);
}

static int reap(struct ring *ring, struct io_uring_cqe *out)
{
	uint32_t head = *ring->cq_head;
	uint32_t tail = __atomic_load_n(ring->cq_tail, __ATOMIC_ACQUIRE);
	if (head == tail) return 0;
	*out = ring->cqes[head & ring->cq_mask];
	__atomic_store_n(ring->cq_head, head + 1, __ATOMIC_RELEASE);
	return 1;
}

static unsigned ready(struct ring *ring)
{
	return __atomic_load_n(ring->cq_tail, __ATOMIC_ACQUIRE) - *ring->cq_head;
}

static void on_alarm(int signal) { (void)signal; }

static long elapsed_ms(const struct timespec *from)
{
	struct timespec now;
	clock_gettime(CLOCK_MONOTONIC, &now);
	return (now.tv_sec - from->tv_sec) * 1000 + (now.tv_nsec - from->tv_nsec) / 1000000;
}

int verify_io_uring_core(void)
{
	struct io_uring_params bad;

	/* setup 参数校验：entries=0、未实现的 flags、非零保留字段、不可写的 params 指针。 */
	memset(&bad, 0, sizeof bad);
	CHECK(1, uring_setup(0, &bad) == -1 && errno == EINVAL);
	memset(&bad, 0, sizeof bad);
	bad.flags = IORING_SETUP_SQPOLL;
	CHECK(2, uring_setup(8, &bad) == -1 && errno == EINVAL);
	memset(&bad, 0, sizeof bad);
	bad.resv[1] = 1;
	CHECK(3, uring_setup(8, &bad) == -1 && errno == EINVAL);
	CHECK(4, uring_setup(8, (struct io_uring_params *)1) == -1 && errno == EFAULT);
	memset(&bad, 0, sizeof bad);
	CHECK(5, uring_setup(1u << 30, &bad) == -1 && errno == EINVAL);

	struct ring ring;
	CHECK(6, map_ring(&ring, 5, 0, 0) == 0);
	CHECK(7, ring.sq_n == 8 && ring.cq_n == 16);
	CHECK(8, (ring.params.features & (IORING_FEAT_SINGLE_MMAP | IORING_FEAT_NODROP |
	                                  IORING_FEAT_SUBMIT_STABLE | IORING_FEAT_EXT_ARG)) ==
	         (IORING_FEAT_SINGLE_MMAP | IORING_FEAT_NODROP | IORING_FEAT_SUBMIT_STABLE |
	          IORING_FEAT_EXT_ARG));
	CHECK(9, (fcntl(ring.fd, F_GETFD) & FD_CLOEXEC) != 0);

	/* mmap 约束：只接受三个魔数偏移、必须 MAP_SHARED。 */
	CHECK(10, mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE, ring.fd, IORING_OFF_SQ_RING) == MAP_FAILED &&
	          errno == EINVAL);
	CHECK(11, mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, ring.fd, 4096) == MAP_FAILED &&
	          errno == EINVAL);

	/* 单个 NOP：enter 返回提交数，CQE 带回 user_data，SQ head 被推进。 */
	struct io_uring_sqe *sqe = next_sqe(&ring);
	sqe->opcode = IORING_OP_NOP;
	sqe->user_data = 0x1111;
	publish(&ring);
	CHECK(12, uring_enter(ring.fd, 1, 1, IORING_ENTER_GETEVENTS, NULL, 0) == 1);
	struct io_uring_cqe cqe;
	CHECK(13, reap(&ring, &cqe) == 1 && cqe.user_data == 0x1111 && cqe.res == 0);
	CHECK(14, __atomic_load_n(ring.sq_head, __ATOMIC_ACQUIRE) == 1);

	/* NOP_INJECT_RESULT：len 作为 res 原样返回，负值是错误结果。 */
	sqe = next_sqe(&ring);
	sqe->opcode = IORING_OP_NOP;
	sqe->nop_flags = IORING_NOP_INJECT_RESULT;
	sqe->len = (uint32_t)-7;
	sqe->user_data = 2;
	sqe = next_sqe(&ring);
	sqe->opcode = IORING_OP_NOP;
	sqe->nop_flags = IORING_NOP_INJECT_RESULT;
	sqe->len = 5;
	sqe->user_data = 3;
	publish(&ring);
	CHECK(15, uring_enter(ring.fd, 2, 2, IORING_ENTER_GETEVENTS, NULL, 0) == 2);
	CHECK(16, reap(&ring, &cqe) == 1 && cqe.user_data == 2 && cqe.res == -7);
	CHECK(17, reap(&ring, &cqe) == 1 && cqe.user_data == 3 && cqe.res == 5);

	/* 溢出：24 个完成超过 16 项的 CQ，多出的保持顺序进入溢出链表并置 CQ_OVERFLOW。 */
	for (int round = 0; round < 3; round++) {
		for (int i = 0; i < 8; i++) {
			sqe = next_sqe(&ring);
			sqe->opcode = IORING_OP_NOP;
			sqe->user_data = 100 + round * 8 + i;
		}
		publish(&ring);
		CHECK(18, uring_enter(ring.fd, 8, 0, 0, NULL, 0) == 8);
	}
	CHECK(19, ready(&ring) == 16);
	CHECK(20, (__atomic_load_n(ring.sq_flags, __ATOMIC_ACQUIRE) & IORING_SQ_CQ_OVERFLOW) != 0);
	for (int i = 0; i < 16; i++)
		CHECK(21, reap(&ring, &cqe) == 1 && cqe.user_data == (uint64_t)(100 + i));
	CHECK(22, uring_enter(ring.fd, 0, 0, IORING_ENTER_GETEVENTS, NULL, 0) == 0);
	CHECK(23, ready(&ring) == 8 && (*ring.sq_flags & IORING_SQ_CQ_OVERFLOW) == 0);
	for (int i = 16; i < 24; i++)
		CHECK(24, reap(&ring, &cqe) == 1 && cqe.user_data == (uint64_t)(100 + i));

	/* init 失败：坏 opcode 产生 -EINVAL 的 CQE，并且不再提交后面的 SQE（Linux 语义）。 */
	sqe = next_sqe(&ring);
	sqe->opcode = 200;
	sqe->user_data = 7;
	sqe = next_sqe(&ring);
	sqe->opcode = IORING_OP_NOP;
	sqe->user_data = 8;
	publish(&ring);
	CHECK(25, uring_enter(ring.fd, 2, 0, 0, NULL, 0) == 1);
	CHECK(26, reap(&ring, &cqe) == 1 && cqe.user_data == 7 && cqe.res == -EINVAL);
	CHECK(27, reap(&ring, &cqe) == 0);
	CHECK(28, uring_enter(ring.fd, 1, 1, IORING_ENTER_GETEVENTS, NULL, 0) == 1);
	CHECK(29, reap(&ring, &cqe) == 1 && cqe.user_data == 8 && cqe.res == 0);

	/* 逐类 prep 错误：已知但未实现的 opcode / buffer-select 是 EOPNOTSUPP，未知位与非零 ioprio 是 EINVAL。 */
	static const struct { uint8_t opcode, flags; uint16_t ioprio; uint32_t nop_flags; int expected; } cases[] = {
		{ IORING_OP_READ_FIXED, 0, 0, 0, -EOPNOTSUPP },
		{ IORING_OP_NOP, IOSQE_BUFFER_SELECT, 0, 0, -EOPNOTSUPP },
		{ IORING_OP_NOP, 0x80, 0, 0, -EINVAL },
		{ IORING_OP_NOP, 0, 1, 0, -EINVAL },
		{ IORING_OP_NOP, 0, 0, 0x8000, -EINVAL },
		{ IORING_OP_NOP, 0, 0, IORING_NOP_CQE32, -EINVAL },
	};
	for (unsigned i = 0; i < sizeof cases / sizeof cases[0]; i++) {
		sqe = next_sqe(&ring);
		sqe->opcode = cases[i].opcode;
		sqe->flags = cases[i].flags;
		sqe->ioprio = cases[i].ioprio;
		sqe->nop_flags = cases[i].nop_flags;
		sqe->user_data = 300 + i;
		publish(&ring);
		CHECK(30, uring_enter(ring.fd, 1, 1, IORING_ENTER_GETEVENTS, NULL, 0) == 1);
		CHECK(31, reap(&ring, &cqe) == 1 && cqe.user_data == 300 + i && cqe.res == cases[i].expected);
	}

	/* SQ array 下标越界：条目被消费但不计入提交数，并累加 sq_dropped。 */
	ring.sq_array[ring.tail & ring.sq_mask] = 9999;
	ring.tail++;
	publish(&ring);
	CHECK(32, uring_enter(ring.fd, 1, 0, 0, NULL, 0) == 0);
	CHECK(33, *ring.sq_dropped == 1 && *ring.sq_head == ring.tail);

	/* NOP_FILE：有效 fd 成功，无效 fd 与 fixed file 是 -EBADF；fixed buffer 是 -EFAULT。 */
	static const struct { int fd; uint32_t nop_flags; int expected; } files[] = {
		{ 0, IORING_NOP_FILE, 0 },
		{ 987, IORING_NOP_FILE, -EBADF },
		{ -1, IORING_NOP_FILE, -EBADF },
		{ 0, IORING_NOP_FILE | IORING_NOP_FIXED_FILE, -EBADF },
		{ 0, IORING_NOP_FIXED_BUFFER, -EFAULT },
	};
	for (unsigned i = 0; i < sizeof files / sizeof files[0]; i++) {
		sqe = next_sqe(&ring);
		sqe->opcode = IORING_OP_NOP;
		sqe->fd = files[i].fd;
		sqe->nop_flags = files[i].nop_flags;
		sqe->user_data = 400 + i;
		publish(&ring);
		CHECK(34, uring_enter(ring.fd, 1, 1, IORING_ENTER_GETEVENTS, NULL, 0) == 1);
		CHECK(35, reap(&ring, &cqe) == 1 && cqe.res == files[i].expected);
	}

	/* CQE_SKIP_SUCCESS：成功不产生 CQE，失败仍然产生。 */
	sqe = next_sqe(&ring);
	sqe->opcode = IORING_OP_NOP;
	sqe->flags = IOSQE_CQE_SKIP_SUCCESS;
	sqe = next_sqe(&ring);
	sqe->opcode = IORING_OP_NOP;
	sqe->flags = IOSQE_CQE_SKIP_SUCCESS;
	sqe->nop_flags = IORING_NOP_INJECT_RESULT;
	sqe->len = (uint32_t)-5;
	sqe->user_data = 500;
	publish(&ring);
	CHECK(36, uring_enter(ring.fd, 2, 0, 0, NULL, 0) == 2);
	CHECK(37, reap(&ring, &cqe) == 1 && cqe.user_data == 500 && cqe.res == -5 && reap(&ring, &cqe) == 0);

	/* poll / epoll：空 CQ 只可写；完成到达后可读；epoll 边沿唤醒。 */
	struct pollfd pfd = { ring.fd, POLLIN | POLLOUT, 0 };
	CHECK(38, poll(&pfd, 1, 0) == 1 && pfd.revents == POLLOUT);
	int epfd = epoll_create1(0);
	struct epoll_event event = { .events = EPOLLIN };
	CHECK(39, epfd >= 0 && epoll_ctl(epfd, EPOLL_CTL_ADD, ring.fd, &event) == 0);
	CHECK(40, epoll_wait(epfd, &event, 1, 0) == 0);
	sqe = next_sqe(&ring);
	sqe->opcode = IORING_OP_NOP;
	sqe->user_data = 600;
	publish(&ring);
	CHECK(41, uring_enter(ring.fd, 1, 0, 0, NULL, 0) == 1);
	pfd.revents = 0;
	CHECK(42, poll(&pfd, 1, 0) == 1 && (pfd.revents & POLLIN) != 0);
	CHECK(43, epoll_wait(epfd, &event, 1, 0) == 1 && (event.events & EPOLLIN) != 0);
	CHECK(44, reap(&ring, &cqe) == 1 && cqe.user_data == 600);
	close(epfd);

	/* 等待：EXT_ARG 超时返回 -ETIME 且至少等了超时时长。 */
	struct __kernel_timespec timeout = { .tv_sec = 0, .tv_nsec = 60 * 1000 * 1000 };
	struct io_uring_getevents_arg arg = { .ts = (uintptr_t)&timeout };
	struct timespec started;
	clock_gettime(CLOCK_MONOTONIC, &started);
	CHECK(45, uring_enter(ring.fd, 0, 1, IORING_ENTER_GETEVENTS | IORING_ENTER_EXT_ARG, &arg, sizeof arg) == -1 &&
	          errno == ETIME);
	CHECK(46, elapsed_ms(&started) >= 55);
	arg.min_wait_usec = 1;
	CHECK(47, uring_enter(ring.fd, 0, 1, IORING_ENTER_GETEVENTS | IORING_ENTER_EXT_ARG, &arg, sizeof arg) == -1 &&
	          errno == EINVAL);
	arg.min_wait_usec = 0;
	CHECK(48, uring_enter(ring.fd, 0, 1, IORING_ENTER_GETEVENTS | IORING_ENTER_EXT_ARG, &arg, sizeof arg - 1) == -1 &&
	          errno == EINVAL);

	/* 唤醒：子进程共享 MAP_SHARED ring，延迟后提交 NOP；父进程阻塞在 enter 中被完成唤醒。 */
	timeout.tv_sec = 5;
	timeout.tv_nsec = 0;
	pid_t child = fork();
	CHECK(49, child >= 0);
	if (child == 0) {
		usleep(80 * 1000);
		struct io_uring_sqe *theirs = next_sqe(&ring);
		theirs->opcode = IORING_OP_NOP;
		theirs->user_data = 700;
		publish(&ring);
		_exit(uring_enter(ring.fd, 1, 0, 0, NULL, 0) == 1 ? 0 : 1);
	}
	clock_gettime(CLOCK_MONOTONIC, &started);
	CHECK(50, uring_enter(ring.fd, 0, 1, IORING_ENTER_GETEVENTS | IORING_ENTER_EXT_ARG, &arg, sizeof arg) == 0);
	CHECK(51, elapsed_ms(&started) < 2000 && reap(&ring, &cqe) == 1 && cqe.user_data == 700);
	int status = -1;
	CHECK(52, waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0);
	ring.tail = *ring.sq_tail; /* 子进程推进了共享 tail；本进程的本地副本必须同步。 */

	/* signal 中断：阻塞等待被 SIGALRM 打断返回 -EINTR。 */
	struct sigaction action = { .sa_handler = on_alarm };
	sigemptyset(&action.sa_mask);
	CHECK(53, sigaction(SIGALRM, &action, NULL) == 0);
	struct itimerval timer = { .it_value = { .tv_usec = 50 * 1000 } };
	CHECK(54, setitimer(ITIMER_REAL, &timer, NULL) == 0);
	CHECK(55, uring_enter(ring.fd, 0, 1, IORING_ENTER_GETEVENTS, NULL, 0) == -1 && errno == EINTR);

	/* enter 的 fd 与 flags 校验。 */
	int pipes[2];
	CHECK(56, pipe(pipes) == 0);
	CHECK(57, uring_enter(pipes[0], 0, 0, 0, NULL, 0) == -1 && errno == EOPNOTSUPP);
	CHECK(58, uring_enter(-1, 0, 0, 0, NULL, 0) == -1 && errno == EBADF);
	CHECK(59, uring_enter(ring.fd, 0, 0, 0x80000000u, NULL, 0) == -1 && errno == EINVAL);

	/* probe：只有 NOP 受支持；缓冲区必须整体清零；非 ring fd 与未知 opcode 的错误码。 */
	struct { struct io_uring_probe header; struct io_uring_probe_op ops[IORING_OP_LAST]; } probe;
	memset(&probe, 0, sizeof probe);
	CHECK(60, uring_register(ring.fd, IORING_REGISTER_PROBE, &probe, IORING_OP_LAST) == 0);
	CHECK(61, probe.header.last_op == IORING_OP_LAST - 1 && probe.header.ops_len == IORING_OP_LAST);
	CHECK(62, (probe.ops[IORING_OP_NOP].flags & IO_URING_OP_SUPPORTED) != 0 &&
	          (probe.ops[IORING_OP_READ].flags & IO_URING_OP_SUPPORTED) != 0 &&
	          (probe.ops[IORING_OP_READ_FIXED].flags & IO_URING_OP_SUPPORTED) == 0 &&
	          probe.ops[IORING_OP_READ_FIXED].op == IORING_OP_READ_FIXED);
	probe.header.resv = 1;
	CHECK(63, uring_register(ring.fd, IORING_REGISTER_PROBE, &probe, 4) == -1 && errno == EINVAL);
	CHECK(64, uring_register(pipes[0], IORING_REGISTER_PROBE, &probe, 4) == -1 && errno == EOPNOTSUPP);
	CHECK(65, uring_register(ring.fd, 9999, NULL, 0) == -1 && errno == EINVAL);
	close(pipes[0]);
	close(pipes[1]);

	/* CQSIZE 取整、CLAMP；关闭 fd 后已映射的 ring 页仍可读（映射持有 backing）。 */
	struct ring second;
	CHECK(66, map_ring(&second, 4, IORING_SETUP_CQSIZE, 40) == 0 && second.sq_n == 4 && second.cq_n == 64);
	unmap_ring(&second);
	memset(&bad, 0, sizeof bad);
	bad.flags = IORING_SETUP_CQSIZE;
	bad.cq_entries = 2;
	CHECK(67, uring_setup(8, &bad) == -1 && errno == EINVAL);
	memset(&bad, 0, sizeof bad);
	bad.flags = IORING_SETUP_CLAMP;
	long clamped = uring_setup(1u << 30, &bad);
	CHECK(68, clamped >= 0);
	close((int)clamped);
	int closing = ring.fd;
	ring.fd = -1;
	close(closing);
	CHECK(69, ring.cqes[0].user_data != 0xdeadbeef); /* 关闭后映射仍可读 */
	munmap(ring.rings, ring.rings_len);
	munmap(ring.sqes, ring.sqes_len);
	return 0;
}

/* ---- 读写类 opcode 与阻塞请求 ---------------------------------------------------------------- */

static void fill_read(struct io_uring_sqe *sqe, int fd, void *buffer, unsigned length, uint64_t user_data)
{
	sqe->opcode = IORING_OP_READ;
	sqe->fd = fd;
	sqe->addr = (uintptr_t)buffer;
	sqe->len = length;
	sqe->off = (uint64_t)-1;
	sqe->user_data = user_data;
}

static void fill_write(struct io_uring_sqe *sqe, int fd, const void *buffer, unsigned length, uint64_t user_data)
{
	sqe->opcode = IORING_OP_WRITE;
	sqe->fd = fd;
	sqe->addr = (uintptr_t)buffer;
	sqe->len = length;
	sqe->off = (uint64_t)-1;
	sqe->user_data = user_data;
}

/* 等待并取出一个 CQE（最多 3 秒）。 */
static int wait_cqe(struct ring *ring, struct io_uring_cqe *cqe)
{
	struct __kernel_timespec timeout = { .tv_sec = 3 };
	struct io_uring_getevents_arg arg = { .ts = (uintptr_t)&timeout };
	if (ready(ring) == 0 &&
	    uring_enter(ring->fd, 0, 1, IORING_ENTER_GETEVENTS | IORING_ENTER_EXT_ARG, &arg, sizeof arg) < 0)
		return 0;
	return reap(ring, cqe);
}

int verify_io_uring_io(void)
{
	struct ring ring;
	struct io_uring_cqe cqe;
	CHECK(1, map_ring(&ring, 8, 0, 0) == 0);
	CHECK(2, (ring.params.features & (IORING_FEAT_RW_CUR_POS | IORING_FEAT_FAST_POLL)) ==
	         (IORING_FEAT_RW_CUR_POS | IORING_FEAT_FAST_POLL));

	/* 普通文件：write 推进 position，read 用 off=-1 读回；显式偏移不动 position。 */
	int file = open("/tmp/uring-file", O_CREAT | O_RDWR | O_TRUNC, 0644);
	CHECK(3, file >= 0);
	struct io_uring_sqe *sqe = next_sqe(&ring);
	fill_write(sqe, file, "hello, uring", 12, 11);
	publish(&ring);
	CHECK(4, uring_enter(ring.fd, 1, 1, IORING_ENTER_GETEVENTS, NULL, 0) == 1);
	CHECK(5, reap(&ring, &cqe) == 1 && cqe.user_data == 11 && cqe.res == 12);
	CHECK(6, lseek(file, 0, SEEK_CUR) == 12);
	char buffer[64];
	sqe = next_sqe(&ring);
	fill_read(sqe, file, buffer, 5, 12);
	sqe->off = 7; /* 显式偏移：读 "uring" 且不改变 position */
	publish(&ring);
	CHECK(7, uring_enter(ring.fd, 1, 1, IORING_ENTER_GETEVENTS, NULL, 0) == 1);
	CHECK(8, reap(&ring, &cqe) == 1 && cqe.res == 5 && memcmp(buffer, "uring", 5) == 0);
	CHECK(9, lseek(file, 0, SEEK_CUR) == 12);

	/* READV / WRITEV：分散读取两段。 */
	char first[4], second[8];
	struct iovec vectors[2] = { { first, sizeof first }, { second, sizeof second } };
	sqe = next_sqe(&ring);
	sqe->opcode = IORING_OP_READV;
	sqe->fd = file;
	sqe->addr = (uintptr_t)vectors;
	sqe->len = 2;
	sqe->off = 0;
	sqe->user_data = 13;
	publish(&ring);
	CHECK(10, uring_enter(ring.fd, 1, 1, IORING_ENTER_GETEVENTS, NULL, 0) == 1);
	CHECK(11, reap(&ring, &cqe) == 1 && cqe.res == 12 && memcmp(first, "hell", 4) == 0 &&
	          memcmp(second, "o, uring", 8) == 0);

	/* 错误：坏 fd、只读 fd 上的写、坏 buffer 地址（prep 阶段 EFAULT）。 */
	sqe = next_sqe(&ring);
	fill_read(sqe, 977, buffer, 4, 14);
	publish(&ring);
	CHECK(12, uring_enter(ring.fd, 1, 1, IORING_ENTER_GETEVENTS, NULL, 0) == 1);
	CHECK(13, reap(&ring, &cqe) == 1 && cqe.res == -EBADF);
	int read_only = open("/tmp/uring-file", O_RDONLY);
	sqe = next_sqe(&ring);
	fill_write(sqe, read_only, "x", 1, 15);
	publish(&ring);
	CHECK(14, uring_enter(ring.fd, 1, 1, IORING_ENTER_GETEVENTS, NULL, 0) == 1);
	CHECK(15, reap(&ring, &cqe) == 1 && cqe.res == -EBADF);
	close(read_only);

	/* pipe：数据已就绪时同步完成。 */
	int pipes[2];
	CHECK(16, pipe(pipes) == 0);
	CHECK(17, write(pipes[1], "ready", 5) == 5);
	sqe = next_sqe(&ring);
	fill_read(sqe, pipes[0], buffer, sizeof buffer, 16);
	publish(&ring);
	CHECK(18, uring_enter(ring.fd, 1, 1, IORING_ENTER_GETEVENTS, NULL, 0) == 1);
	CHECK(19, reap(&ring, &cqe) == 1 && cqe.res == 5 && memcmp(buffer, "ready", 5) == 0);

	/* 阻塞读：空 pipe 上的读被挂起，enter 不阻塞；同一线程随后写入，请求被异步完成。
	 * 这是线程池方案会死锁的场景。 */
	sqe = next_sqe(&ring);
	fill_read(sqe, pipes[0], buffer, sizeof buffer, 17);
	publish(&ring);
	CHECK(20, uring_enter(ring.fd, 1, 0, 0, NULL, 0) == 1);
	CHECK(21, ready(&ring) == 0);
	CHECK(22, write(pipes[1], "later", 5) == 5);
	CHECK(23, wait_cqe(&ring, &cqe) == 1 && cqe.user_data == 17 && cqe.res == 5 &&
	          memcmp(buffer, "later", 5) == 0);

	/* 完成通知通过 epoll 可见：不必进入 io_uring_enter 就能看到 CQ 变为可读。 */
	int epfd = epoll_create1(0);
	struct epoll_event event = { .events = EPOLLIN };
	CHECK(24, epfd >= 0 && epoll_ctl(epfd, EPOLL_CTL_ADD, ring.fd, &event) == 0);
	sqe = next_sqe(&ring);
	fill_read(sqe, pipes[0], buffer, sizeof buffer, 18);
	publish(&ring);
	CHECK(25, uring_enter(ring.fd, 1, 0, 0, NULL, 0) == 1);
	CHECK(26, epoll_wait(epfd, &event, 1, 0) == 0);
	CHECK(27, write(pipes[1], "epoll", 5) == 5);
	CHECK(28, epoll_wait(epfd, &event, 1, 3000) == 1 && ready(&ring) == 1);
	CHECK(29, reap(&ring, &cqe) == 1 && cqe.user_data == 18 && cqe.res == 5);
	close(epfd);

	/* 多个阻塞读共用一个后续写：三个读都挂起，三次写依次唤醒它们。 */
	char slots[3][8];
	for (int i = 0; i < 3; i++) {
		sqe = next_sqe(&ring);
		fill_read(sqe, pipes[0], slots[i], 1, 30 + i);
	}
	publish(&ring);
	CHECK(30, uring_enter(ring.fd, 3, 0, 0, NULL, 0) == 3);
	CHECK(31, ready(&ring) == 0);
	CHECK(32, write(pipes[1], "abc", 3) == 3);
	int seen = 0;
	for (int i = 0; i < 3; i++) {
		CHECK(33, wait_cqe(&ring, &cqe) == 1 && cqe.res == 1);
		seen += 1 << (cqe.user_data - 30);
	}
	CHECK(34, seen == 7);

	/* 写端关闭：挂起的读得到 EOF（res=0）。 */
	sqe = next_sqe(&ring);
	fill_read(sqe, pipes[0], buffer, sizeof buffer, 40);
	publish(&ring);
	CHECK(35, uring_enter(ring.fd, 1, 0, 0, NULL, 0) == 1);
	close(pipes[1]);
	CHECK(36, wait_cqe(&ring, &cqe) == 1 && cqe.user_data == 40 && cqe.res == 0);
	close(pipes[0]);

	/* O_NONBLOCK 的 pipe：不挂起，直接返回 -EAGAIN。 */
	CHECK(37, pipe2(pipes, O_NONBLOCK) == 0);
	sqe = next_sqe(&ring);
	fill_read(sqe, pipes[0], buffer, sizeof buffer, 41);
	publish(&ring);
	CHECK(38, uring_enter(ring.fd, 1, 1, IORING_ENTER_GETEVENTS, NULL, 0) == 1);
	CHECK(39, reap(&ring, &cqe) == 1 && cqe.res == -EAGAIN);
	close(pipes[0]);
	close(pipes[1]);

	/* socketpair 与 eventfd 同样经挂起路径完成。 */
	int pair[2];
	CHECK(40, socketpair(AF_UNIX, SOCK_STREAM, 0, pair) == 0);
	sqe = next_sqe(&ring);
	fill_read(sqe, pair[0], buffer, sizeof buffer, 42);
	publish(&ring);
	CHECK(41, uring_enter(ring.fd, 1, 0, 0, NULL, 0) == 1);
	CHECK(42, write(pair[1], "sock", 4) == 4);
	CHECK(43, wait_cqe(&ring, &cqe) == 1 && cqe.user_data == 42 && cqe.res == 4);
	close(pair[0]);
	close(pair[1]);
	int event_fd = eventfd(0, 0);
	uint64_t counter = 0;
	sqe = next_sqe(&ring);
	fill_read(sqe, event_fd, &counter, sizeof counter, 43);
	publish(&ring);
	CHECK(44, uring_enter(ring.fd, 1, 0, 0, NULL, 0) == 1);
	uint64_t one = 5;
	CHECK(45, write(event_fd, &one, sizeof one) == sizeof one);
	CHECK(46, wait_cqe(&ring, &cqe) == 1 && cqe.user_data == 43 && cqe.res == 8 && counter == 5);
	close(event_fd);

	/* 阻塞写：写满 pipe 后再提交写，被挂起；读走数据后完成。 */
	CHECK(47, pipe(pipes) == 0);
	int flags = fcntl(pipes[1], F_GETFL);
	CHECK(48, fcntl(pipes[1], F_SETFL, flags | O_NONBLOCK) == 0);
	char filler[4096];
	memset(filler, 'f', sizeof filler);
	while (write(pipes[1], filler, sizeof filler) > 0) {}
	CHECK(49, errno == EAGAIN && fcntl(pipes[1], F_SETFL, flags) == 0);
	sqe = next_sqe(&ring);
	fill_write(sqe, pipes[1], "tail", 4, 44);
	publish(&ring);
	CHECK(50, uring_enter(ring.fd, 1, 0, 0, NULL, 0) == 1);
	CHECK(51, ready(&ring) == 0);
	CHECK(52, read(pipes[0], filler, sizeof filler) > 0);
	CHECK(53, wait_cqe(&ring, &cqe) == 1 && cqe.user_data == 44 && cqe.res == 4);
	close(pipes[0]);
	close(pipes[1]);

	/* FSYNC 与 CLOSE。 */
	sqe = next_sqe(&ring);
	sqe->opcode = IORING_OP_FSYNC;
	sqe->fd = file;
	sqe->user_data = 45;
	publish(&ring);
	CHECK(54, uring_enter(ring.fd, 1, 1, IORING_ENTER_GETEVENTS, NULL, 0) == 1);
	CHECK(55, reap(&ring, &cqe) == 1 && cqe.res == 0);
	sqe = next_sqe(&ring);
	sqe->opcode = IORING_OP_CLOSE;
	sqe->fd = file;
	sqe->user_data = 46;
	publish(&ring);
	CHECK(56, uring_enter(ring.fd, 1, 1, IORING_ENTER_GETEVENTS, NULL, 0) == 1);
	CHECK(57, reap(&ring, &cqe) == 1 && cqe.res == 0 && fcntl(file, F_GETFD) == -1 && errno == EBADF);
	sqe = next_sqe(&ring);
	sqe->opcode = IORING_OP_CLOSE;
	sqe->fd = file;
	sqe->user_data = 47;
	publish(&ring);
	CHECK(58, uring_enter(ring.fd, 1, 1, IORING_ENTER_GETEVENTS, NULL, 0) == 1);
	CHECK(59, reap(&ring, &cqe) == 1 && cqe.res == -EBADF);

	/* 关闭 ring 取消挂起的读：请求持有的 pipe 读端被释放，写者得到 EPIPE。 */
	CHECK(60, pipe(pipes) == 0);
	sqe = next_sqe(&ring);
	fill_read(sqe, pipes[0], buffer, sizeof buffer, 48);
	publish(&ring);
	CHECK(61, uring_enter(ring.fd, 1, 0, 0, NULL, 0) == 1);
	close(pipes[0]); /* 用户态的读端已关；挂起的请求仍持有最后一个引用 */
	signal(SIGPIPE, SIG_IGN);
	CHECK(62, write(pipes[1], "x", 1) == 1); /* 读端仍被挂起请求持有，写入成功 */
	unmap_ring(&ring); /* 关闭 ring fd：取消请求 */
	int closed = 0;
	for (int attempt = 0; attempt < 200 && !closed; attempt++) {
		if (write(pipes[1], "x", 1) < 0 && errno == EPIPE) closed = 1;
		else usleep(10 * 1000);
	}
	CHECK(63, closed);
	close(pipes[1]);

	/* 提交者退出：子进程提交阻塞读后退出，其挂起请求随 fd 表关闭被取消，父进程的写者得到 EPIPE。 */
	CHECK(64, pipe(pipes) == 0);
	pid_t child = fork();
	CHECK(65, child >= 0);
	if (child == 0) {
		struct ring theirs;
		char scratch[16];
		if (map_ring(&theirs, 4, 0, 0) != 0) _exit(2);
		struct io_uring_sqe *queued = next_sqe(&theirs);
		fill_read(queued, pipes[0], scratch, sizeof scratch, 1);
		publish(&theirs);
		_exit(uring_enter(theirs.fd, 1, 0, 0, NULL, 0) == 1 ? 0 : 1);
	}
	int status = -1;
	CHECK(66, waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0);
	close(pipes[0]);
	closed = 0;
	for (int attempt = 0; attempt < 200 && !closed; attempt++) {
		if (write(pipes[1], "x", 1) < 0 && errno == EPIPE) closed = 1;
		else usleep(10 * 1000);
	}
	CHECK(67, closed);
	close(pipes[1]);
	unlink("/tmp/uring-file");
	return 0;
}


/* ---- poll / timeout / cancel ------------------------------------------------------------------ */

static void fill_poll(struct io_uring_sqe *sqe, int fd, unsigned events, uint64_t user_data)
{
	sqe->opcode = IORING_OP_POLL_ADD;
	sqe->fd = fd;
	sqe->poll32_events = events;
	sqe->user_data = user_data;
}

static void fill_timeout(struct io_uring_sqe *sqe, const struct __kernel_timespec *time, unsigned count,
                         unsigned flags, uint64_t user_data)
{
	sqe->opcode = IORING_OP_TIMEOUT;
	sqe->fd = -1;
	sqe->addr = (uintptr_t)time;
	sqe->len = 1;
	sqe->off = count;
	sqe->timeout_flags = flags;
	sqe->user_data = user_data;
}

static void fill_cancel(struct io_uring_sqe *sqe, uint8_t opcode, uint64_t target, unsigned flags,
                        uint64_t user_data)
{
	sqe->opcode = opcode;
	sqe->fd = -1;
	sqe->addr = target;
	sqe->cancel_flags = flags;
	sqe->user_data = user_data;
}

static int submit_one(struct ring *ring)
{
	publish(ring);
	return (int)uring_enter(ring->fd, 1, 0, 0, NULL, 0);
}

/* 取消是异步的：取消请求的 CQE 与目标的 -ECANCELED 谁先到不确定（Linux 同），按 user_data 配对。 */
static int expect_pair(struct ring *ring, uint64_t first, int first_res, uint64_t second, int second_res)
{
	struct io_uring_cqe cqe;
	int seen_first = 0, seen_second = 0;
	for (int i = 0; i < 2; i++) {
		if (wait_cqe(ring, &cqe) != 1) return 0;
		if (cqe.user_data == first && cqe.res == first_res) seen_first++;
		else if (cqe.user_data == second && cqe.res == second_res) seen_second++;
	}
	return seen_first == 1 && seen_second == 1;
}

int verify_io_uring_events(void)
{
	struct ring ring;
	struct io_uring_cqe cqe;
	struct io_uring_sqe *sqe;
	struct timespec started;
	CHECK(1, map_ring(&ring, 16, 0, 0) == 0);
	int pipes[2];
	CHECK(2, pipe(pipes) == 0);

	/* POLL_ADD：读端空，不就绪；写入后异步完成，res 是 revents。 */
	sqe = next_sqe(&ring);
	fill_poll(sqe, pipes[0], POLLIN, 1);
	CHECK(3, submit_one(&ring) == 1 && ready(&ring) == 0);
	CHECK(4, write(pipes[1], "p", 1) == 1);
	CHECK(5, wait_cqe(&ring, &cqe) == 1 && cqe.user_data == 1 && (cqe.res & POLLIN) != 0);

	/* 已就绪时同步完成：读端有数据、写端可写。 */
	sqe = next_sqe(&ring);
	fill_poll(sqe, pipes[0], POLLIN, 2);
	sqe = next_sqe(&ring);
	fill_poll(sqe, pipes[1], POLLOUT, 3);
	publish(&ring);
	CHECK(6, uring_enter(ring.fd, 2, 2, IORING_ENTER_GETEVENTS, NULL, 0) == 2);
	CHECK(7, reap(&ring, &cqe) == 1 && cqe.user_data == 2 && (cqe.res & POLLIN) != 0);
	CHECK(8, reap(&ring, &cqe) == 1 && cqe.user_data == 3 && (cqe.res & POLLOUT) != 0);
	char drain[8];
	CHECK(9, read(pipes[0], drain, sizeof drain) == 1);

	/* 对端关闭：HUP 总是被报告，即使没有请求它。 */
	sqe = next_sqe(&ring);
	fill_poll(sqe, pipes[0], POLLIN, 4);
	CHECK(10, submit_one(&ring) == 1 && ready(&ring) == 0);
	close(pipes[1]);
	CHECK(11, wait_cqe(&ring, &cqe) == 1 && cqe.user_data == 4 && (cqe.res & POLLHUP) != 0);
	close(pipes[0]);

	/* POLL_ADD 的 prep：坏 fd 是 CQE 错误；multishot 未实现；非零 addr 是 EINVAL。 */
	sqe = next_sqe(&ring);
	fill_poll(sqe, 999, POLLIN, 5);
	CHECK(12, submit_one(&ring) == 1 && wait_cqe(&ring, &cqe) == 1 && cqe.res == -EBADF);
	sqe = next_sqe(&ring);
	fill_poll(sqe, 0, POLLIN, 6);
	sqe->len = IORING_POLL_ADD_MULTI;
	CHECK(13, submit_one(&ring) == 1 && reap(&ring, &cqe) == 1 && cqe.res == -EOPNOTSUPP);
	sqe = next_sqe(&ring);
	fill_poll(sqe, 0, POLLIN, 7);
	sqe->addr = 1;
	CHECK(14, submit_one(&ring) == 1 && reap(&ring, &cqe) == 1 && cqe.res == -EINVAL);

	/* TIMEOUT：纯超时以 -ETIME 完成，且至少等了设定时间。 */
	struct __kernel_timespec fifty_ms = { .tv_sec = 0, .tv_nsec = 50 * 1000 * 1000 };
	clock_gettime(CLOCK_MONOTONIC, &started);
	sqe = next_sqe(&ring);
	fill_timeout(sqe, &fifty_ms, 0, 0, 10);
	CHECK(15, submit_one(&ring) == 1 && ready(&ring) == 0);
	CHECK(16, wait_cqe(&ring, &cqe) == 1 && cqe.user_data == 10 && cqe.res == -ETIME);
	CHECK(17, elapsed_ms(&started) >= 45);

	/* ETIME_SUCCESS：到期的 CQE 仍是 -ETIME（Linux `io_timeout_fn`），只是不使链断开。 */
	sqe = next_sqe(&ring);
	fill_timeout(sqe, &fifty_ms, 0, IORING_TIMEOUT_ETIME_SUCCESS, 11);
	CHECK(18, submit_one(&ring) == 1 && wait_cqe(&ring, &cqe) == 1 && cqe.res == -ETIME);

	/* 绝对时间（CLOCK_MONOTONIC）。 */
	struct timespec now;
	clock_gettime(CLOCK_MONOTONIC, &now);
	struct __kernel_timespec absolute = { .tv_sec = now.tv_sec, .tv_nsec = now.tv_nsec + 50 * 1000 * 1000 };
	if (absolute.tv_nsec >= 1000000000) { absolute.tv_sec++; absolute.tv_nsec -= 1000000000; }
	clock_gettime(CLOCK_MONOTONIC, &started);
	sqe = next_sqe(&ring);
	fill_timeout(sqe, &absolute, 0, IORING_TIMEOUT_ABS, 12);
	CHECK(19, submit_one(&ring) == 1 && wait_cqe(&ring, &cqe) == 1 && cqe.res == -ETIME);
	CHECK(20, elapsed_ms(&started) >= 45);

	/* 带计数的 timeout：等 2 个其他 CQE，而不是 5 秒；timeout 自己的完成不计入别的 timeout。 */
	struct __kernel_timespec five_s = { .tv_sec = 5 };
	sqe = next_sqe(&ring);
	fill_timeout(sqe, &five_s, 2, 0, 20);
	CHECK(21, submit_one(&ring) == 1 && ready(&ring) == 0);
	clock_gettime(CLOCK_MONOTONIC, &started);
	for (int i = 0; i < 2; i++) {
		sqe = next_sqe(&ring);
		sqe->opcode = IORING_OP_NOP;
		sqe->user_data = 21 + i;
	}
	publish(&ring);
	CHECK(22, uring_enter(ring.fd, 2, 0, 0, NULL, 0) == 2);
	int saw_timeout = 0;
	for (int i = 0; i < 3; i++) {
		CHECK(23, wait_cqe(&ring, &cqe) == 1);
		if (cqe.user_data == 20) {
			CHECK(24, cqe.res == 0);
			saw_timeout = 1;
		}
	}
	CHECK(25, saw_timeout && elapsed_ms(&started) < 2000);

	/* TIMEOUT 的 prep 错误。 */
	sqe = next_sqe(&ring);
	fill_timeout(sqe, &fifty_ms, 0, 0, 30);
	sqe->len = 0;
	CHECK(26, submit_one(&ring) == 1 && reap(&ring, &cqe) == 1 && cqe.res == -EINVAL);
	sqe = next_sqe(&ring);
	fill_timeout(sqe, (const struct __kernel_timespec *)1, 0, 0, 31);
	CHECK(27, submit_one(&ring) == 1 && reap(&ring, &cqe) == 1 && cqe.res == -EFAULT);
	sqe = next_sqe(&ring);
	fill_timeout(sqe, &fifty_ms, 0, IORING_TIMEOUT_REALTIME, 32);
	CHECK(28, submit_one(&ring) == 1 && reap(&ring, &cqe) == 1 && cqe.res == -EOPNOTSUPP);
	sqe = next_sqe(&ring);
	fill_timeout(sqe, &fifty_ms, 0, IORING_TIMEOUT_ABS | IORING_TIMEOUT_MULTISHOT, 33);
	CHECK(29, submit_one(&ring) == 1 && reap(&ring, &cqe) == 1 && cqe.res == -EINVAL);
	struct __kernel_timespec negative = { .tv_sec = -1 };
	sqe = next_sqe(&ring);
	fill_timeout(sqe, &negative, 0, 0, 34);
	CHECK(30, submit_one(&ring) == 1 && reap(&ring, &cqe) == 1 && cqe.res == -EINVAL);

	/* TIMEOUT_REMOVE：目标以 -ECANCELED 完成，移除请求得到 0；不存在的目标是 -ENOENT。 */
	sqe = next_sqe(&ring);
	fill_timeout(sqe, &five_s, 0, 0, 40);
	CHECK(31, submit_one(&ring) == 1);
	sqe = next_sqe(&ring);
	fill_cancel(sqe, IORING_OP_TIMEOUT_REMOVE, 40, 0, 41);
	CHECK(32, submit_one(&ring) == 1);
	CHECK(33, expect_pair(&ring, 40, -ECANCELED, 41, 0));
	sqe = next_sqe(&ring);
	fill_cancel(sqe, IORING_OP_TIMEOUT_REMOVE, 40, 0, 42);
	CHECK(35, submit_one(&ring) == 1 && wait_cqe(&ring, &cqe) == 1 && cqe.res == -ENOENT);

	/* POLL_REMOVE 只能取消 poll：对 timeout 的 user_data 是 -ENOENT。 */
	CHECK(36, pipe(pipes) == 0);
	sqe = next_sqe(&ring);
	fill_poll(sqe, pipes[0], POLLIN, 50);
	CHECK(37, submit_one(&ring) == 1);
	sqe = next_sqe(&ring);
	fill_cancel(sqe, IORING_OP_POLL_REMOVE, 50, 0, 51);
	CHECK(38, submit_one(&ring) == 1);
	CHECK(39, expect_pair(&ring, 50, -ECANCELED, 51, 0));

	/* ASYNC_CANCEL 取消阻塞读：读 CQE 是 -ECANCELED，并且没有消费随后写入的数据。 */
	char buffer[16];
	sqe = next_sqe(&ring);
	fill_read(sqe, pipes[0], buffer, sizeof buffer, 60);
	CHECK(41, submit_one(&ring) == 1 && ready(&ring) == 0);
	sqe = next_sqe(&ring);
	fill_cancel(sqe, IORING_OP_ASYNC_CANCEL, 60, 0, 61);
	CHECK(42, submit_one(&ring) == 1);
	CHECK(43, expect_pair(&ring, 60, -ECANCELED, 61, 0));
	CHECK(45, write(pipes[1], "z", 1) == 1 && read(pipes[0], buffer, sizeof buffer) == 1 && buffer[0] == 'z');

	/* 取消不存在或已完成的请求：-ENOENT。 */
	sqe = next_sqe(&ring);
	fill_cancel(sqe, IORING_OP_ASYNC_CANCEL, 9999, 0, 62);
	CHECK(46, submit_one(&ring) == 1 && wait_cqe(&ring, &cqe) == 1 && cqe.res == -ENOENT);

	/* CANCEL_ALL：同一 user_data 的两个挂起读都被取消，结果是个数。 */
	for (int i = 0; i < 2; i++) {
		sqe = next_sqe(&ring);
		fill_read(sqe, pipes[0], buffer, sizeof buffer, 77);
	}
	publish(&ring);
	CHECK(47, uring_enter(ring.fd, 2, 0, 0, NULL, 0) == 2);
	sqe = next_sqe(&ring);
	fill_cancel(sqe, IORING_OP_ASYNC_CANCEL, 77, IORING_ASYNC_CANCEL_ALL, 63);
	CHECK(48, submit_one(&ring) == 1);
	int canceled = 0, count = -1;
	for (int i = 0; i < 3; i++) {
		CHECK(49, wait_cqe(&ring, &cqe) == 1);
		if (cqe.user_data == 77 && cqe.res == -ECANCELED) canceled++;
		else if (cqe.user_data == 63) count = cqe.res;
	}
	CHECK(50, canceled == 2 && count == 2);

	/* CANCEL_ANY | CANCEL_ALL：取消该 ring 的所有挂起请求（读与 poll 与 timeout）。 */
	sqe = next_sqe(&ring);
	fill_read(sqe, pipes[0], buffer, sizeof buffer, 80);
	sqe = next_sqe(&ring);
	fill_poll(sqe, pipes[0], POLLIN, 81);
	sqe = next_sqe(&ring);
	fill_timeout(sqe, &five_s, 0, 0, 82);
	publish(&ring);
	CHECK(51, uring_enter(ring.fd, 3, 0, 0, NULL, 0) == 3);
	sqe = next_sqe(&ring);
	fill_cancel(sqe, IORING_OP_ASYNC_CANCEL, 0, IORING_ASYNC_CANCEL_ANY | IORING_ASYNC_CANCEL_ALL, 64);
	CHECK(52, submit_one(&ring) == 1);
	canceled = 0;
	count = -1;
	for (int i = 0; i < 4; i++) {
		CHECK(53, wait_cqe(&ring, &cqe) == 1);
		if (cqe.user_data == 64) count = cqe.res;
		else if (cqe.res == -ECANCELED) canceled++;
	}
	CHECK(54, canceled == 3 && count == 3);

	/* ASYNC_CANCEL 的 prep：按 fd 匹配时 fd 在 issue 时解析，无效 fd 是 -EBADF；未知 flags 是 -EINVAL。 */
	sqe = next_sqe(&ring);
	fill_cancel(sqe, IORING_OP_ASYNC_CANCEL, 0, IORING_ASYNC_CANCEL_FD, 65);
	CHECK(55, submit_one(&ring) == 1 && reap(&ring, &cqe) == 1 && cqe.res == -EBADF);
	sqe = next_sqe(&ring);
	fill_cancel(sqe, IORING_OP_ASYNC_CANCEL, 0, 0x80, 66);
	CHECK(56, submit_one(&ring) == 1 && reap(&ring, &cqe) == 1 && cqe.res == -EINVAL);
	close(pipes[0]);
	close(pipes[1]);
	unmap_ring(&ring);
	return 0;
}

/* ---- 链接（IO_LINK / IO_HARDLINK）与 IO_DRAIN ------------------------------------------------- */

static void fill_nop(struct io_uring_sqe *sqe, uint8_t flags, int inject, uint64_t user_data)
{
	sqe->opcode = IORING_OP_NOP;
	sqe->flags = flags;
	if (inject != 0) {
		sqe->nop_flags = IORING_NOP_INJECT_RESULT;
		sqe->len = (uint32_t)inject;
	}
	sqe->user_data = user_data;
}

/* 按序取出 n 个 CQE 并比对 (user_data, res)；`exp` 为 NULL 项表示只检查 user_data 与 res 的顺序对。 */
struct expect { uint64_t user_data; int res; };

static int expect_cqes(struct ring *ring, const struct expect *want, unsigned count)
{
	struct io_uring_cqe cqe;
	for (unsigned i = 0; i < count; i++) {
		if (wait_cqe(ring, &cqe) != 1 || cqe.user_data != want[i].user_data || cqe.res != want[i].res) return 0;
	}
	return 1;
}

int verify_io_uring_chains(void)
{
	struct ring ring;
	struct io_uring_cqe cqe;
	struct io_uring_sqe *sqe;
	int pipes[2];
	char buffer[16];
	CHECK(1, map_ring(&ring, 16, 0, 0) == 0);

	/* 成功的链按序执行。 */
	fill_nop(next_sqe(&ring), IOSQE_IO_LINK, 0, 1);
	fill_nop(next_sqe(&ring), IOSQE_IO_LINK, 0, 2);
	fill_nop(next_sqe(&ring), 0, 0, 3);
	publish(&ring);
	CHECK(2, uring_enter(ring.fd, 3, 3, IORING_ENTER_GETEVENTS, NULL, 0) == 3);
	CHECK(3, expect_cqes(&ring, (const struct expect[]){ {1, 0}, {2, 0}, {3, 0} }, 3));

	/* 中间成员失败：其后的非 hardlink 成员以 -ECANCELED 完成。 */
	fill_nop(next_sqe(&ring), IOSQE_IO_LINK, 0, 10);
	fill_nop(next_sqe(&ring), IOSQE_IO_LINK, -5, 11);
	fill_nop(next_sqe(&ring), IOSQE_IO_LINK, 0, 12);
	fill_nop(next_sqe(&ring), 0, 0, 13);
	publish(&ring);
	CHECK(4, uring_enter(ring.fd, 4, 4, IORING_ENTER_GETEVENTS, NULL, 0) == 4);
	CHECK(5, expect_cqes(&ring, (const struct expect[]){ {10, 0}, {11, -5}, {12, -ECANCELED}, {13, -ECANCELED} }, 4));

	/* HARDLINK：失败不断开链。 */
	fill_nop(next_sqe(&ring), IOSQE_IO_HARDLINK, -5, 20);
	fill_nop(next_sqe(&ring), IOSQE_IO_HARDLINK, 0, 21);
	fill_nop(next_sqe(&ring), 0, 0, 22);
	publish(&ring);
	CHECK(6, uring_enter(ring.fd, 3, 3, IORING_ENTER_GETEVENTS, NULL, 0) == 3);
	CHECK(7, expect_cqes(&ring, (const struct expect[]){ {20, -5}, {21, 0}, {22, 0} }, 3));

	/* 批末未结束的链按现状结束，仍会执行。 */
	fill_nop(next_sqe(&ring), IOSQE_IO_LINK, 0, 30);
	fill_nop(next_sqe(&ring), IOSQE_IO_LINK, 0, 31);
	publish(&ring);
	CHECK(8, uring_enter(ring.fd, 2, 2, IORING_ENTER_GETEVENTS, NULL, 0) == 2);
	CHECK(9, expect_cqes(&ring, (const struct expect[]){ {30, 0}, {31, 0} }, 2));

	/* prep 失败发生在链中间：整条链不执行，失败成员保留自己的错误，其余 -ECANCELED；提交继续消费。 */
	fill_nop(next_sqe(&ring), IOSQE_IO_LINK, 0, 40);
	sqe = next_sqe(&ring);
	fill_nop(sqe, IOSQE_IO_LINK, 0, 41);
	sqe->ioprio = 1;
	fill_nop(next_sqe(&ring), 0, 0, 42);
	fill_nop(next_sqe(&ring), 0, 0, 43);
	publish(&ring);
	CHECK(10, uring_enter(ring.fd, 4, 4, IORING_ENTER_GETEVENTS, NULL, 0) == 4);
	CHECK(11, expect_cqes(&ring, (const struct expect[]){ {40, -ECANCELED}, {41, -EINVAL}, {42, -ECANCELED}, {43, 0} }, 4));

	/* 结束链的成员 prep 失败：本次提交在此停止，其后的 SQE 留给下一次。 */
	fill_nop(next_sqe(&ring), IOSQE_IO_LINK, 0, 50);
	sqe = next_sqe(&ring);
	fill_nop(sqe, 0, 0, 51);
	sqe->ioprio = 1;
	fill_nop(next_sqe(&ring), 0, 0, 52);
	publish(&ring);
	CHECK(12, uring_enter(ring.fd, 3, 0, 0, NULL, 0) == 2);
	CHECK(13, expect_cqes(&ring, (const struct expect[]){ {50, -ECANCELED}, {51, -EINVAL} }, 2));
	CHECK(14, uring_enter(ring.fd, 1, 1, IORING_ENTER_GETEVENTS, NULL, 0) == 1);
	CHECK(15, expect_cqes(&ring, (const struct expect[]){ {52, 0} }, 1));

	/* 链头是阻塞读（读满请求长度才算成功）：后继等它被写唤醒完成后才执行。 */
	CHECK(16, pipe(pipes) == 0);
	sqe = next_sqe(&ring);
	fill_read(sqe, pipes[0], buffer, 1, 60);
	sqe->flags = IOSQE_IO_LINK;
	fill_nop(next_sqe(&ring), 0, 0, 61);
	publish(&ring);
	CHECK(17, uring_enter(ring.fd, 2, 0, 0, NULL, 0) == 2 && ready(&ring) == 0);
	CHECK(18, write(pipes[1], "x", 1) == 1);
	CHECK(19, expect_cqes(&ring, (const struct expect[]){ {60, 1}, {61, 0} }, 2));

	/* 短读使链断开（Linux 以 res != 请求长度判定失败）。 */
	CHECK(20, write(pipes[1], "y", 1) == 1);
	sqe = next_sqe(&ring);
	fill_read(sqe, pipes[0], buffer, 4, 70);
	sqe->flags = IOSQE_IO_LINK;
	fill_nop(next_sqe(&ring), 0, 0, 71);
	publish(&ring);
	CHECK(21, uring_enter(ring.fd, 2, 2, IORING_ENTER_GETEVENTS, NULL, 0) == 2);
	CHECK(22, expect_cqes(&ring, (const struct expect[]){ {70, 1}, {71, -ECANCELED} }, 2));

	/* 链头是 TIMEOUT：到期的 -ETIME 使链断开；ETIME_SUCCESS 则不断开（CQE 仍是 -ETIME）。 */
	struct __kernel_timespec twenty_ms = { .tv_sec = 0, .tv_nsec = 20 * 1000 * 1000 };
	sqe = next_sqe(&ring);
	fill_timeout(sqe, &twenty_ms, 0, 0, 80);
	sqe->flags = IOSQE_IO_LINK;
	fill_nop(next_sqe(&ring), 0, 0, 81);
	publish(&ring);
	CHECK(23, uring_enter(ring.fd, 2, 0, 0, NULL, 0) == 2);
	CHECK(24, expect_cqes(&ring, (const struct expect[]){ {80, -ETIME}, {81, -ECANCELED} }, 2));
	sqe = next_sqe(&ring);
	fill_timeout(sqe, &twenty_ms, 0, IORING_TIMEOUT_ETIME_SUCCESS, 82);
	sqe->flags = IOSQE_IO_LINK;
	fill_nop(next_sqe(&ring), 0, 0, 83);
	publish(&ring);
	CHECK(25, uring_enter(ring.fd, 2, 0, 0, NULL, 0) == 2);
	CHECK(26, expect_cqes(&ring, (const struct expect[]){ {82, -ETIME}, {83, 0} }, 2));

	/* 取消正在等待的链头：它得到 -ECANCELED，后继随之取消。 */
	sqe = next_sqe(&ring);
	fill_read(sqe, pipes[0], buffer, sizeof buffer, 90);
	sqe->flags = IOSQE_IO_LINK;
	fill_nop(next_sqe(&ring), 0, 0, 91);
	publish(&ring);
	CHECK(27, uring_enter(ring.fd, 2, 0, 0, NULL, 0) == 2 && ready(&ring) == 0);
	fill_cancel(next_sqe(&ring), IORING_OP_ASYNC_CANCEL, 90, 0, 92);
	CHECK(28, submit_one(&ring) == 1);
	int saw_cancel = 0, saw_head = 0, saw_tail = 0;
	for (int i = 0; i < 3; i++) {
		CHECK(29, wait_cqe(&ring, &cqe) == 1);
		if (cqe.user_data == 92 && cqe.res == 0) saw_cancel = 1;
		else if (cqe.user_data == 90 && cqe.res == -ECANCELED) saw_head = 1;
		else if (cqe.user_data == 91 && cqe.res == -ECANCELED) saw_tail = saw_head;
	}
	CHECK(30, saw_cancel && saw_head && saw_tail);

	/* fd 在 issue 时解析：链里先 close 再读，后者看到的是新的 fd 表（-EBADF）。 */
	int duplicate = dup(pipes[0]);
	CHECK(31, duplicate >= 0);
	sqe = next_sqe(&ring);
	sqe->opcode = IORING_OP_CLOSE;
	sqe->fd = duplicate;
	sqe->flags = IOSQE_IO_LINK;
	sqe->user_data = 100;
	fill_read(next_sqe(&ring), duplicate, buffer, sizeof buffer, 101);
	publish(&ring);
	CHECK(32, uring_enter(ring.fd, 2, 2, IORING_ENTER_GETEVENTS, NULL, 0) == 2);
	CHECK(33, expect_cqes(&ring, (const struct expect[]){ {100, 0}, {101, -EBADF} }, 2));

	/* DRAIN：等所有更早的请求完成才开始，其后的请求等它完成。 */
	fill_read(next_sqe(&ring), pipes[0], buffer, sizeof buffer, 120);
	fill_nop(next_sqe(&ring), IOSQE_IO_DRAIN, 0, 121);
	fill_nop(next_sqe(&ring), 0, 0, 122);
	publish(&ring);
	CHECK(37, uring_enter(ring.fd, 3, 0, 0, NULL, 0) == 3);
	CHECK(38, ready(&ring) == 0);
	CHECK(39, write(pipes[1], "d", 1) == 1);
	CHECK(40, expect_cqes(&ring, (const struct expect[]){ {120, 1}, {121, 0}, {122, 0} }, 3));

	/* DRAIN 与链混用：DRAIN 链等先前的链完成，且在 SKIP_SUCCESS 之后 DRAIN 被拒绝。 */
	sqe = next_sqe(&ring);
	fill_read(sqe, pipes[0], buffer, 1, 130);
	sqe->flags = IOSQE_IO_LINK;
	fill_nop(next_sqe(&ring), 0, 0, 131);
	fill_nop(next_sqe(&ring), IOSQE_IO_DRAIN, 0, 132);
	publish(&ring);
	CHECK(41, uring_enter(ring.fd, 3, 0, 0, NULL, 0) == 3 && ready(&ring) == 0);
	CHECK(42, write(pipes[1], "e", 1) == 1);
	CHECK(43, expect_cqes(&ring, (const struct expect[]){ {130, 1}, {131, 0}, {132, 0} }, 3));
	/* 后继在 worker 线程上执行并访问提交者的 fd 表：阻塞读完成后 close 一个 fd。 */
	int extra = dup(pipes[0]);
	CHECK(46, extra >= 0);
	sqe = next_sqe(&ring);
	fill_read(sqe, pipes[0], buffer, 1, 105);
	sqe->flags = IOSQE_IO_LINK;
	sqe = next_sqe(&ring);
	sqe->opcode = IORING_OP_CLOSE;
	sqe->fd = extra;
	sqe->user_data = 106;
	publish(&ring);
	CHECK(47, uring_enter(ring.fd, 2, 0, 0, NULL, 0) == 2 && ready(&ring) == 0);
	CHECK(48, write(pipes[1], "w", 1) == 1);
	CHECK(49, expect_cqes(&ring, (const struct expect[]){ {105, 1}, {106, 0} }, 2));
	CHECK(50, fcntl(extra, F_GETFD) == -1 && errno == EBADF);

	/* SKIP_SUCCESS：成功成员不产生 CQE；失败成员自己可见，其后被取消的成员也不产生 CQE。 */
	fill_nop(next_sqe(&ring), IOSQE_IO_LINK | IOSQE_CQE_SKIP_SUCCESS, 0, 110);
	fill_nop(next_sqe(&ring), 0, 0, 111);
	fill_nop(next_sqe(&ring), IOSQE_IO_LINK | IOSQE_CQE_SKIP_SUCCESS, -5, 112);
	fill_nop(next_sqe(&ring), 0, 0, 113);
	publish(&ring);
	CHECK(34, uring_enter(ring.fd, 4, 2, IORING_ENTER_GETEVENTS, NULL, 0) == 4);
	CHECK(35, expect_cqes(&ring, (const struct expect[]){ {111, 0}, {112, -5} }, 2));
	CHECK(36, ready(&ring) == 0);

	fill_nop(next_sqe(&ring), IOSQE_CQE_SKIP_SUCCESS, 0, 140);
	fill_nop(next_sqe(&ring), IOSQE_IO_DRAIN, 0, 141);
	publish(&ring);
	CHECK(44, uring_enter(ring.fd, 2, 1, IORING_ENTER_GETEVENTS, NULL, 0) == 2);
	CHECK(45, expect_cqes(&ring, (const struct expect[]){ {141, -EOPNOTSUPP} }, 1));
	close(pipes[0]);
	close(pipes[1]);
	unmap_ring(&ring);
	return 0;
}

/* ---- socket：ACCEPT / CONNECT / SEND / RECV ---------------------------------------------------- */

static volatile sig_atomic_t sigpipe_seen;
static void on_sigpipe(int signal) { (void)signal; sigpipe_seen = 1; }

int verify_io_uring_sockets(void)
{
	struct ring ring;
	struct io_uring_cqe cqe;
	struct io_uring_sqe *sqe;
	struct sockaddr_un path = { .sun_family = AF_UNIX };
	char buffer[16];
	strcpy(path.sun_path, "/tmp/uring-socket");
	unlink(path.sun_path);
	CHECK(1, map_ring(&ring, 16, 0, 0) == 0);
	int listener = socket(AF_UNIX, SOCK_STREAM, 0);
	CHECK(2, listener >= 0 && bind(listener, (struct sockaddr *)&path, sizeof path) == 0 && listen(listener, 4) == 0);

	/* ACCEPT 先于连接提交：挂起，连接到来后以新 fd 完成，并写回对端地址长度。 */
	struct sockaddr_un peer;
	socklen_t peer_length = sizeof peer;
	sqe = next_sqe(&ring);
	sqe->opcode = IORING_OP_ACCEPT;
	sqe->fd = listener;
	sqe->addr = (uintptr_t)&peer;
	sqe->addr2 = (uintptr_t)&peer_length;
	sqe->user_data = 1;
	CHECK(3, submit_one(&ring) == 1 && ready(&ring) == 0);
	int client = socket(AF_UNIX, SOCK_STREAM, 0);
	CHECK(4, client >= 0 && connect(client, (struct sockaddr *)&path, sizeof path) == 0);
	CHECK(5, wait_cqe(&ring, &cqe) == 1 && cqe.user_data == 1 && cqe.res >= 0);
	int served = cqe.res;
	CHECK(6, peer_length >= sizeof(sa_family_t) && fcntl(served, F_GETFD) >= 0);

	/* RECV 先挂起，SEND 到来后唤醒；数据完整。 */
	sqe = next_sqe(&ring);
	sqe->opcode = IORING_OP_RECV;
	sqe->fd = served;
	sqe->addr = (uintptr_t)buffer;
	sqe->len = sizeof buffer;
	sqe->user_data = 2;
	CHECK(7, submit_one(&ring) == 1 && ready(&ring) == 0);
	sqe = next_sqe(&ring);
	sqe->opcode = IORING_OP_SEND;
	sqe->fd = client;
	sqe->addr = (uintptr_t)"hello";
	sqe->len = 5;
	sqe->user_data = 3;
	CHECK(8, submit_one(&ring) == 1);
	int saw_send = 0, saw_recv = 0;
	for (int i = 0; i < 2; i++) {
		CHECK(9, wait_cqe(&ring, &cqe) == 1);
		if (cqe.user_data == 3 && cqe.res == 5) saw_send = 1;
		else if (cqe.user_data == 2 && cqe.res == 5 && memcmp(buffer, "hello", 5) == 0) saw_recv = 1;
	}
	CHECK(10, saw_send && saw_recv);

	/* CONNECT 经 io_uring：成功为 0，之后可 accept；不存在的路径是 -ENOENT。 */
	int second = socket(AF_UNIX, SOCK_STREAM, 0);
	CHECK(11, second >= 0);
	sqe = next_sqe(&ring);
	sqe->opcode = IORING_OP_CONNECT;
	sqe->fd = second;
	sqe->addr = (uintptr_t)&path;
	sqe->off = sizeof path;
	sqe->user_data = 4;
	CHECK(12, submit_one(&ring) == 1 && wait_cqe(&ring, &cqe) == 1 && cqe.user_data == 4 && cqe.res == 0);
	int accepted = accept(listener, NULL, NULL);
	CHECK(13, accepted >= 0);
	struct sockaddr_un missing = { .sun_family = AF_UNIX };
	strcpy(missing.sun_path, "/tmp/uring-missing");
	int third = socket(AF_UNIX, SOCK_STREAM, 0);
	sqe = next_sqe(&ring);
	sqe->opcode = IORING_OP_CONNECT;
	sqe->fd = third;
	sqe->addr = (uintptr_t)&missing;
	sqe->off = sizeof missing;
	sqe->user_data = 5;
	CHECK(14, submit_one(&ring) == 1 && wait_cqe(&ring, &cqe) == 1 && cqe.res == -ENOENT);

	/* SEND 到已关闭的对端：-EPIPE，且不向提交线程投递 SIGPIPE。 */
	struct sigaction action = { .sa_handler = on_sigpipe };
	struct sigaction previous;
	CHECK(15, sigaction(SIGPIPE, &action, &previous) == 0);
	close(accepted);
	sqe = next_sqe(&ring);
	sqe->opcode = IORING_OP_SEND;
	sqe->fd = second;
	sqe->addr = (uintptr_t)"x";
	sqe->len = 1;
	sqe->user_data = 6;
	CHECK(16, submit_one(&ring) == 1 && wait_cqe(&ring, &cqe) == 1 && cqe.res == -EPIPE);
	CHECK(17, sigpipe_seen == 0);
	sigaction(SIGPIPE, &previous, NULL);

	/* prep：非 socket 的 SEND 是 -ENOTSOCK；未支持的 msg_flags 与目的地址是 -EOPNOTSUPP。 */
	int plain[2];
	CHECK(18, pipe(plain) == 0);
	sqe = next_sqe(&ring);
	sqe->opcode = IORING_OP_SEND;
	sqe->fd = plain[1];
	sqe->addr = (uintptr_t)"x";
	sqe->len = 1;
	sqe->user_data = 7;
	CHECK(19, submit_one(&ring) == 1 && wait_cqe(&ring, &cqe) == 1 && cqe.res == -ENOTSOCK);
	sqe = next_sqe(&ring);
	sqe->opcode = IORING_OP_RECV;
	sqe->fd = served;
	sqe->addr = (uintptr_t)buffer;
	sqe->len = 1;
	sqe->msg_flags = MSG_PEEK;
	sqe->user_data = 8;
	CHECK(20, submit_one(&ring) == 1 && reap(&ring, &cqe) == 1 && cqe.res == -EOPNOTSUPP);

	/* MSG_DONTWAIT 的 RECV 在没有数据时立即 -EAGAIN，不挂起。 */
	sqe = next_sqe(&ring);
	sqe->opcode = IORING_OP_RECV;
	sqe->fd = served;
	sqe->addr = (uintptr_t)buffer;
	sqe->len = 1;
	sqe->msg_flags = MSG_DONTWAIT;
	sqe->user_data = 9;
	CHECK(21, submit_one(&ring) == 1 && reap(&ring, &cqe) == 1 && cqe.res == -EAGAIN);

	/* 取消挂起的 ACCEPT 后监听 fd 的引用被释放：close 之后 connect 得到 ECONNREFUSED。 */
	sqe = next_sqe(&ring);
	sqe->opcode = IORING_OP_ACCEPT;
	sqe->fd = listener;
	sqe->user_data = 10;
	CHECK(22, submit_one(&ring) == 1 && ready(&ring) == 0);
	sqe = next_sqe(&ring);
	fill_cancel(sqe, IORING_OP_ASYNC_CANCEL, 10, 0, 11);
	CHECK(23, submit_one(&ring) == 1 && expect_pair(&ring, 10, -ECANCELED, 11, 0));
	close(listener);
	int late = socket(AF_UNIX, SOCK_STREAM, 0);
	errno = 0;
	CHECK(24, connect(late, (struct sockaddr *)&path, sizeof path) == -1 && errno == ECONNREFUSED);
	close(late);
	close(second);
	close(third);
	close(client);
	close(served);
	close(plain[0]);
	close(plain[1]);
	unlink(path.sun_path);
	unmap_ring(&ring);
	return 0;
}

/* 取 n 个 CQE，不要求顺序；返回 user_data 对应的 res（缺失返回 INT32_MIN）。 */
static int cqe_result(const struct io_uring_cqe *cqes, unsigned count, uint64_t user_data)
{
	for (unsigned i = 0; i < count; i++)
		if (cqes[i].user_data == user_data) return cqes[i].res;
	return INT32_MIN;
}

static int collect_cqes(struct ring *ring, struct io_uring_cqe *out, unsigned count)
{
	for (unsigned i = 0; i < count; i++)
		if (wait_cqe(ring, &out[i]) != 1) return 0;
	return 1;
}

/* ---- OPENAT ------------------------------------------------------------------------------------ */

static void fill_openat(struct io_uring_sqe *sqe, int dirfd, const char *path, unsigned flags, unsigned mode,
                        uint64_t user_data)
{
	sqe->opcode = IORING_OP_OPENAT;
	sqe->fd = dirfd;
	sqe->addr = (uintptr_t)path;
	sqe->open_flags = flags;
	sqe->len = mode;
	sqe->user_data = user_data;
}

int verify_io_uring_openat(void)
{
	struct ring ring;
	struct io_uring_cqe cqe;
	struct io_uring_sqe *sqe;
	char path[64];
	CHECK(1, map_ring(&ring, 8, 0, 0) == 0);
	unlink("/tmp/uring-openat");

	/* 创建并打开：结果是新 fd，且 O_CLOEXEC 保留在 fd 表里；路径在 prep 时复制，提交后改写缓冲区不影响。 */
	strcpy(path, "/tmp/uring-openat");
	fill_openat(next_sqe(&ring), AT_FDCWD, path, O_CREAT | O_RDWR | O_CLOEXEC, 0600, 1);
	CHECK(2, submit_one(&ring) == 1);
	memset(path, 'x', sizeof path - 1);
	CHECK(3, wait_cqe(&ring, &cqe) == 1 && cqe.user_data == 1 && cqe.res >= 0);
	int created = cqe.res;
	CHECK(4, (fcntl(created, F_GETFD) & FD_CLOEXEC) != 0);
	CHECK(5, write(created, "uring", 5) == 5);

	/* 相对路径与目录 fd：在 /tmp 下按相对名字打开刚才创建的文件，读到同样的内容。 */
	int dir = open("/tmp", O_RDONLY | O_DIRECTORY);
	CHECK(6, dir >= 0);
	fill_openat(next_sqe(&ring), dir, "uring-openat", O_RDONLY, 0, 2);
	CHECK(7, submit_one(&ring) == 1 && wait_cqe(&ring, &cqe) == 1 && cqe.user_data == 2 && cqe.res >= 0);
	int relative = cqe.res;
	char buffer[8] = { 0 };
	CHECK(8, pread(relative, buffer, sizeof buffer, 0) == 5 && memcmp(buffer, "uring", 5) == 0);

	/* 错误：不存在的文件 -ENOENT，无效目录 fd -EBADF，O_CREAT|O_EXCL 遇到已存在文件 -EEXIST。 */
	fill_openat(next_sqe(&ring), AT_FDCWD, "/tmp/uring-openat-missing", O_RDONLY, 0, 3);
	CHECK(9, submit_one(&ring) == 1 && wait_cqe(&ring, &cqe) == 1 && cqe.res == -ENOENT);
	fill_openat(next_sqe(&ring), 9999, "relative", O_RDONLY, 0, 4);
	CHECK(10, submit_one(&ring) == 1 && wait_cqe(&ring, &cqe) == 1 && cqe.res == -EBADF);
	fill_openat(next_sqe(&ring), AT_FDCWD, "/tmp/uring-openat", O_CREAT | O_EXCL | O_RDWR, 0600, 5);
	CHECK(11, submit_one(&ring) == 1 && wait_cqe(&ring, &cqe) == 1 && cqe.res == -EEXIST);

	/* prep 校验：空路径 -ENOENT，直接描述符（file_index）与 FIXED_FILE 分别是 -EOPNOTSUPP / -EBADF。 */
	fill_openat(next_sqe(&ring), AT_FDCWD, "", O_RDONLY, 0, 6);
	CHECK(12, submit_one(&ring) == 1 && wait_cqe(&ring, &cqe) == 1 && cqe.res == -ENOENT);
	sqe = next_sqe(&ring);
	fill_openat(sqe, AT_FDCWD, "/tmp/uring-openat", O_RDONLY, 0, 7);
	sqe->file_index = 1;
	CHECK(13, submit_one(&ring) == 1 && wait_cqe(&ring, &cqe) == 1 && cqe.res == -EOPNOTSUPP);
	sqe = next_sqe(&ring);
	fill_openat(sqe, AT_FDCWD, "/tmp/uring-openat", O_RDONLY, 0, 8);
	sqe->flags = IOSQE_FIXED_FILE;
	CHECK(14, submit_one(&ring) == 1 && wait_cqe(&ring, &cqe) == 1 && cqe.res == -EBADF);

	/* 链：openat 失败（短于预期的 -ENOENT）取消其后成员。 */
	sqe = next_sqe(&ring);
	fill_openat(sqe, AT_FDCWD, "/tmp/uring-openat-missing", O_RDONLY, 0, 9);
	sqe->flags = IOSQE_IO_LINK;
	fill_nop(next_sqe(&ring), 0, 0, 10);
	publish(&ring);
	CHECK(15, uring_enter(ring.fd, 2, 0, 0, NULL, 0) == 2);
	CHECK(16, expect_cqes(&ring, (const struct expect[]){ {9, -ENOENT}, {10, -ECANCELED} }, 2));

	close(created);
	close(relative);
	close(dir);
	unlink("/tmp/uring-openat");
	unmap_ring(&ring);
	return 0;
}

/* ---- 车道作业的取消：未开始可取消，已开始不可取消（-EALREADY）且照常完成 ----------------------------- */

int verify_io_uring_lane_cancel(void)
{
	/* 车道最多 8 个 worker（lane.rs 的 MAX_WORKERS）：8 个对 FIFO 的阻塞 O_RDONLY openat 在写端出现之前
	 * 一直阻塞在 worker 上，使它们全部处于“已开始”状态；第 9 个作业因此只能排队，尚未开始。 */
	enum { WORKERS = 8 };
	struct ring ring;
	struct io_uring_cqe cqe;
	char names[WORKERS][48];
	CHECK(1, map_ring(&ring, 32, 0, 0) == 0);
	unlink("/tmp/uring-lane-never");
	for (int i = 0; i < WORKERS; i++) {
		snprintf(names[i], sizeof names[i], "/tmp/uring-lane-fifo-%d", i);
		unlink(names[i]);
		CHECK(2, mkfifo(names[i], 0600) == 0);
		fill_openat(next_sqe(&ring), AT_FDCWD, names[i], O_RDONLY, 0, 100 + i);
	}
	fill_openat(next_sqe(&ring), AT_FDCWD, "/tmp/uring-lane-never", O_CREAT | O_WRONLY, 0600, 200);
	publish(&ring);
	CHECK(3, uring_enter(ring.fd, WORKERS + 1, 0, 0, NULL, 0) == WORKERS + 1);
	/* 给 worker 时间开始各自的作业（它们的开始延迟远小于这个时间）。 */
	struct timespec pause = { .tv_nsec = 300 * 1000 * 1000 };
	nanosleep(&pause, NULL);
	CHECK(4, ready(&ring) == 0);

	/* 排队中的作业可以取消：取消请求得到 0，目标得到 -ECANCELED，之后绝不会执行（文件不会被创建）。 */
	fill_cancel(next_sqe(&ring), IORING_OP_ASYNC_CANCEL, 200, 0, 300);
	CHECK(5, submit_one(&ring) == 1 && expect_pair(&ring, 200, -ECANCELED, 300, 0));

	/* 已开始的作业不能取消：取消请求得到 -EALREADY，目标仍在运行（没有 CQE）。 */
	fill_cancel(next_sqe(&ring), IORING_OP_ASYNC_CANCEL, 100, 0, 301);
	CHECK(6, submit_one(&ring) == 1 && wait_cqe(&ring, &cqe) == 1 && cqe.user_data == 301 &&
		 cqe.res == -EALREADY);
	CHECK(7, ready(&ring) == 0);

	/* 打开写端：与阻塞在 worker 上的读端会合，8 个作业都以真实结果（新 fd）完成，而不是 -ECANCELED。 */
	int writers[WORKERS];
	for (int i = 0; i < WORKERS; i++) {
		writers[i] = open(names[i], O_WRONLY);
		CHECK(8, writers[i] >= 0);
	}
	unsigned seen = 0;
	int readers[WORKERS];
	for (int i = 0; i < WORKERS; i++) {
		CHECK(9, wait_cqe(&ring, &cqe) == 1);
		CHECK(10, cqe.user_data >= 100 && cqe.user_data < 100 + WORKERS && cqe.res >= 0);
		readers[cqe.user_data - 100] = cqe.res;
		seen |= 1u << (cqe.user_data - 100);
	}
	CHECK(11, seen == (1u << WORKERS) - 1);

	/* 被取消的作业在 FIFO 里排在这个之前，worker 取到它时已经跳过：文件从未被创建。 */
	fill_openat(next_sqe(&ring), AT_FDCWD, "/tmp", O_RDONLY | O_DIRECTORY, 0, 400);
	CHECK(12, submit_one(&ring) == 1 && wait_cqe(&ring, &cqe) == 1 && cqe.user_data == 400 && cqe.res >= 0);
	close(cqe.res);
	CHECK(13, access("/tmp/uring-lane-never", F_OK) == -1 && errno == ENOENT);

	for (int i = 0; i < WORKERS; i++) {
		close(readers[i]);
		close(writers[i]);
		unlink(names[i]);
	}
	unmap_ring(&ring);
	return 0;
}

/* ---- ring 大小上限：与 Linux 相同的 32768 / 65536 --------------------------------------------- */

int verify_io_uring_size_limits(void)
{
	struct io_uring_params params;
	memset(&params, 0, sizeof params);
	long largest = uring_setup(32768, &params);
	CHECK(1, largest >= 0 && params.sq_entries == 32768 && params.cq_entries == 65536);
	close((int)largest);
	memset(&params, 0, sizeof params);
	CHECK(2, uring_setup(32769, &params) < 0 && errno == EINVAL);
	memset(&params, 0, sizeof params);
	params.flags = IORING_SETUP_CLAMP;
	long clamped = uring_setup(1u << 20, &params);
	CHECK(3, clamped >= 0 && params.sq_entries == 32768);
	close((int)clamped);
	/* 最大 ring 约 3.2 MB：每 UID 的预算只容得下有限个，超出得到 ENOMEM（不是 EINVAL）。 */
	enum { MAX_BIG = 64 };
	static int fds[MAX_BIG];
	int count = 0;
	for (;;) {
		CHECK(4, count < MAX_BIG);
		memset(&params, 0, sizeof params);
		long fd = uring_setup(32768, &params);
		if (fd < 0) {
			CHECK(5, errno == ENOMEM);
			break;
		}
		fds[count++] = (int)fd;
	}
	CHECK(6, count >= 10 && count <= 25);
	for (int i = 0; i < count; i++) close(fds[i]);
	memset(&params, 0, sizeof params);
	long fresh = uring_setup(32768, &params);
	CHECK(7, fresh >= 0);
	close((int)fresh);
	return 0;
}

/* ---- ring 内存配额：每个真实 UID 有固定预算，超出得到 -ENOMEM ------------------------------------ */

int verify_io_uring_quota(void)
{
	enum { MAX_RINGS = 400 };
	static int fds[MAX_RINGS];
	int count = 0;
	/* 4096 条目的 ring 约 400 KiB：不断创建直到预算耗尽。 */
	for (;;) {
		CHECK(1, count < MAX_RINGS);
		struct io_uring_params params;
		memset(&params, 0, sizeof params);
		long fd = uring_setup(4096, &params);
		if (fd < 0) {
			CHECK(2, errno == ENOMEM);
			break;
		}
		fds[count++] = (int)fd;
	}
	CHECK(3, count >= 20);

	/* 关闭一个（没有映射）：配额归还，恰好能再建一个。 */
	close(fds[0]);
	struct io_uring_params params;
	memset(&params, 0, sizeof params);
	long again = uring_setup(4096, &params);
	CHECK(4, again >= 0);
	fds[0] = (int)again;
	memset(&params, 0, sizeof params);
	CHECK(5, uring_setup(4096, &params) < 0 && errno == ENOMEM);

	/* 映射保活 ring：mmap 之后关闭 fd 不归还配额（页仍在用户态可见），munmap 之后才归还。 */
	void *mapped = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fds[1], IORING_OFF_SQ_RING);
	CHECK(6, mapped != MAP_FAILED);
	close(fds[1]);
	memset(&params, 0, sizeof params);
	CHECK(7, uring_setup(4096, &params) < 0 && errno == ENOMEM);
	CHECK(8, munmap(mapped, 4096) == 0);
	memset(&params, 0, sizeof params);
	long freed = uring_setup(4096, &params);
	CHECK(9, freed >= 0);
	close((int)freed);

	for (int i = 0; i < count; i++)
		if (i != 1) close(fds[i]);
	/* 全部归还后预算恢复：可以重新创建。 */
	memset(&params, 0, sizeof params);
	long fresh = uring_setup(4096, &params);
	CHECK(10, fresh >= 0);
	close((int)fresh);
	return 0;
}

/* ---- ASYNC_CANCEL 按 fd / opcode / user_data 条件匹配 ------------------------------------------ */

#ifndef IORING_ASYNC_CANCEL_USERDATA
#define IORING_ASYNC_CANCEL_USERDATA (1U << 4)
#endif
#ifndef IORING_ASYNC_CANCEL_OP
#define IORING_ASYNC_CANCEL_OP (1U << 5)
#endif

int verify_io_uring_cancel_criteria(void)
{
	struct ring ring;
	struct io_uring_cqe cqes[4];
	struct io_uring_sqe *sqe;
	char buffer[16];
	int first[2], second[2];
	CHECK(1, map_ring(&ring, 16, 0, 0) == 0 && pipe(first) == 0 && pipe(second) == 0);

	/* 两个读分别等在两个管道上；按第一个管道的 fd 取消（FD|ALL）：只取消它。 */
	fill_read(next_sqe(&ring), first[0], buffer, sizeof buffer, 1);
	fill_read(next_sqe(&ring), second[0], buffer, sizeof buffer, 2);
	publish(&ring);
	CHECK(2, uring_enter(ring.fd, 2, 0, 0, NULL, 0) == 2 && ready(&ring) == 0);
	sqe = next_sqe(&ring);
	fill_cancel(sqe, IORING_OP_ASYNC_CANCEL, 0, IORING_ASYNC_CANCEL_FD | IORING_ASYNC_CANCEL_ALL, 10);
	sqe->fd = first[0];
	CHECK(3, submit_one(&ring) == 1);
	CHECK(4, collect_cqes(&ring, cqes, 2));
	CHECK(5, cqe_result(cqes, 2, 10) == 1 && cqe_result(cqes, 2, 1) == -ECANCELED);
	CHECK(6, ready(&ring) == 0);

	/* fd 与 user_data 叠加：fd 匹配但 user_data 不匹配 -> -ENOENT；都匹配才取消。 */
	sqe = next_sqe(&ring);
	fill_cancel(sqe, IORING_OP_ASYNC_CANCEL, 999, IORING_ASYNC_CANCEL_FD | IORING_ASYNC_CANCEL_USERDATA, 11);
	sqe->fd = second[0];
	CHECK(7, submit_one(&ring) == 1 && wait_cqe(&ring, &cqes[0]) == 1 && cqes[0].user_data == 11 &&
		 cqes[0].res == -ENOENT);
	sqe = next_sqe(&ring);
	fill_cancel(sqe, IORING_OP_ASYNC_CANCEL, 2, IORING_ASYNC_CANCEL_FD | IORING_ASYNC_CANCEL_USERDATA, 12);
	sqe->fd = second[0];
	CHECK(8, submit_one(&ring) == 1 && expect_pair(&ring, 12, 0, 2, -ECANCELED));

	/* 按 opcode：IORING_OP_WRITE 的条件不匹配读；IORING_OP_READ|ALL 取消全部读并返回个数。 */
	fill_read(next_sqe(&ring), first[0], buffer, sizeof buffer, 3);
	fill_read(next_sqe(&ring), second[0], buffer, sizeof buffer, 4);
	publish(&ring);
	CHECK(9, uring_enter(ring.fd, 2, 0, 0, NULL, 0) == 2 && ready(&ring) == 0);
	sqe = next_sqe(&ring);
	fill_cancel(sqe, IORING_OP_ASYNC_CANCEL, 0, IORING_ASYNC_CANCEL_OP, 13);
	sqe->len = IORING_OP_WRITE;
	CHECK(10, submit_one(&ring) == 1 && wait_cqe(&ring, &cqes[0]) == 1 && cqes[0].user_data == 13 &&
		  cqes[0].res == -ENOENT);
	sqe = next_sqe(&ring);
	fill_cancel(sqe, IORING_OP_ASYNC_CANCEL, 0, IORING_ASYNC_CANCEL_OP | IORING_ASYNC_CANCEL_ALL, 14);
	sqe->len = IORING_OP_READ;
	CHECK(11, submit_one(&ring) == 1);
	CHECK(12, collect_cqes(&ring, cqes, 3));
	CHECK(13, cqe_result(cqes, 3, 14) == 2 && cqe_result(cqes, 3, 3) == -ECANCELED &&
		  cqe_result(cqes, 3, 4) == -ECANCELED);

	/* FD 与 ANY 互斥；fixed fd 没有注册文件表 -> -EBADF。 */
	sqe = next_sqe(&ring);
	fill_cancel(sqe, IORING_OP_ASYNC_CANCEL, 0, IORING_ASYNC_CANCEL_FD | IORING_ASYNC_CANCEL_ANY, 15);
	sqe->fd = first[0];
	CHECK(14, submit_one(&ring) == 1 && wait_cqe(&ring, &cqes[0]) == 1 && cqes[0].res == -EINVAL);
	sqe = next_sqe(&ring);
	fill_cancel(sqe, IORING_OP_ASYNC_CANCEL, 0, IORING_ASYNC_CANCEL_FD | IORING_ASYNC_CANCEL_FD_FIXED, 16);
	sqe->fd = 0;
	CHECK(15, submit_one(&ring) == 1 && wait_cqe(&ring, &cqes[0]) == 1 && cqes[0].res == -EBADF);

	close(first[0]);
	close(first[1]);
	close(second[0]);
	close(second[1]);
	unmap_ring(&ring);
	return 0;
}

/* ---- CONNECT 遇到 Unix backlog 已满：挂起等容量，而不是 -EAGAIN -------------------------------- */

int verify_io_uring_connect_backlog(void)
{
	struct ring ring;
	struct io_uring_cqe cqe;
	struct io_uring_sqe *sqe;
	struct sockaddr_un path = { .sun_family = AF_UNIX };
	strcpy(path.sun_path, "/tmp/uring-backlog");
	unlink(path.sun_path);
	CHECK(1, map_ring(&ring, 8, 0, 0) == 0);
	int listener = socket(AF_UNIX, SOCK_STREAM, 0);
	CHECK(2, listener >= 0 && bind(listener, (struct sockaddr *)&path, sizeof path) == 0 &&
		 listen(listener, 1) == 0);

	/* 用非阻塞 connect 把 backlog 塞满：直到 -EAGAIN。 */
	int fillers[8];
	int filled = 0;
	for (;;) {
		CHECK(3, filled < 8);
		int fd = socket(AF_UNIX, SOCK_STREAM | SOCK_NONBLOCK, 0);
		CHECK(4, fd >= 0);
		fillers[filled] = fd;
		if (connect(fd, (struct sockaddr *)&path, sizeof path) == 0) {
			filled++;
			continue;
		}
		CHECK(5, errno == EAGAIN);
		filled++;
		break;
	}

	/* 阻塞 socket 经 io_uring CONNECT：不立即完成。腾出容量（accept 一个）后以 0 完成。 */
	int waiter_fd = socket(AF_UNIX, SOCK_STREAM, 0);
	CHECK(6, waiter_fd >= 0);
	sqe = next_sqe(&ring);
	sqe->opcode = IORING_OP_CONNECT;
	sqe->fd = waiter_fd;
	sqe->addr = (uintptr_t)&path;
	sqe->off = sizeof path;
	sqe->user_data = 1;
	CHECK(7, submit_one(&ring) == 1 && ready(&ring) == 0);
	int accepted = accept(listener, NULL, NULL);
	CHECK(8, accepted >= 0);
	CHECK(9, wait_cqe(&ring, &cqe) == 1 && cqe.user_data == 1 && cqe.res == 0);

	/* 同样的等待可以被取消：被取消的 CONNECT 得到 -ECANCELED，ring 之后仍可用。 */
	int canceled_fd = socket(AF_UNIX, SOCK_STREAM, 0);
	CHECK(10, canceled_fd >= 0);
	sqe = next_sqe(&ring);
	sqe->opcode = IORING_OP_CONNECT;
	sqe->fd = canceled_fd;
	sqe->addr = (uintptr_t)&path;
	sqe->off = sizeof path;
	sqe->user_data = 2;
	CHECK(11, submit_one(&ring) == 1 && ready(&ring) == 0);
	fill_cancel(next_sqe(&ring), IORING_OP_ASYNC_CANCEL, 2, 0, 3);
	CHECK(12, submit_one(&ring) == 1 && expect_pair(&ring, 2, -ECANCELED, 3, 0));

	close(canceled_fd);
	close(accepted);
	close(waiter_fd);
	for (int i = 0; i < filled; i++) close(fillers[i]);
	close(listener);
	unlink(path.sun_path);
	unmap_ring(&ring);
	return 0;
}

/* ---- 冷读：page cache 未命中走异步填页 ------------------------------------------------------- */

/* 与 scripts/verify_musl.py 生成冷文件的公式一致；hole_page 整页为零。 */
static uint8_t cold_byte(size_t offset, long hole_page)
{
	if (hole_page >= 0 && offset / 4096 == (size_t)hole_page) return 0;
	return (uint8_t)((offset * 131) + (offset >> 12) * 17 + 7);
}

static int cold_matches(const uint8_t *data, size_t offset, size_t length, long hole_page)
{
	for (size_t i = 0; i < length; i++)
		if (data[i] != cold_byte(offset + i, hole_page)) return 0;
	return 1;
}

int verify_io_uring_cold(void)
{
	struct ring ring;
	struct io_uring_cqe cqe;
	struct io_uring_sqe *sqe;
	static uint8_t buffer[8 * 4096];
	const size_t a_size = 5 * 4096 + 1234;
	CHECK(1, map_ring(&ring, 16, 0, 0) == 0);

	/* 冷文件 a：多页、含一整页 hole、不对齐的尾部。第一次读未命中，第二次读命中且在 enter 返回前完成。 */
	int a = open("/usr/lib/uring-cold-a.bin", O_RDONLY);
	CHECK(2, a >= 0);
	memset(buffer, 0xee, sizeof buffer);
	sqe = next_sqe(&ring);
	fill_read(sqe, a, buffer, a_size, 1);
	sqe->off = 0;
	CHECK(3, submit_one(&ring) == 1 && wait_cqe(&ring, &cqe) == 1);
	CHECK(4, cqe.user_data == 1 && cqe.res == (int)a_size && cold_matches(buffer, 0, a_size, 2));
	memset(buffer, 0xee, sizeof buffer);
	sqe = next_sqe(&ring);
	fill_read(sqe, a, buffer, a_size, 2);
	sqe->off = 0;
	CHECK(5, submit_one(&ring) == 1 && ready(&ring) == 1 && reap(&ring, &cqe) == 1);
	CHECK(6, cqe.res == (int)a_size && cold_matches(buffer, 0, a_size, 2));

	/* EOF：尾部短读与越过 EOF 的读。 */
	sqe = next_sqe(&ring);
	fill_read(sqe, a, buffer, 4096, 3);
	sqe->off = a_size - 100;
	CHECK(7, submit_one(&ring) == 1 && wait_cqe(&ring, &cqe) == 1 && cqe.res == 100);
	CHECK(8, cold_matches(buffer, a_size - 100, 100, 2));
	sqe = next_sqe(&ring);
	fill_read(sqe, a, buffer, 4096, 4);
	sqe->off = a_size + 4096;
	CHECK(9, submit_one(&ring) == 1 && wait_cqe(&ring, &cqe) == 1 && cqe.res == 0);

	/* 冷文件 b：一次提交 8 个单页读，全部在途，各自得到正确内容。 */
	int b = open("/usr/lib/uring-cold-b.bin", O_RDONLY);
	CHECK(10, b >= 0);
	memset(buffer, 0xee, sizeof buffer);
	for (unsigned page = 0; page < 8; page++) {
		sqe = next_sqe(&ring);
		fill_read(sqe, b, buffer + page * 4096, 4096, 100 + page);
		sqe->off = page * 4096;
	}
	publish(&ring);
	CHECK(11, uring_enter(ring.fd, 8, 0, 0, NULL, 0) == 8);
	unsigned seen = 0;
	for (unsigned i = 0; i < 8; i++) {
		CHECK(12, wait_cqe(&ring, &cqe) == 1 && cqe.res == 4096);
		seen |= 1u << (cqe.user_data - 100);
	}
	CHECK(13, seen == 0xff && cold_matches(buffer, 0, sizeof buffer, -1));

	/* 读在途时关闭 ring：不崩溃、不泄漏请求槽；之后同一个文件仍能正确读出（cache 没有被毒化）。 */
	int c = open("/usr/lib/uring-cold-c.bin", O_RDONLY);
	CHECK(14, c >= 0);
	struct ring doomed;
	CHECK(15, map_ring(&doomed, 4, 0, 0) == 0);
	sqe = next_sqe(&doomed);
	fill_read(sqe, c, buffer, 3 * 4096, 1);
	sqe->off = 0;
	CHECK(16, submit_one(&doomed) == 1);
	unmap_ring(&doomed);
	CHECK(17, pread(c, buffer, 3 * 4096, 0) == 3 * 4096 && cold_matches(buffer, 0, 3 * 4096, -1));

	/* 读在途时提交线程退出：请求被取消，父进程之后读到完整正确的内容。 */
	int d = open("/usr/lib/uring-cold-d.bin", O_RDONLY);
	CHECK(18, d >= 0);
	pid_t child = fork();
	CHECK(19, child >= 0);
	if (child == 0) {
		struct ring theirs;
		static uint8_t scratch[2 * 4096];
		if (map_ring(&theirs, 4, 0, 0) != 0) _exit(2);
		struct io_uring_sqe *queued = next_sqe(&theirs);
		fill_read(queued, d, scratch, sizeof scratch, 1);
		queued->off = 0;
		publish(&theirs);
		_exit(uring_enter(theirs.fd, 1, 0, 0, NULL, 0) == 1 ? 0 : 1);
	}
	int status = -1;
	CHECK(20, waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0);
	CHECK(21, pread(d, buffer, 2 * 4096, 0) == 2 * 4096 && cold_matches(buffer, 0, 2 * 4096, -1));

	/* 写后读一致：写走阻塞车道，随后的读（命中）看到刚写入的内容。 */
	int w = open("/tmp/uring-cold-write", O_RDWR | O_CREAT | O_TRUNC, 0600);
	CHECK(22, w >= 0);
	static uint8_t pattern[3 * 4096];
	for (size_t i = 0; i < sizeof pattern; i++) pattern[i] = (uint8_t)(i * 7 + 1);
	sqe = next_sqe(&ring);
	fill_write(sqe, w, pattern, sizeof pattern, 200);
	sqe->off = 0;
	sqe->flags = IOSQE_IO_LINK;
	sqe = next_sqe(&ring);
	fill_read(sqe, w, buffer, sizeof pattern, 201);
	sqe->off = 0;
	publish(&ring);
	CHECK(23, uring_enter(ring.fd, 2, 0, 0, NULL, 0) == 2);
	CHECK(24, expect_cqes(&ring, (const struct expect[]){ {200, (int)sizeof pattern}, {201, (int)sizeof pattern} }, 2));
	CHECK(25, memcmp(buffer, pattern, sizeof pattern) == 0);
	close(w);
	unlink("/tmp/uring-cold-write");

	/* 当前位置读（off == -1）的冷读：异步填页，完成时推进共享 position；之后同步 read(2) 从推进后的位置继续。 */
	int e = open("/usr/lib/uring-cold-e.bin", O_RDONLY);
	CHECK(26, e >= 0);
	memset(buffer, 0xee, sizeof buffer);
	sqe = next_sqe(&ring);
	fill_read(sqe, e, buffer, 4096, 300);
	CHECK(27, submit_one(&ring) == 1 && wait_cqe(&ring, &cqe) == 1 && cqe.res == 4096);
	CHECK(28, cold_matches(buffer, 0, 4096, -1) && lseek(e, 0, SEEK_CUR) == 4096);
	sqe = next_sqe(&ring);
	fill_read(sqe, e, buffer, 4096, 301);
	CHECK(29, submit_one(&ring) == 1 && wait_cqe(&ring, &cqe) == 1 && cqe.res == 4096);
	CHECK(30, cold_matches(buffer, 4096, 4096, -1) && lseek(e, 0, SEEK_CUR) == 8192);
	CHECK(31, read(e, buffer, 4096) == 4096 && cold_matches(buffer, 8192, 4096, -1));
	sqe = next_sqe(&ring);
	fill_read(sqe, e, buffer, 4096, 302);
	CHECK(32, submit_one(&ring) == 1 && wait_cqe(&ring, &cqe) == 1 && cqe.res == 0 && lseek(e, 0, SEEK_CUR) == 12288);
	close(e);

	/* 阻塞车道是 worker 池：超过池上限的写+fsync 链同时在途，全部完成且数据落盘可读。 */
	enum { LANE_FILES = 12 };
	struct ring wide;
	CHECK(33, map_ring(&wide, 64, 0, 0) == 0);
	static uint8_t block[LANE_FILES][4096];
	int lane_fd[LANE_FILES];
	for (int i = 0; i < LANE_FILES; i++) {
		char name[48];
		snprintf(name, sizeof name, "/tmp/uring-lane-%d", i);
		lane_fd[i] = open(name, O_RDWR | O_CREAT | O_TRUNC, 0600);
		CHECK(34, lane_fd[i] >= 0);
		for (size_t j = 0; j < sizeof block[i]; j++) block[i][j] = (uint8_t)(j * 3 + i);
		sqe = next_sqe(&wide);
		fill_write(sqe, lane_fd[i], block[i], sizeof block[i], 400 + i);
		sqe->off = 0;
		sqe->flags = IOSQE_IO_LINK;
		sqe = next_sqe(&wide);
		sqe->opcode = IORING_OP_FSYNC;
		sqe->fd = lane_fd[i];
		sqe->user_data = 500 + i;
	}
	publish(&wide);
	CHECK(35, uring_enter(wide.fd, 2 * LANE_FILES, 0, 0, NULL, 0) == 2 * LANE_FILES);
	unsigned long long done = 0;
	for (int i = 0; i < 2 * LANE_FILES; i++) {
		CHECK(36, wait_cqe(&wide, &cqe) == 1);
		int index = cqe.user_data >= 500 ? (int)cqe.user_data - 500 : (int)cqe.user_data - 400;
		CHECK(37, index >= 0 && index < LANE_FILES);
		CHECK(38, cqe.res == (cqe.user_data >= 500 ? 0 : 4096));
		done |= 1ull << (cqe.user_data >= 500 ? LANE_FILES + index : index);
	}
	CHECK(39, done == (1ull << (2 * LANE_FILES)) - 1);
	for (int i = 0; i < LANE_FILES; i++) {
		char name[48];
		uint8_t readback[4096];
		CHECK(40, pread(lane_fd[i], readback, sizeof readback, 0) == (ssize_t)sizeof readback &&
			  memcmp(readback, block[i], sizeof readback) == 0);
		close(lane_fd[i]);
		snprintf(name, sizeof name, "/tmp/uring-lane-%d", i);
		unlink(name);
	}
	unmap_ring(&wide);
	close(a);
	close(b);
	close(c);
	close(d);
	unmap_ring(&ring);
	return 0;
}

/* ---- LINK_TIMEOUT ------------------------------------------------------------------------------ */

#ifndef IORING_OP_LINK_TIMEOUT
#define IORING_OP_LINK_TIMEOUT 15
#endif

static void fill_link_timeout(struct io_uring_sqe *sqe, const struct __kernel_timespec *time, uint8_t flags,
                              uint64_t user_data)
{
	sqe->opcode = IORING_OP_LINK_TIMEOUT;
	sqe->fd = -1;
	sqe->addr = (uintptr_t)time;
	sqe->len = 1;
	sqe->flags = flags;
	sqe->user_data = user_data;
}

int verify_io_uring_link_timeout(void)
{
	struct ring ring;
	struct io_uring_cqe cqes[4];
	struct io_uring_sqe *sqe;
	char buffer[16];
	int pipes[2];
	const struct __kernel_timespec soon = { .tv_nsec = 50 * 1000 * 1000 };
	const struct __kernel_timespec later = { .tv_sec = 30 };
	CHECK(1, map_ring(&ring, 16, 0, 0) == 0 && pipe(pipes) == 0);

	/* 定时器先到：被约束的读得到 -ECANCELED，timeout 得到 -ETIME，链上其后的成员按链规则取消。 */
	sqe = next_sqe(&ring);
	fill_read(sqe, pipes[0], buffer, sizeof buffer, 1);
	sqe->flags = IOSQE_IO_LINK;
	fill_link_timeout(next_sqe(&ring), &soon, IOSQE_IO_LINK, 2);
	fill_nop(next_sqe(&ring), 0, 0, 3);
	publish(&ring);
	CHECK(2, uring_enter(ring.fd, 3, 0, 0, NULL, 0) == 3);
	CHECK(3, collect_cqes(&ring, cqes, 3));
	CHECK(4, cqe_result(cqes, 3, 1) == -ECANCELED && cqe_result(cqes, 3, 2) == -ETIME &&
		 cqe_result(cqes, 3, 3) == -ECANCELED);

	/* 成员先完成（管道里有数据，同步完成）：timeout 得到 -ECANCELED，且不使链断开。 */
	CHECK(5, write(pipes[1], "x", 1) == 1);
	sqe = next_sqe(&ring);
	fill_read(sqe, pipes[0], buffer, 1, 10); /* 恰好 1 字节：短读会按链规则取消后继 */
	sqe->flags = IOSQE_IO_LINK;
	fill_link_timeout(next_sqe(&ring), &later, IOSQE_IO_LINK, 11);
	fill_nop(next_sqe(&ring), 0, 0, 12);
	publish(&ring);
	CHECK(6, uring_enter(ring.fd, 3, 0, 0, NULL, 0) == 3);
	CHECK(7, collect_cqes(&ring, cqes, 3));
	CHECK(8, cqe_result(cqes, 3, 10) == 1 && cqe_result(cqes, 3, 11) == -ECANCELED &&
		 cqe_result(cqes, 3, 12) == 0);

	/* 成员开始等待、随后在 deadline 之前完成：timeout 同样得到 -ECANCELED。 */
	sqe = next_sqe(&ring);
	fill_read(sqe, pipes[0], buffer, sizeof buffer, 20);
	sqe->flags = IOSQE_IO_LINK;
	fill_link_timeout(next_sqe(&ring), &later, 0, 21);
	publish(&ring);
	CHECK(9, uring_enter(ring.fd, 2, 0, 0, NULL, 0) == 2 && ready(&ring) == 0);
	CHECK(10, write(pipes[1], "y", 1) == 1);
	CHECK(11, collect_cqes(&ring, cqes, 2));
	CHECK(12, cqe_result(cqes, 2, 20) == 1 && cqe_result(cqes, 2, 21) == -ECANCELED);

	/* 位置非法：孤立的 timeout、跟在另一个 timeout 之后的 timeout 都是 -EINVAL。 */
	fill_link_timeout(next_sqe(&ring), &soon, 0, 30);
	CHECK(13, submit_one(&ring) == 1 && wait_cqe(&ring, &cqes[0]) == 1 && cqes[0].user_data == 30 &&
		  cqes[0].res == -EINVAL);
	sqe = next_sqe(&ring);
	fill_nop(sqe, IOSQE_IO_LINK, 0, 40);
	fill_link_timeout(next_sqe(&ring), &soon, IOSQE_IO_LINK, 41);
	fill_link_timeout(next_sqe(&ring), &soon, 0, 42);
	publish(&ring);
	CHECK(14, uring_enter(ring.fd, 3, 0, 0, NULL, 0) >= 1);
	CHECK(15, collect_cqes(&ring, cqes, 3));
	CHECK(16, cqe_result(cqes, 3, 42) == -EINVAL && cqe_result(cqes, 3, 40) == -ECANCELED &&
		  cqe_result(cqes, 3, 41) == -ECANCELED);

	close(pipes[0]);
	close(pipes[1]);
	unmap_ring(&ring);
	return 0;
}

int verify_io_uring(void)
{
	int core = verify_io_uring_core();
	if (core != 0) return core;
	int io = verify_io_uring_io();
	if (io != 0) return 100 + io;
	int events = verify_io_uring_events();
	if (events != 0) return 150 + events;
	int chains = verify_io_uring_chains();
	if (chains != 0) return 200 + chains;
	int sockets = verify_io_uring_sockets();
	if (sockets != 0) return 300 + sockets;
	int cold = verify_io_uring_cold();
	if (cold != 0) return 400 + cold;
	int link_timeout = verify_io_uring_link_timeout();
	if (link_timeout != 0) return 500 + link_timeout;
	int backlog = verify_io_uring_connect_backlog();
	if (backlog != 0) return 600 + backlog;
	int criteria = verify_io_uring_cancel_criteria();
	if (criteria != 0) return 700 + criteria;
	int quota = verify_io_uring_quota();
	if (quota != 0) return 800 + quota;
	int limits = verify_io_uring_size_limits();
	if (limits != 0) return 900 + limits;
	int openat_result = verify_io_uring_openat();
	if (openat_result != 0) return 1000 + openat_result;
	int lane_cancel = verify_io_uring_lane_cancel();
	return lane_cancel != 0 ? 1100 + lane_cancel : 0;
}
