#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <time.h>
#include <unistd.h>

#define SNAPSHOT_BYTES 160
#define POINTER_LATENCY_LIMIT_MS 20

struct snapshot {
    uint64_t frames_completed;
    uint64_t pointer_samples;
    uint64_t last_pointer_latency_ms;
    uint64_t last_pointer_pixels;
    uint64_t resize_commits;
    uint32_t width;
    uint32_t height;
    uint32_t flags;
    uint32_t client_mask;
};

static uint32_t read_u32(const unsigned char *bytes, size_t offset) {
    return (uint32_t)bytes[offset] | (uint32_t)bytes[offset + 1] << 8 |
           (uint32_t)bytes[offset + 2] << 16 |
           (uint32_t)bytes[offset + 3] << 24;
}

static uint64_t read_u64(const unsigned char *bytes, size_t offset) {
    return (uint64_t)read_u32(bytes, offset) |
           (uint64_t)read_u32(bytes, offset + 4) << 32;
}

static int read_exact(int fd, unsigned char *bytes, size_t length) {
    size_t offset = 0;
    while (offset < length) {
        ssize_t count = read(fd, bytes + offset, length - offset);
        if (count > 0) {
            offset += (size_t)count;
        } else if (count < 0 && errno == EINTR) {
            continue;
        } else {
            return -1;
        }
    }
    return 0;
}

static int query(struct snapshot *result) {
    int fd = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    if (fd < 0) {
        return -1;
    }
    struct sockaddr_un address = { .sun_family = AF_UNIX };
    const char path[] = "/run/liteui/compositor.sock";
    memcpy(address.sun_path, path, sizeof(path));
    if (connect(fd, (const struct sockaddr *)&address,
                (socklen_t)(offsetof(struct sockaddr_un, sun_path) + sizeof(path))) != 0) {
        close(fd);
        return -1;
    }
    unsigned char bytes[SNAPSHOT_BYTES];
    int status = read_exact(fd, bytes, sizeof(bytes));
    close(fd);
    if (status != 0 || memcmp(bytes, "LUD1", 4) != 0 ||
        bytes[4] != 1 || bytes[5] != 0 ||
        bytes[6] != SNAPSHOT_BYTES || bytes[7] != 0) {
        return -1;
    }
    result->pointer_samples = read_u64(bytes, 48);
    result->frames_completed = read_u64(bytes, 32);
    result->last_pointer_latency_ms = read_u64(bytes, 64);
    result->last_pointer_pixels = read_u64(bytes, 80);
    result->resize_commits = read_u64(bytes, 120);
    result->width = read_u32(bytes, 144);
    result->height = read_u32(bytes, 148);
    result->flags = read_u32(bytes, 152);
    result->client_mask = read_u32(bytes, 156);
    return 0;
}

static void pause_milliseconds(long milliseconds) {
    struct timespec delay = {
        .tv_sec = milliseconds / 1000,
        .tv_nsec = milliseconds % 1000 * 1000000,
    };
    while (nanosleep(&delay, &delay) != 0 && errno == EINTR) {
    }
}

static int parse_u64(const char *text, uint64_t *value) {
    char *end = NULL;
    errno = 0;
    unsigned long long parsed = strtoull(text, &end, 10);
    if (errno != 0 || end == text || *end != '\0') {
        return -1;
    }
    *value = (uint64_t)parsed;
    return 0;
}

static int wait_ready(void) {
    struct snapshot current = {0};
    int queried = -1;
    uint64_t stable_frame = UINT64_MAX;
    unsigned int stable_samples = 0;
    for (unsigned int attempt = 0; attempt < 300; ++attempt) {
        queried = query(&current);
        if (queried == 0 && current.client_mask == 7 &&
            current.width != 0 && current.height != 0 && current.flags == 0) {
            if (stable_frame == current.frames_completed) {
                stable_samples += 1;
                if (stable_samples == 50) return 0;
            } else {
                stable_frame = current.frames_completed;
                stable_samples = 0;
            }
        } else {
            stable_frame = UINT64_MAX;
            stable_samples = 0;
        }
        pause_milliseconds(10);
    }
    fprintf(stderr,
            "liteui-inspect: desktop not ready: query=%d mask=%u size=%ux%u flags=%u\n",
            queried, current.client_mask, current.width, current.height, current.flags);
    return -1;
}

