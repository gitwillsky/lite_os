/* 固定 Linux UAPI 的真实 control/OFD/子设备验证；不依赖项目私有协议。 */
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <sound/asound.h>
#include <stddef.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/syscall.h>
#include <unistd.h>

_Static_assert(sizeof(struct snd_ctl_event) == 72, "control event ABI");
_Static_assert(offsetof(struct snd_ctl_event, data.elem.id) == 8, "event id ABI");
_Static_assert(sizeof(struct snd_ctl_elem_value) == 1224, "control value ABI");

#define CHECK(step, condition) do { if (!(condition)) { dprintf(2, "ALSA_PROBE_FAILED control step=%d errno=%d\n", step, errno); _exit(step); } } while (0)

static void put(int fd, unsigned id, long left, long right, int step)
{
    struct snd_ctl_elem_value value = {0};
    value.id.numid = id;
    value.value.integer.value[0] = left;
    value.value.integer.value[1] = right;
    CHECK(step, ioctl(fd, SNDRV_CTL_IOCTL_ELEM_WRITE, &value) == 0);
    CHECK(step, value.id.iface == SNDRV_CTL_ELEM_IFACE_MIXER);
}

static int pcm_open(int expected, int step)
{
    int fd = open("/dev/snd/pcmC0D0p", O_RDWR | O_NONBLOCK | O_CLOEXEC);
    CHECK(step, fd >= 0);
    struct snd_pcm_info info = {0};
    CHECK(step, ioctl(fd, SNDRV_PCM_IOCTL_INFO, &info) == 0);
    CHECK(step, (int)info.subdevice == expected && info.subdevices_count == 8);
    unsigned protocol = SNDRV_PCM_VERSION;
    CHECK(step, ioctl(fd, SNDRV_PCM_IOCTL_USER_PVERSION, &protocol) == 0);
    CHECK(step, ioctl(fd, SNDRV_PCM_IOCTL_USER_PVERSION, (void *)1) == -1 && errno == EFAULT);
    int clock = -1;
    CHECK(step, ioctl(fd, SNDRV_PCM_IOCTL_TTSTAMP, &clock) == -1 && errno == EINVAL);
    clock = SNDRV_PCM_TSTAMP_TYPE_MONOTONIC;
    CHECK(step, ioctl(fd, SNDRV_PCM_IOCTL_TTSTAMP, &clock) == 0);
    return fd;
}

static void software_parameters(void)
{
    int fd = pcm_open(0, 60);
    struct snd_pcm_sw_params software = {.tstamp_mode = 2, .avail_min = 256,
        .start_threshold = 1024, .stop_threshold = 1024, .boundary = 1ULL << 31};
    CHECK(61, ioctl(fd, SNDRV_PCM_IOCTL_SW_PARAMS, &software) == -1 && errno == EBADFD);
    struct snd_pcm_hw_params hardware = {0};
    for (unsigned i = 0; i < 3; ++i)
        for (unsigned j = 0; j < 8; ++j) hardware.masks[i].bits[j] = ~0U;
    for (unsigned i = 0; i < 12; ++i) hardware.intervals[i].max = ~0U;
    hardware.rmask = ~0U;
    CHECK(62, ioctl(fd, SNDRV_PCM_IOCTL_HW_REFINE, &hardware) == 0);
    hardware.masks[0].bits[0] = 1U << SNDRV_PCM_ACCESS_RW_INTERLEAVED;
    CHECK(63, ioctl(fd, SNDRV_PCM_IOCTL_HW_PARAMS, &hardware) == 0);
    CHECK(64, ioctl(fd, SNDRV_PCM_IOCTL_SW_PARAMS, &software) == -1 && errno == EINVAL);
    software.tstamp_mode = SNDRV_PCM_TSTAMP_ENABLE;
    software.proto = SNDRV_PROTOCOL_VERSION(2, 0, 12);
    software.tstamp_type = 3;
    CHECK(65, ioctl(fd, SNDRV_PCM_IOCTL_SW_PARAMS, &software) == -1 && errno == EINVAL);
    software.tstamp_type = SNDRV_PCM_TSTAMP_TYPE_MONOTONIC;
    CHECK(66, ioctl(fd, SNDRV_PCM_IOCTL_SW_PARAMS, &software) == 0);
    CHECK(67, ioctl(fd, SNDRV_PCM_IOCTL_PREPARE) == 0);
    CHECK(68, ioctl(fd, SNDRV_PCM_IOCTL_SW_PARAMS, &software) == 0);
    struct snd_pcm_status status = {0};
    CHECK(69, ioctl(fd, SNDRV_PCM_IOCTL_STATUS, &status) == 0);
    CHECK(70, status.tstamp.tv_sec != 0 || status.tstamp.tv_nsec != 0);
    CHECK(71, close(fd) == 0);
}

static void *other_thread(void *unused)
{
    (void)unused;
    int fd = pcm_open(0, 40);
    CHECK(41, close(fd) == 0);
    return NULL;
}

struct event_request { int ctl; int ready; int use_poll; };

static void *wait_event(void *argument)
{
    struct event_request *request = argument;
    CHECK(50, write(request->ready, "r", 1) == 1);
    if (request->use_poll) {
        struct pollfd descriptor = {.fd = request->ctl, .events = POLLIN};
        CHECK(51, poll(&descriptor, 1, 1000) == 1 && (descriptor.revents & POLLIN));
    }
    struct snd_ctl_event event;
    CHECK(52, read(request->ctl, &event, sizeof(event)) == sizeof(event));
    CHECK(53, event.data.elem.id.numid == 4);
    return NULL;
}

