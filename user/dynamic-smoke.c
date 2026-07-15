#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <grp.h>
#include <arpa/inet.h>
#include <netdb.h>
#include <poll.h>
#include <signal.h>
#include <stddef.h>
#include <stdio.h>
#include <string.h>
#include <sys/random.h>
#include <sys/mman.h>
#include <sys/epoll.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

int verify_process_spawn(void);

/*
 * OWNER: verify_unix_epoll's single-threaded SIGPIPE probe; sig_atomic_t is
 * the only async-signal-safe observation shared with record_sigpipe.
 * FAILURE: an ordinary flag would make signal delivery/read undefined and
 * could falsely accept or reject MSG_NOSIGNAL/SIGPIPE behavior.
 */
static volatile sig_atomic_t observed_sigpipe;

static void record_sigpipe(int signal)
{
    if (signal == SIGPIPE) ++observed_sigpipe;
}

static int resolve_host(const char *host)
{
    struct addrinfo hints = { .ai_family = AF_INET, .ai_socktype = SOCK_STREAM };
    struct addrinfo *addresses = NULL;
    int error = getaddrinfo(host, NULL, &hints, &addresses);
    if (error != 0 || addresses == NULL) return 70;
    char address[INET_ADDRSTRLEN];
    struct sockaddr_in *resolved = (struct sockaddr_in *)addresses->ai_addr;
    if (inet_ntop(AF_INET, &resolved->sin_addr, address, sizeof(address)) == NULL) {
        freeaddrinfo(addresses);
        return 71;
    }
    printf("LITEOS_DNS_51 %s\n", address);
    freeaddrinfo(addresses);
    return 0;
}

static int verify_shared_mapping(void)
{
    static const char persisted[] = "LITEOS_SHARED_PERSIST_45\n";
    unlink("/shared-persist");
    int fd = open("/shared-persist", O_CREAT | O_EXCL | O_RDWR, 0644);
    if (fd < 0 || ftruncate(fd, 4096) != 0) return 50;
    char *shared = mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (shared == MAP_FAILED) return 51;
    memcpy(shared, persisted, sizeof(persisted) - 1);
    char observed[sizeof(persisted)] = { 0 };
    if (lseek(fd, 0, SEEK_SET) != 0
        || read(fd, observed, sizeof(persisted) - 1) != sizeof(persisted) - 1
        || memcmp(observed, persisted, sizeof(persisted) - 1) != 0) return 52;
    static const char direct[] = "direct-45";
    if (lseek(fd, 64, SEEK_SET) != 64 || write(fd, direct, sizeof(direct)) != sizeof(direct)
        || memcmp(shared + 64, direct, sizeof(direct)) != 0) return 53;
    pid_t child = fork();
    if (child == 0) {
        memcpy(shared + 128, "fork-45", 8);
        _exit(0);
    }
    int status;
    if (child <= 0 || waitpid(child, &status, 0) != child || !WIFEXITED(status)
        || WEXITSTATUS(status) != 0 || memcmp(shared + 128, "fork-45", 8) != 0
        || msync(shared, 4096, MS_SYNC) != 0 || munmap(shared, 8192) != 0) return 54;
    child = fork();
    if (child == 0) {
        char *eof = mmap(NULL, 8192, PROT_READ, MAP_SHARED, fd, 0);
        if (eof == MAP_FAILED) _exit(1);
        volatile char byte = eof[4096];
        (void)byte;
        _exit(2);
    }
    if (child <= 0 || waitpid(child, &status, 0) != child
        || !WIFSIGNALED(status) || WTERMSIG(status) != SIGBUS || close(fd) != 0) return 55;
    puts("LITEOS_SHARED_MMAP_45");
    return 0;
}

static int shared_crash_loop(void)
{
    int fd = open("/shared-crash", O_CREAT | O_RDWR, 0644);
    if (fd < 0 || ftruncate(fd, 4096) != 0) return 1;
    unsigned long *value = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (value == MAP_FAILED) return 2;
    puts("LITEOS_SHARED_CRASH_ACTIVE_45");
    for (;;) {
        ++*value;
        if (msync(value, 4096, MS_SYNC) != 0) return 3;
    }
}

