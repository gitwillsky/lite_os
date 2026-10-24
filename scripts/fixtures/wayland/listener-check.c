/* Linux SO_ACCEPTCONN 的真实 consumer；只注入一次性会话门禁镜像。 */
#include <sys/socket.h>
#include <sys/un.h>
#include <netinet/in.h>
#include <unistd.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static void require(int condition, const char *what) {
    if (!condition) { perror(what); exit(1); }
}
static void state(int fd, int expected) {
    int value = -1;
    socklen_t length = sizeof(value);
    require(getsockopt(fd, SOL_SOCKET, SO_ACCEPTCONN, &value, &length) == 0, "SO_ACCEPTCONN");
    require(length == sizeof(value) && value == expected, "listener state");
}
int main(void) {
    int udp = socket(AF_INET, SOCK_DGRAM, 0);
    require(udp >= 0, "udp"); state(udp, 0); close(udp);
    int tcp = socket(AF_INET, SOCK_STREAM, 0);
    require(tcp >= 0, "tcp"); state(tcp, 0);
    struct sockaddr_in address = { .sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK) };
    require(bind(tcp, (struct sockaddr *)&address, sizeof(address)) == 0, "tcp bind");
    require(listen(tcp, 2) == 0, "tcp listen"); state(tcp, 1);
    socklen_t length = sizeof(address);
    require(getsockname(tcp, (struct sockaddr *)&address, &length) == 0, "tcp address");
    int client = socket(AF_INET, SOCK_STREAM, 0);
    require(client >= 0 && connect(client, (struct sockaddr *)&address, sizeof(address)) == 0, "tcp connect");
    int child = accept(tcp, NULL, NULL);
    require(child >= 0, "tcp accept"); state(child, 0); state(tcp, 1);
    close(child); close(client); close(tcp);
    int pair[2]; require(socketpair(AF_UNIX, SOCK_STREAM, 0, pair) == 0, "unix pair");
    state(pair[0], 0); state(pair[1], 0); close(pair[0]); close(pair[1]);
    int listener = socket(AF_UNIX, SOCK_STREAM, 0);
    require(listener >= 0, "unix socket"); state(listener, 0);
    struct sockaddr_un unix_address = { .sun_family = AF_UNIX };
    strcpy(unix_address.sun_path, "/var/tmp/listener-check.sock");
    require(bind(listener, (struct sockaddr *)&unix_address, sizeof(unix_address)) == 0, "unix bind");
    require(listen(listener, 2) == 0, "unix listen"); state(listener, 1);
    close(listener); unlink(unix_address.sun_path);
    puts("LISTENER_OK");
    return 0;
}
