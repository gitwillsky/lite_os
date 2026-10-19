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

	/* 逐类 prep 错误：已知但未实现的 opcode / 链接 / buffer-select 是 EOPNOTSUPP，未知位与非零 ioprio 是 EINVAL。 */
	static const struct { uint8_t opcode, flags; uint16_t ioprio; uint32_t nop_flags; int expected; } cases[] = {
		{ IORING_OP_READ_FIXED, 0, 0, 0, -EOPNOTSUPP },
		{ IORING_OP_NOP, IOSQE_IO_LINK, 0, 0, -EOPNOTSUPP },
		{ IORING_OP_NOP, IOSQE_BUFFER_SELECT, 0, 0, -EOPNOTSUPP },
		{ IORING_OP_NOP, IOSQE_IO_DRAIN, 0, 0, -EOPNOTSUPP },
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

	/* ETIME_SUCCESS：到期也以 0 完成。 */
	sqe = next_sqe(&ring);
	fill_timeout(sqe, &fifty_ms, 0, IORING_TIMEOUT_ETIME_SUCCESS, 11);
	CHECK(18, submit_one(&ring) == 1 && wait_cqe(&ring, &cqe) == 1 && cqe.res == 0);

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
	CHECK(33, wait_cqe(&ring, &cqe) == 1 && cqe.user_data == 40 && cqe.res == -ECANCELED);
	CHECK(34, wait_cqe(&ring, &cqe) == 1 && cqe.user_data == 41 && cqe.res == 0);
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
	CHECK(39, wait_cqe(&ring, &cqe) == 1 && cqe.user_data == 50 && cqe.res == -ECANCELED);
	CHECK(40, wait_cqe(&ring, &cqe) == 1 && cqe.user_data == 51 && cqe.res == 0);

	/* ASYNC_CANCEL 取消阻塞读：读 CQE 是 -ECANCELED，并且没有消费随后写入的数据。 */
	char buffer[16];
	sqe = next_sqe(&ring);
	fill_read(sqe, pipes[0], buffer, sizeof buffer, 60);
	CHECK(41, submit_one(&ring) == 1 && ready(&ring) == 0);
	sqe = next_sqe(&ring);
	fill_cancel(sqe, IORING_OP_ASYNC_CANCEL, 60, 0, 61);
	CHECK(42, submit_one(&ring) == 1);
	CHECK(43, wait_cqe(&ring, &cqe) == 1 && cqe.user_data == 60 && cqe.res == -ECANCELED);
	CHECK(44, wait_cqe(&ring, &cqe) == 1 && cqe.user_data == 61 && cqe.res == 0);
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

	/* ASYNC_CANCEL 的 prep：按 fd / opcode 匹配尚未实现。 */
	sqe = next_sqe(&ring);
	fill_cancel(sqe, IORING_OP_ASYNC_CANCEL, 0, IORING_ASYNC_CANCEL_FD, 65);
	CHECK(55, submit_one(&ring) == 1 && reap(&ring, &cqe) == 1 && cqe.res == -EOPNOTSUPP);
	sqe = next_sqe(&ring);
	fill_cancel(sqe, IORING_OP_ASYNC_CANCEL, 0, 0x80, 66);
	CHECK(56, submit_one(&ring) == 1 && reap(&ring, &cqe) == 1 && cqe.res == -EINVAL);
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
	return events != 0 ? 150 + events : 0;
}