int main(void)
{
    int ctl = open("/dev/snd/controlC0", O_RDWR | O_NONBLOCK | O_CLOEXEC);
    CHECK(1, ctl >= 0);
    software_parameters();
    struct snd_ctl_event events[2];
    CHECK(2, read(ctl, events, sizeof(events)) == -1 && errno == EBADFD);
    int subscribe = -1;
    CHECK(3, ioctl(ctl, SNDRV_CTL_IOCTL_SUBSCRIBE_EVENTS, &subscribe) == 0 && subscribe == 0);
    subscribe = 1;
    CHECK(4, ioctl(ctl, SNDRV_CTL_IOCTL_SUBSCRIBE_EVENTS, &subscribe) == 0);
    subscribe = -1;
    CHECK(5, ioctl(ctl, SNDRV_CTL_IOCTL_SUBSCRIBE_EVENTS, &subscribe) == 0 && subscribe == 1);
    CHECK(6, read(ctl, events, sizeof(events)) == -1 && errno == EAGAIN);
    CHECK(7, syscall(SYS_read, ctl, (void *)1, sizeof(events[0])) == -1 && errno == EAGAIN);
    CHECK(8, read(ctl, events, 1) == -1 && errno == EINVAL);

    struct snd_ctl_elem_id ids[10];
    struct snd_ctl_elem_list list = {.space = 10, .pids = ids};
    CHECK(9, ioctl(ctl, SNDRV_CTL_IOCTL_ELEM_LIST, &list) == 0 && list.count == 10 && list.used == 10);
    CHECK(10, ids[9].numid == 10 && ids[9].index == 7 && !strcmp((char *)ids[9].name, "PCM Playback Volume"));
    list.space = 1;
    list.pids = NULL;
    CHECK(11, ioctl(ctl, SNDRV_CTL_IOCTL_ELEM_LIST, &list) == -1 && errno == EFAULT);
    list.space = 0;
    CHECK(12, ioctl(ctl, SNDRV_CTL_IOCTL_ELEM_LIST, &list) == 0 && list.used == 0 && list.count == 10);

    put(ctl, 2, 0, 1, 13);
    put(ctl, 1, 50, 75, 14);
    put(ctl, 1, 25, 75, 15);
    struct pollfd pollfd = {.fd = ctl, .events = POLLIN};
    CHECK(16, poll(&pollfd, 1, 0) == 1 && (pollfd.revents & POLLIN));
    CHECK(17, read(ctl, events, sizeof(events)) == (ssize_t)sizeof(events));
    CHECK(18, events[0].type == SNDRV_CTL_EVENT_ELEM && events[0].data.elem.mask == SNDRV_CTL_EVENT_MASK_VALUE && events[0].data.elem.id.numid == 2);
    CHECK(19, events[1].data.elem.id.numid == 1);
    put(ctl, 1, 25, 75, 20);
    CHECK(21, read(ctl, events, sizeof(events)) == -1 && errno == EAGAIN);
    put(ctl, 1, 30, 75, 22);
    subscribe = 0;
    CHECK(23, ioctl(ctl, SNDRV_CTL_IOCTL_SUBSCRIBE_EVENTS, &subscribe) == 0);
    subscribe = 1;
    CHECK(24, ioctl(ctl, SNDRV_CTL_IOCTL_SUBSCRIBE_EVENTS, &subscribe) == 0);
    CHECK(25, read(ctl, events, sizeof(events)) == -1 && errno == EAGAIN);

    int preference = open("/dev/snd/controlC0", O_RDWR | O_CLOEXEC);
    CHECK(26, preference >= 0);
    int selected = 5;
    CHECK(27, ioctl(preference, SNDRV_CTL_IOCTL_PCM_PREFER_SUBDEVICE, &selected) == 0);
    int pcm = pcm_open(5, 28);
    CHECK(29, open("/dev/snd/pcmC0D0p", O_RDWR | O_NONBLOCK) == -1 && errno == EBUSY);
    pthread_t thread;
    CHECK(30, pthread_create(&thread, NULL, other_thread, NULL) == 0);
    CHECK(31, pthread_join(thread, NULL) == 0);
    CHECK(32, close(pcm) == 0);
    int duplicate = dup(preference);
    CHECK(33, duplicate >= 0 && close(preference) == 0);
    pcm = pcm_open(5, 34);
    CHECK(35, close(duplicate) == 0);
    int automatic = pcm_open(0, 36);
    CHECK(37, close(pcm) == 0 && close(automatic) == 0);
    for (int use_poll = 1; use_poll >= 0; --use_poll) {
        int ready[2];
        CHECK(42, pipe(ready) == 0);
        CHECK(43, fcntl(ctl, F_SETFL, use_poll ? O_NONBLOCK : 0) == 0);
        struct event_request request = {ctl, ready[1], use_poll};
        CHECK(44, pthread_create(&thread, NULL, wait_event, &request) == 0);
        char byte;
        CHECK(45, read(ready[0], &byte, 1) == 1);
        usleep(20000);
        put(ctl, 4, 92 + use_poll, 100, 46);
        CHECK(47, pthread_join(thread, NULL) == 0);
        CHECK(48, close(ready[0]) == 0 && close(ready[1]) == 0);
    }
    CHECK(38, close(ctl) == 0);
    puts("ALSA_CONTROL_READY");
    return 0;
}