static int wait_pointer(uint64_t previous) {
    struct snapshot current = {0};
    int queried = -1;
    for (unsigned int attempt = 0; attempt < 300; ++attempt) {
        queried = query(&current);
        if (queried == 0 && current.pointer_samples > previous &&
            current.last_pointer_latency_ms <= POINTER_LATENCY_LIMIT_MS &&
            current.last_pointer_pixels != 0 &&
            current.last_pointer_pixels <
                (uint64_t)current.width * current.height / 8 &&
            (current.flags & 7) == 0) {
            return 0;
        }
        pause_milliseconds(10);
    }
    fprintf(stderr,
            "liteui-inspect: pointer stalled: query=%d samples=%llu previous=%llu "
            "latency=%llums pixels=%llu size=%ux%u flags=%u\n",
            queried, (unsigned long long)current.pointer_samples,
            (unsigned long long)previous,
            (unsigned long long)current.last_pointer_latency_ms,
            (unsigned long long)current.last_pointer_pixels,
            current.width, current.height, current.flags);
    return -1;
}

static int wait_resize(uint64_t previous, uint32_t width, uint32_t height) {
    for (unsigned int attempt = 0; attempt < 500; ++attempt) {
        struct snapshot current;
        if (query(&current) == 0 && current.resize_commits == previous + 1 &&
            current.width == width && current.height == height && current.flags == 0) {
            return 0;
        }
        pause_milliseconds(10);
    }
    return -1;
}

static int wait_frame(uint64_t previous) {
    for (unsigned int attempt = 0; attempt < 500; ++attempt) {
        struct snapshot current;
        if (query(&current) == 0 && current.frames_completed > previous &&
            current.flags == 0) {
            return 0;
        }
        pause_milliseconds(10);
    }
    return -1;
}

struct role {
    const char *name;
    const char *program;
    unsigned int uid;
};

static const struct role roles[] = {
    { "session", "/bin/liteui-session", 0 },
    { "broker", "/bin/display-session", 0 },
    { "compositor", "/bin/liteui-compositor", 0 },
    { "shell", "/bin/liteui-host", 100 },
    { "terminal", "/bin/terminal-service", 101 },
    { "application", "/bin/liteui-host", 102 },
};

static const struct role *find_role(const char *name) {
    for (size_t index = 0; index < sizeof(roles) / sizeof(roles[0]); ++index) {
        if (strcmp(roles[index].name, name) == 0) {
            return &roles[index];
        }
    }
    return NULL;
}

static int read_uid(const char *directory, unsigned int *uid) {
    char path[64];
    snprintf(path, sizeof(path), "%s/status", directory);
    FILE *file = fopen(path, "r");
    if (file == NULL) {
        return -1;
    }
    char line[160];
    int found = -1;
    while (fgets(line, sizeof(line), file) != NULL) {
        if (sscanf(line, "Uid:\t%u", uid) == 1) {
            found = 0;
            break;
        }
    }
    fclose(file);
    return found;
}

static int role_pid(const struct role *role) {
    DIR *proc = opendir("/proc");
    if (proc == NULL) {
        return -1;
    }
    int result = -1;
    struct dirent *entry;
    while ((entry = readdir(proc)) != NULL) {
        char *end = NULL;
        long pid = strtol(entry->d_name, &end, 10);
        if (pid <= 0 || *end != '\0') {
            continue;
        }
        char directory[48];
        snprintf(directory, sizeof(directory), "/proc/%ld", pid);
        unsigned int uid;
        if (read_uid(directory, &uid) != 0 || uid != role->uid) {
            continue;
        }
        char path[64];
        snprintf(path, sizeof(path), "%s/cmdline", directory);
        int fd = open(path, O_RDONLY | O_CLOEXEC);
        if (fd < 0) {
            continue;
        }
        char command[128];
        ssize_t count = read(fd, command, sizeof(command) - 1);
        close(fd);
        if (count <= 0) {
            continue;
        }
        command[count] = '\0';
        if (strcmp(command, role->program) == 0) {
            result = (int)pid;
            break;
        }
    }
    closedir(proc);
    return result;
}