static int verify_unix_epoll(void)
{
    int pair[2];
    if (socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, pair) != 0) return 60;
    int epoll = epoll_create1(EPOLL_CLOEXEC);
    struct epoll_event interest = { .events = EPOLLIN | EPOLLET | EPOLLONESHOT, .data.u64 = 46 };
    struct epoll_event event;
    if (epoll < 0 || epoll_ctl(epoll, EPOLL_CTL_ADD, pair[1], &interest) != 0
        || write(pair[0], "edge", 4) != 4 || epoll_wait(epoll, &event, 1, 1000) != 1
        || event.data.u64 != 46 || !(event.events & EPOLLIN)
        || epoll_wait(epoll, &event, 1, 0) != 0
        || epoll_ctl(epoll, EPOLL_CTL_MOD, pair[1], &interest) != 0
        || epoll_wait(epoll, &event, 1, 0) != 1) return 61;
    char bytes[8] = { 0 };
    if (read(pair[1], bytes, sizeof(bytes)) != 4 || memcmp(bytes, "edge", 4) != 0) return 62;
    int nonblocking[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, nonblocking) != 0
        || fcntl(nonblocking[1], F_SETFL, O_NONBLOCK) != 0
        || read(nonblocking[1], bytes, 1) != -1 || errno != EAGAIN) return 68;
    close(nonblocking[0]); close(nonblocking[1]);

    int datagram[2];
    if (socketpair(AF_UNIX, SOCK_DGRAM, 0, datagram) != 0
        || send(datagram[0], "packet", 6, 0) != 6
        || recv(datagram[1], bytes, sizeof(bytes), 0) != 6
        || memcmp(bytes, "packet", 6) != 0) return 63;

    char prefix[2] = { 0 };
    struct iovec vector = { .iov_base = prefix, .iov_len = sizeof(prefix) };
    struct msghdr message = { .msg_iov = &vector, .msg_iovlen = 1 };
    if (send(datagram[0], "abcdef", 6, 0) != 6
        || recvmsg(datagram[1], &message, MSG_TRUNC) != 6
        || memcmp(prefix, "ab", 2) != 0 || !(message.msg_flags & MSG_TRUNC)
        || send(datagram[0], "ghijkl", 6, 0) != 6
        || recvfrom(datagram[1], prefix, sizeof(prefix), MSG_TRUNC, NULL, NULL) != 6
        || memcmp(prefix, "gh", 2) != 0) return 70;

    static char large_datagram_tail[65536];
    struct iovec large_datagram_vectors[] = {
        { .iov_base = prefix, .iov_len = sizeof(prefix) },
        { .iov_base = large_datagram_tail, .iov_len = sizeof(large_datagram_tail) },
    };
    message.msg_iov = large_datagram_vectors;
    message.msg_iovlen = 2;
    message.msg_flags = 0;
    if (send(datagram[0], "mnopqr", 6, 0) != 6
        || recvmsg(datagram[1], &message, MSG_TRUNC) != 6
        || memcmp(prefix, "mn", 2) != 0 || memcmp(large_datagram_tail, "opqr", 4) != 0
        || (message.msg_flags & MSG_TRUNC)) return 83;

    static char oversized[65536];
    struct iovec oversized_vector = { .iov_base = oversized, .iov_len = sizeof(oversized) };
    struct msghdr oversized_message = { .msg_iov = &oversized_vector, .msg_iovlen = 1 };
    errno = 0;
    if (write(datagram[0], oversized, sizeof(oversized)) != -1 || errno != EMSGSIZE) return 76;
    errno = 0;
    if (sendto(datagram[0], oversized, sizeof(oversized), 0, NULL, 0) != -1
        || errno != EMSGSIZE) return 77;
    errno = 0;
    if (sendmsg(datagram[0], &oversized_message, 0) != -1 || errno != EMSGSIZE) return 78;
    struct pollfd oversized_readable = { .fd = datagram[1], .events = POLLIN };
    if (poll(&oversized_readable, 1, 0) != 0) return 90;

    if (fcntl(datagram[0], F_SETFL, O_NONBLOCK) != 0) return 71;
    int queued = 0;
    while (queued < 64 && send(datagram[0], "q", 1, 0) == 1) ++queued;
    if (queued == 0 || queued == 64 || errno != EAGAIN) return 72;
    struct pollfd writable = { .fd = datagram[0], .events = POLLOUT };
    interest.events = EPOLLOUT;
    interest.data.u64 = 49;
    if (poll(&writable, 1, 0) != 0 || (writable.revents & POLLOUT)
        || epoll_ctl(epoll, EPOLL_CTL_ADD, datagram[0], &interest) != 0
        || epoll_wait(epoll, &event, 1, 0) != 0
        || recv(datagram[1], bytes, 1, 0) != 1
        || epoll_wait(epoll, &event, 1, 1000) != 1
        || event.data.u64 != 49 || !(event.events & EPOLLOUT)
        || send(datagram[0], "r", 1, 0) != 1
        || epoll_ctl(epoll, EPOLL_CTL_DEL, datagram[0], NULL) != 0) return 73;

    int ready_pipe[2], done_pipe[2];
    if (fcntl(datagram[0], F_SETFL, 0) != 0
        || pipe(ready_pipe) != 0 || pipe(done_pipe) != 0) return 74;
    pid_t blocked = fork();
    if (blocked == 0) {
        close(ready_pipe[0]); close(done_pipe[0]);
        if (write(ready_pipe[1], "s", 1) != 1
            || send(datagram[0], "b", 1, 0) != 1
            || write(done_pipe[1], "d", 1) != 1) _exit(1);
        _exit(0);
    }
    close(ready_pipe[1]); close(done_pipe[1]);
    struct pollfd done = { .fd = done_pipe[0], .events = POLLIN };
    int blocked_status;
    if (blocked <= 0 || read(ready_pipe[0], bytes, 1) != 1
        || poll(&done, 1, 20) != 0 || recv(datagram[1], bytes, 1, 0) != 1
        || poll(&done, 1, 1000) != 1 || read(done_pipe[0], bytes, 1) != 1
        || waitpid(blocked, &blocked_status, 0) != blocked
        || !WIFEXITED(blocked_status) || WEXITSTATUS(blocked_status) != 0) {
        if (blocked > 0) { kill(blocked, SIGKILL); waitpid(blocked, NULL, 0); }
        return 75;
    }
    close(ready_pipe[0]); close(done_pipe[0]);

    int reconnect_sender = socket(AF_UNIX, SOCK_DGRAM, 0);
    int reconnect_full = socket(AF_UNIX, SOCK_DGRAM, 0);
    int reconnect_empty = socket(AF_UNIX, SOCK_DGRAM, 0);
    static const char full_name[] = "liteos-dgram-full";
    static const char empty_name[] = "liteos-dgram-empty";
    struct sockaddr_un full_address = { .sun_family = AF_UNIX };
    struct sockaddr_un empty_address = { .sun_family = AF_UNIX };
    memcpy(full_address.sun_path + 1, full_name, sizeof(full_name) - 1);
    memcpy(empty_address.sun_path + 1, empty_name, sizeof(empty_name) - 1);
    socklen_t full_length = offsetof(struct sockaddr_un, sun_path) + sizeof(full_name);
    socklen_t empty_length = offsetof(struct sockaddr_un, sun_path) + sizeof(empty_name);
    if (reconnect_sender < 0 || reconnect_full < 0 || reconnect_empty < 0
        || bind(reconnect_full, (struct sockaddr *)&full_address, full_length) != 0
        || bind(reconnect_empty, (struct sockaddr *)&empty_address, empty_length) != 0
        || connect(reconnect_sender, (struct sockaddr *)&full_address, full_length) != 0
        || fcntl(reconnect_sender, F_SETFL, O_NONBLOCK) != 0) return 79;
    queued = 0;
    while (queued < 64 && send(reconnect_sender, "f", 1, 0) == 1) ++queued;
    interest.events = EPOLLOUT;
    interest.data.u64 = 50;
    if (queued == 0 || queued == 64 || errno != EAGAIN
        || epoll_ctl(epoll, EPOLL_CTL_ADD, reconnect_sender, &interest) != 0
        || epoll_wait(epoll, &event, 1, 0) != 0
        || pipe(ready_pipe) != 0 || pipe(done_pipe) != 0) return 80;
    pid_t reconnect_waiter = fork();
    if (reconnect_waiter == 0) {
        close(ready_pipe[0]); close(done_pipe[0]);
        if (write(ready_pipe[1], "s", 1) != 1
            || epoll_wait(epoll, &event, 1, 1000) != 1
            || event.data.u64 != 50 || !(event.events & EPOLLOUT)
            || write(done_pipe[1], "d", 1) != 1) _exit(1);
        _exit(0);
    }
    close(ready_pipe[1]); close(done_pipe[1]);
    done.fd = done_pipe[0];
    done.events = POLLIN;
    if (reconnect_waiter <= 0 || read(ready_pipe[0], bytes, 1) != 1
        || poll(&done, 1, 20) != 0
        || connect(reconnect_sender, (struct sockaddr *)&empty_address, empty_length) != 0
        || poll(&done, 1, 1000) != 1 || read(done_pipe[0], bytes, 1) != 1
        || waitpid(reconnect_waiter, &blocked_status, 0) != reconnect_waiter
        || !WIFEXITED(blocked_status) || WEXITSTATUS(blocked_status) != 0) {
        if (reconnect_waiter > 0) {
            kill(reconnect_waiter, SIGKILL);
            waitpid(reconnect_waiter, NULL, 0);
        }
        return 81;
    }
    if (epoll_ctl(epoll, EPOLL_CTL_DEL, reconnect_sender, NULL) != 0) return 82;
    close(ready_pipe[0]); close(done_pipe[0]);
    close(reconnect_sender); close(reconnect_full); close(reconnect_empty);

    int message_stream[2];
    static char large_stream_capacity[65536];
    char stream_prefix[8] = { 0 };
    struct iovec stream_receive_vectors[] = {
        { .iov_base = stream_prefix, .iov_len = sizeof(stream_prefix) },
        { .iov_base = large_stream_capacity, .iov_len = sizeof(large_stream_capacity) },
    };
    struct msghdr stream_message = {
        .msg_iov = stream_receive_vectors,
        .msg_iovlen = 2,
    };
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, message_stream) != 0
        || send(message_stream[0], "wide", 4, 0) != 4
        || recvmsg(message_stream[1], &stream_message, 0) != 4
        || memcmp(stream_prefix, "wide", 4) != 0) return 84;

    char fault_prefix[] = "drop";
    struct iovec fault_vectors[] = {
        { .iov_base = fault_prefix, .iov_len = sizeof(fault_prefix) - 1 },
        { .iov_base = (void *)1, .iov_len = 1 },
    };
    struct msghdr fault_message = { .msg_iov = fault_vectors, .msg_iovlen = 2 };
    errno = 0;
    struct pollfd fault_readable = { .fd = message_stream[1], .events = POLLIN };
    if (sendmsg(message_stream[0], &fault_message, 0) != -1 || errno != EFAULT
        || poll(&fault_readable, 1, 0) != 0) return 87;

    static char stream_payload[65537];
    static char stream_received[65537];
    for (size_t index = 0; index < sizeof(stream_payload); ++index)
        stream_payload[index] = (char)(index * 17u + 3u);
    struct iovec stream_send_vectors[] = {
        { .iov_base = stream_payload, .iov_len = 32768 },
        { .iov_base = stream_payload + 32768, .iov_len = sizeof(stream_payload) - 32768 },
    };
    struct msghdr stream_send_message = {
        .msg_iov = stream_send_vectors,
        .msg_iovlen = 2,
    };
    if (fcntl(message_stream[0], F_SETFL, O_NONBLOCK) != 0) return 85;
    ssize_t stream_sent = sendmsg(message_stream[0], &stream_send_message, 0);
    struct pollfd stream_readable = { .fd = message_stream[1], .events = POLLIN };
    if (stream_sent <= 0 || (size_t)stream_sent > sizeof(stream_payload)
        || poll(&stream_readable, 1, 1000) != 1 || !(stream_readable.revents & POLLIN)
        || recv(message_stream[1], stream_received, (size_t)stream_sent, 0) != stream_sent
        || memcmp(stream_received, stream_payload, (size_t)stream_sent) != 0) return 86;
    close(message_stream[0]); close(message_stream[1]);

    int broken_stream[2];
    struct sigaction pipe_action = { .sa_handler = record_sigpipe };
    struct sigaction previous_pipe_action;
    sigemptyset(&pipe_action.sa_mask);
    if (sigaction(SIGPIPE, &pipe_action, &previous_pipe_action) != 0
        || socketpair(AF_UNIX, SOCK_STREAM, 0, broken_stream) != 0
        || close(broken_stream[1]) != 0) return 87;
    observed_sigpipe = 0;
    errno = 0;
    if (send(broken_stream[0], "x", 1, MSG_NOSIGNAL) != -1 || errno != EPIPE
        || observed_sigpipe != 0) return 88;
    errno = 0;
    if (send(broken_stream[0], "x", 1, 0) != -1 || errno != EPIPE
        || observed_sigpipe != 1 || sigaction(SIGPIPE, &previous_pipe_action, NULL) != 0)
        return 89;
    close(broken_stream[0]);

    int listener = socket(AF_UNIX, SOCK_STREAM, 0);
    struct sockaddr_un address = { .sun_family = AF_UNIX };
    memcpy(address.sun_path + 1, "liteos-phase-46", 15);
    socklen_t address_length = offsetof(struct sockaddr_un, sun_path) + 16;
    if (listener < 0 || bind(listener, (struct sockaddr *)&address, address_length) != 0
        || listen(listener, 4) != 0) return 64;
    pid_t child = fork();
    if (child == 0) {
        int client = socket(AF_UNIX, SOCK_STREAM, 0);
        if (client < 0 || connect(client, (struct sockaddr *)&address, address_length) != 0
            || write(client, "request", 7) != 7
            || read(client, bytes, sizeof(bytes)) != 5
            || memcmp(bytes, "reply", 5) != 0) _exit(1);
        _exit(0);
    }
    interest.events = EPOLLIN;
    interest.data.u64 = 47;
    if (epoll_ctl(epoll, EPOLL_CTL_ADD, listener, &interest) != 0
        || epoll_wait(epoll, &event, 1, 1000) != 1 || event.data.u64 != 47) return 65;
    int accepted = accept4(listener, NULL, NULL, SOCK_CLOEXEC);
    if (accepted < 0 || epoll_ctl(epoll, EPOLL_CTL_ADD, accepted, &interest) != 0
        || epoll_wait(epoll, &event, 1, 1000) != 1
        || read(accepted, bytes, sizeof(bytes)) != 7 || memcmp(bytes, "request", 7) != 0
        || write(accepted, "reply", 5) != 5) return 66;
    int status;
    if (waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status) != 0) return 67;
    close(accepted); close(listener); close(datagram[0]); close(datagram[1]);
    close(pair[0]); close(pair[1]);
    int stale[2], replacement[2];
    interest.data.u64 = 48;
    if (pipe(stale) != 0 || epoll_ctl(epoll, EPOLL_CTL_ADD, stale[0], &interest) != 0
        || close(stale[0]) != 0 || pipe(replacement) != 0
        || write(replacement[1], "x", 1) != 1 || epoll_wait(epoll, &event, 1, 0) != 0) return 69;
    close(stale[1]); close(replacement[0]); close(replacement[1]); close(epoll);
    puts("LITEOS_UNIX_EPOLL_46");
    return 0;
}

