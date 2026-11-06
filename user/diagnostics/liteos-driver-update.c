// Guest-side driver deployment client. Transfer an already-open image over AF_UNIX.
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

static int send_image(int socket_fd, int image, uint64_t address)
{
    uint8_t request[16] = "LDRVUPD1";
    for (unsigned byte = 0; byte < 8; byte++)
        request[8 + byte] = (uint8_t)(address >> (8 * byte));

    struct sockaddr_un target = { .sun_family = AF_UNIX, .sun_path = "/dev/driver-update" };
    struct iovec vector = { .iov_base = request, .iov_len = sizeof(request) };
    union {
        struct cmsghdr alignment;
        char bytes[CMSG_SPACE(sizeof(int))];
    } control = { 0 };
    struct msghdr message = {
        .msg_name = &target,
        .msg_namelen = offsetof(struct sockaddr_un, sun_path) + sizeof("/dev/driver-update"),
        .msg_iov = &vector,
        .msg_iovlen = 1,
        .msg_control = control.bytes,
        .msg_controllen = sizeof(control.bytes),
    };
    struct cmsghdr *header = CMSG_FIRSTHDR(&message);
    header->cmsg_level = SOL_SOCKET;
    header->cmsg_type = SCM_RIGHTS;
    header->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(header), &image, sizeof(image));
    return sendmsg(socket_fd, &message, 0) == (ssize_t)sizeof(request) ? 0 : -1;
}

static int32_t status_at(const uint8_t *bytes)
{
    uint32_t value = 0;
    for (unsigned byte = 0; byte < 4; byte++)
        value |= (uint32_t)bytes[byte] << (8 * byte);
    return (int32_t)value;
}

int main(int argc, char **argv)
{
    if (argc != 3) {
        fprintf(stderr, "usage: %s manager|0xNODE_ADDRESS IMAGE\n", argv[0]);
        return 2;
    }
    uint64_t address = 0;
    if (strcmp(argv[1], "manager") != 0) {
        char *end;
        errno = 0;
        address = strtoull(argv[1], &end, 0);
        if (errno || *end || address == 0 || argv[1][0] == '-') {
            fputs("invalid node address\n", stderr);
            return 2;
        }
    }

    int image = open(argv[2], O_RDONLY | O_CLOEXEC);
    if (image < 0) {
        perror("open image");
        return 1;
    }
    int connection = socket(AF_UNIX, SOCK_DGRAM | SOCK_CLOEXEC, 0);
    if (connection < 0) {
        perror("socket");
        return 1;
    }
    // An abstract address avoids creating a filesystem node for the reply.
    struct sockaddr_un source = { .sun_family = AF_UNIX };
    int length = snprintf(source.sun_path + 1, sizeof(source.sun_path) - 1,
                          "liteos-driver-update-%ld", (long)getpid());
    if (bind(connection, (struct sockaddr *)&source,
             offsetof(struct sockaddr_un, sun_path) + 1 + (size_t)length) < 0 ||
        send_image(connection, image, address) < 0) {
        perror("submit driver update");
        return 1;
    }
    close(image);
    uint8_t reply[8];
    ssize_t received = recv(connection, reply, sizeof(reply), MSG_TRUNC);
    if (received != (ssize_t)sizeof(reply)) {
        fputs("driver update result unavailable\n", stderr);
        return 1;
    }
    int32_t update = status_at(reply);
    int32_t rollback = status_at(reply + 4);
    if (update || rollback) {
        fprintf(stderr, "update status %" PRId32 ", rollback status %" PRId32 "\n",
                update, rollback);
        return 1;
    }
    puts("driver update complete");
    return 0;
}