static int process_value(const char *role_name, const char *kind) {
    const struct role *role = find_role(role_name);
    int pid = role == NULL ? -1 : role_pid(role);
    if (pid <= 0) {
        return -1;
    }
    if (strcmp(kind, "pid") == 0) {
        printf("%d\n", pid);
        return 0;
    }
    char path[64];
    snprintf(path, sizeof(path), "/proc/%d/%s", pid,
             strcmp(kind, "ticks") == 0 ? "stat" : "statm");
    FILE *file = fopen(path, "r");
    if (file == NULL) {
        return -1;
    }
    unsigned long long value = 0;
    if (strcmp(kind, "ticks") == 0) {
        char line[512];
        if (fgets(line, sizeof(line), file) == NULL) {
            fclose(file);
            return -1;
        }
        char *tail = strrchr(line, ')');
        if (tail == NULL) {
            fclose(file);
            return -1;
        }
        char state;
        unsigned long long ignored[10], user, system;
        int count = sscanf(tail + 1,
            " %c %llu %llu %llu %llu %llu %llu %llu %llu %llu %llu %llu %llu",
            &state, &ignored[0], &ignored[1], &ignored[2], &ignored[3], &ignored[4],
            &ignored[5], &ignored[6], &ignored[7], &ignored[8], &ignored[9], &user,
            &system);
        if (count != 13) {
            fclose(file);
            return -1;
        }
        value = user + system;
    } else {
        unsigned long long virtual_pages;
        if (fscanf(file, "%llu %llu", &virtual_pages, &value) != 2) {
            fclose(file);
            return -1;
        }
    }
    fclose(file);
    printf("%llu\n", value);
    return 0;
}

static int wait_pid_change(const char *role_name, uint64_t previous) {
    const struct role *role = find_role(role_name);
    if (role == NULL || previous > INT32_MAX) {
        return -1;
    }
    for (unsigned int attempt = 0; attempt < 500; ++attempt) {
        int pid = role_pid(role);
        if (pid > 0 && pid != (int)previous) {
            printf("%d\n", pid);
            return 0;
        }
        pause_milliseconds(10);
    }
    return -1;
}

int main(int argc, char **argv) {
    if (argc == 2 && strcmp(argv[1], "wait-ready") == 0) {
        return wait_ready() == 0 ? 0 : 1;
    }
    if (argc == 2 && strcmp(argv[1], "pointer-samples") == 0) {
        struct snapshot current;
        if (query(&current) == 0) {
            printf("%llu\n", (unsigned long long)current.pointer_samples);
            return 0;
        }
        return 1;
    }
    if (argc == 2 && strcmp(argv[1], "frames") == 0) {
        struct snapshot current;
        if (query(&current) != 0) return 1;
        printf("%llu\n", (unsigned long long)current.frames_completed);
        return 0;
    }
    if (argc == 3 && strcmp(argv[1], "wait-frame") == 0) {
        uint64_t previous;
        return parse_u64(argv[2], &previous) == 0 && wait_frame(previous) == 0 ? 0 : 1;
    }
    if (argc == 3 && strcmp(argv[1], "wait-pointer") == 0) {
        uint64_t previous;
        return parse_u64(argv[2], &previous) == 0 && wait_pointer(previous) == 0 ? 0 : 1;
    }
    if (argc == 2 && strcmp(argv[1], "resize-commits") == 0) {
        struct snapshot current;
        if (query(&current) == 0) {
            printf("%llu\n", (unsigned long long)current.resize_commits);
            return 0;
        }
        return 1;
    }
    if (argc == 5 && strcmp(argv[1], "wait-resize") == 0) {
        uint64_t previous, width, height;
        return parse_u64(argv[2], &previous) == 0 &&
               parse_u64(argv[3], &width) == 0 && width <= UINT32_MAX &&
               parse_u64(argv[4], &height) == 0 && height <= UINT32_MAX &&
               wait_resize(previous, (uint32_t)width, (uint32_t)height) == 0 ? 0 : 1;
    }
    if (argc == 3 && (strcmp(argv[1], "pid") == 0 ||
                      strcmp(argv[1], "ticks") == 0 ||
                      strcmp(argv[1], "rss") == 0)) {
        return process_value(argv[2], argv[1]) == 0 ? 0 : 1;
    }
    if (argc == 4 && strcmp(argv[1], "wait-pid") == 0) {
        uint64_t previous;
        return parse_u64(argv[3], &previous) == 0 &&
               wait_pid_change(argv[2], previous) == 0 ? 0 : 1;
    }
    fprintf(stderr, "usage: liteui-inspect wait-ready|frames|wait-frame N|pointer-samples|wait-pointer N|resize-commits|wait-resize N W H|pid ROLE|ticks ROLE|rss ROLE|wait-pid ROLE PID\n");
    return 2;
}
