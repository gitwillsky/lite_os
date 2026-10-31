/* 管理员进程只拥有 USB Audio 2 输出设备的捕获租约；数据转发使用上游 usbredirect。 */
#include <libusb.h>
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <spawn.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

extern char **environ;

struct identity {
    uint16_t vendor, product;
    uint8_t bus, ports[8];
    int depth;
};

struct binding {
    struct identity device;
    pid_t child;
    unsigned port;
    bool present;
};

/* signal handler 不能执行 libusb 或 waitpid；只有主线程拥有设备租约和子进程清理。 */
static volatile sig_atomic_t stopping;

static void interrupt_handler(int signo)
{
    (void)signo;
    stopping = 1;
}

static bool same_device(const struct identity *a, const struct identity *b)
{
    return a->vendor == b->vendor && a->product == b->product &&
           a->bus == b->bus && a->depth == b->depth &&
           memcmp(a->ports, b->ports, (size_t)a->depth) == 0;
}

static bool audio_output(libusb_device *device)
{
    struct libusb_config_descriptor *config = NULL;
    bool found = false;
    if (libusb_get_active_config_descriptor(device, &config) != 0) {
        return false;
    }
    for (unsigned i = 0; i < config->bNumInterfaces && !found; ++i) {
        const struct libusb_interface *interface = &config->interface[i];
        for (int j = 0; j < interface->num_altsetting && !found; ++j) {
            const struct libusb_interface_descriptor *alt = &interface->altsetting[j];
            if (alt->bInterfaceClass != LIBUSB_CLASS_AUDIO ||
                alt->bInterfaceSubClass != 2 || alt->bInterfaceProtocol != 0x20) {
                continue;
            }
            for (unsigned k = 0; k < alt->bNumEndpoints; ++k) {
                const struct libusb_endpoint_descriptor *ep = &alt->endpoint[k];
                if ((ep->bEndpointAddress & LIBUSB_ENDPOINT_DIR_MASK) == LIBUSB_ENDPOINT_OUT &&
                    (ep->bmAttributes & LIBUSB_TRANSFER_TYPE_MASK) == LIBUSB_TRANSFER_TYPE_ISOCHRONOUS) {
                    found = true;
                    break;
                }
            }
        }
    }
    libusb_free_config_descriptor(config);
    return found;
}

static void release_binding(struct binding *binding)
{
    if (binding->child <= 0) {
        return;
    }
    kill(binding->child, SIGTERM);
    /* usbredirect 的 GLib loop 会取消传输并关闭设备；最多给它 3 秒完成释放。 */
    for (unsigned i = 0; i < 30; ++i) {
        pid_t result = waitpid(binding->child, NULL, WNOHANG);
        if (result == binding->child || (result < 0 && errno == ECHILD)) {
            binding->child = 0;
            return;
        }
        struct timespec delay = {.tv_nsec = 100000000};
        nanosleep(&delay, NULL);
    }
    kill(binding->child, SIGKILL);
    while (waitpid(binding->child, NULL, 0) < 0 && errno == EINTR) {
    }
    binding->child = 0;
}

static int redirect_device(struct binding *binding, libusb_device *device,
                           const struct identity *identity, const char *redirector)
{
    char address[16], endpoint[32];
    snprintf(address, sizeof(address), "%u-%u", identity->bus,
             libusb_get_device_address(device));
    snprintf(endpoint, sizeof(endpoint), "127.0.0.1:%u", binding->port);
    char *arguments[] = {(char *)redirector, "--device", address, "--to", endpoint,
                         "--verbose", "2", NULL};
    posix_spawn_file_actions_t actions;
    int error = posix_spawn_file_actions_init(&actions);
    if (error != 0) {
        return error;
    }
    /* 只有 supervisor 读取父进程的控制管道；转发子进程不继承这个会话存活凭据。 */
    error = posix_spawn_file_actions_addopen(&actions, STDIN_FILENO, "/dev/null", O_RDONLY, 0);
    if (error == 0) {
        error = posix_spawn(&binding->child, redirector, &actions, NULL, arguments, environ);
    }
    posix_spawn_file_actions_destroy(&actions);
    if (error == 0) {
        binding->device = *identity;
        binding->present = true;
        fprintf(stderr, "USB DAC %04x:%04x -> local usbredir port %u\n",
                identity->vendor, identity->product, binding->port);
    }
    return error;
}