static int verify_credentials(const char *program)
{
    uid_t ruid, euid, suid;
    gid_t rgid, egid, sgid;
    gid_t groups[] = { 1000, 2000 };
    unlink("/credential-owner");
    unlink("/credential-root");
    if (getresuid(&ruid, &euid, &suid) != 0 || ruid != 0 || euid != 0 || suid != 0
        || getresgid(&rgid, &egid, &sgid) != 0 || rgid != 0 || egid != 0 || sgid != 0
        || setgroups(2, groups) != 0 || getgroups(0, NULL) != 2
        || umask(0027) != 0022) return 10;
    int owner = open("/credential-owner", O_CREAT | O_EXCL | O_RDWR, 0666);
    int root = open("/credential-root", O_CREAT | O_EXCL | O_RDWR, 0600);
    struct stat metadata;
    if (owner < 0 || root < 0 || fstat(owner, &metadata) != 0
        || (metadata.st_mode & 0777) != 0640 || fchmodat(AT_FDCWD, "/credential-owner", 0660, 0) != 0
        || fchownat(AT_FDCWD, "/credential-owner", 1000, 1000, 0) != 0
        || chmod(program, 04755) != 0 || close(owner) != 0 || close(root) != 0) return 11;
    pid_t child = fork();
    if (child == 0) {
        if (setresgid(1000, 1000, 1000) != 0 || setresuid(1000, 1000, 1000) != 0) _exit(21);
        if (getresuid(&ruid, &euid, &suid) != 0 || ruid != 1000 || euid != 1000 || suid != 1000) _exit(26);
        if (stat("/credential-root", &metadata) != 0 || metadata.st_uid != 0 || (metadata.st_mode & 0777) != 0600) _exit(27);
        if (open("/credential-owner", O_RDWR) < 0) _exit(22);
        errno = 0;
        if (open("/credential-root", O_RDONLY) != -1 || errno != EACCES) _exit(23);
        errno = 0;
        if (kill(getppid(), 0) != -1 || errno != EPERM) _exit(24);
        execl(program, program, "setid-probe", (char *)0);
        _exit(25);
    }
    int status;
    if (child <= 0 || waitpid(child, &status, 0) != child || !WIFEXITED(status)) return 12;
    if (WEXITSTATUS(status) != 0) return 40 + WEXITSTATUS(status);
    puts("LITEOS_CREDENTIALS_44");
    return 0;
}