static int scan_devices(struct binding *bindings, size_t count, const char *redirector)
{
    libusb_device **devices = NULL;
    ssize_t length = libusb_get_device_list(NULL, &devices);
    if (length < 0) {
        return (int)length;
    }
    for (size_t i = 0; i < count; ++i) {
        bindings[i].present = false;
    }
    int error = 0;
    for (ssize_t i = 0; i < length && error == 0; ++i) {
        struct libusb_device_descriptor descriptor;
        if (libusb_get_device_descriptor(devices[i], &descriptor) != 0) {
            continue;
        }
        struct identity identity = {.vendor = descriptor.idVendor,
                                    .product = descriptor.idProduct,
                                    .bus = libusb_get_bus_number(devices[i])};
        identity.depth = libusb_get_port_numbers(devices[i], identity.ports, sizeof(identity.ports));
        if (identity.depth <= 0) {
            continue;
        }
        bool bound = false;
        for (size_t j = 0; j < count; ++j) {
            if (bindings[j].child > 0 && same_device(&bindings[j].device, &identity)) {
                bindings[j].present = true;
                bound = true;
                break;
            }
        }
        if (bound || !audio_output(devices[i])) {
            continue;
        }
        for (size_t j = 0; j < count; ++j) {
            if (bindings[j].child == 0) {
                error = redirect_device(&bindings[j], devices[i], &identity, redirector);
                break;
            }
        }
    }
    libusb_free_device_list(devices, 1);
    for (size_t i = 0; i < count && error == 0; ++i) {
        if (bindings[i].child > 0 && !bindings[i].present) {
            release_binding(&bindings[i]);
        } else if (bindings[i].child > 0) {
            int status;
            pid_t result = waitpid(bindings[i].child, &status, WNOHANG);
            if (result == bindings[i].child) {
                bindings[i].child = 0;
                if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
                    fprintf(stderr, "USB redirector failed; stopping this USB session\n");
                    error = -1;
                } else {
                    /* 标准 usbredir 连接的对端结束，不再向已关闭的 QEMU 端口重启子进程。 */
                    stopping = 1;
                }
            }
        }
    }
    return error;
}

int main(int argc, char **argv)
{
    if (argc < 3 || geteuid() != 0 || argv[1][0] != '/') {
        fprintf(stderr, "usage: sudo usb-dac-bridge /absolute/usbredirect port...\n");
        return 2;
    }
    size_t count = (size_t)argc - 2;
    struct binding *bindings = calloc(count, sizeof(*bindings));
    if (bindings == NULL) {
        return 1;
    }
    for (size_t i = 0; i < count; ++i) {
        char *end;
        unsigned long port = strtoul(argv[i + 2], &end, 10);
        if (*end != '\0' || port == 0 || port > 65535) {
            free(bindings);
            return 2;
        }
        bindings[i].port = (unsigned)port;
    }
    struct sigaction action = {.sa_handler = interrupt_handler};
    sigemptyset(&action.sa_mask);
    sigaction(SIGINT, &action, NULL);
    sigaction(SIGTERM, &action, NULL);
    sigaction(SIGHUP, &action, NULL);
    if (libusb_init(NULL) != 0) {
        free(bindings);
        return 1;
    }
    int error = 0;
    fprintf(stderr, "USB DAC session ready; waiting for Audio 2 output devices\n");
    struct pollfd parent = {.fd = STDIN_FILENO, .events = POLLIN};
    /* 1. workflow 是控制管道唯一 writer；正常退出、异常或被杀都会产生 EOF。
     * 2. 数据只在 usbredirect 与 QEMU 的标准 usbredir socket 间流动。
     * 3. parent EOF/信号由本线程统一取消子进程，避免留下独占 DAC 的 root 进程。
     */
    while (!stopping) {
        int ready = poll(&parent, 1, 1000);
        if (ready > 0) {
            break;
        }
        if (ready < 0 && errno != EINTR) {
            error = -1;
            break;
        }
        if (stopping) {
            break;
        }
        error = scan_devices(bindings, count, argv[1]);
        if (error != 0) {
            fprintf(stderr, "USB DAC discovery/forwarding failed: %d\n", error);
            break;
        }
    }
    for (size_t i = 0; i < count; ++i) {
        release_binding(&bindings[i]);
    }
    libusb_exit(NULL);
    free(bindings);
    return error != 0;
}