int main(int argc, char **argv)
{
    if (argc == 2 && strcmp(argv[1], "shared-crash") == 0) return shared_crash_loop();
    if (argc == 2 && strcmp(argv[1], "spawn") == 0) return verify_process_spawn();
    if (argc == 3 && strcmp(argv[1], "resolve") == 0) return resolve_host(argv[2]);
    if (argc == 2 && strcmp(argv[1], "setid-probe") == 0) {
        uid_t real, effective, saved;
        return getresuid(&real, &effective, &saved) != 0 || real != 1000 || effective != 0 || saved != 0;
    }
    static const char payload[] = "link-abi";
    struct stat source_stat;
    struct stat hard_stat;
    struct stat symlink_stat;
    char link_target[32];
    unlink("/abi-source");
    unlink("/abi-hard");
    unlink("/abi-empty");
    unlink("/abi-symlink");
    unlink("/abi-symlink-hard");
    unlink("/abi-follow-hard");
    int source = open("/abi-source", O_CREAT | O_EXCL | O_RDWR, 0644);
    if (source < 0
        || write(source, payload, sizeof(payload)) != sizeof(payload)
        || linkat(AT_FDCWD, "/abi-source", AT_FDCWD, "/abi-hard", 0) != 0
        || linkat(source, "", AT_FDCWD, "/abi-empty", AT_EMPTY_PATH) != 0
        || symlinkat("abi-source", AT_FDCWD, "/abi-symlink") != 0
        || linkat(AT_FDCWD, "/abi-symlink", AT_FDCWD, "/abi-symlink-hard", 0) != 0
        || linkat(AT_FDCWD, "/abi-symlink", AT_FDCWD, "/abi-follow-hard", AT_SYMLINK_FOLLOW) != 0
        || fstat(source, &source_stat) != 0
        || stat("/abi-hard", &hard_stat) != 0
        || lstat("/abi-symlink-hard", &symlink_stat) != 0
        || source_stat.st_ino != hard_stat.st_ino
        || source_stat.st_nlink != 4
        || !S_ISLNK(symlink_stat.st_mode)
        || symlink_stat.st_nlink != 2
        || readlink("/abi-symlink-hard", link_target, sizeof(link_target)) != 10
        || memcmp(link_target, "abi-source", 10) != 0
        || close(source) != 0) {
        return 6;
    }
    struct timeval direct_time;
    struct timespec realtime;
    int timezone[2] = { -1, -1 };
    if (syscall(SYS_gettimeofday, &direct_time, timezone) != 0
        || clock_gettime(CLOCK_REALTIME, &realtime) != 0
        || direct_time.tv_usec < 0 || direct_time.tv_usec >= 1000000
        || realtime.tv_sec < direct_time.tv_sec
        || realtime.tv_sec > direct_time.tv_sec + 1
        || timezone[0] != 0 || timezone[1] != 0) {
        return 5;
    }
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
    int credential_result = verify_credentials(argv[0]);
    if (credential_result != 0) return credential_result;
    int shared_result = verify_shared_mapping();
    if (shared_result != 0) return shared_result;
    int socket_result = verify_unix_epoll();
    if (socket_result != 0) return socket_result;
    puts("LITEOS_DLOPEN_42");
    return 0;
}
