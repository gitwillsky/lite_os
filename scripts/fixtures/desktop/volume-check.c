/* Independent standard Linux ALSA observer; installed only into a private gate image. */
#include <errno.h>
#include <fcntl.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sound/asound.h>
#include <time.h>
#include <unistd.h>
_Static_assert(sizeof(struct snd_ctl_elem_info) == 272, "LP64 ALSA info");
_Static_assert(sizeof(struct snd_ctl_elem_value) == 1224, "LP64 ALSA value");
_Static_assert(offsetof(struct snd_ctl_elem_value, value.integer.value) == 72, "LP64 value offset");
static void check(int condition, const char *stage) {
    if (!condition) { fprintf(stderr, "volume-check: error: %s errno=%d\n", stage, errno); exit(1); }
}
static struct snd_ctl_elem_info element(int fd, const char *name, int type) {
    struct snd_ctl_elem_info info = {0};
    info.id.iface = SNDRV_CTL_ELEM_IFACE_MIXER;
    snprintf((char *)info.id.name, sizeof(info.id.name), "%s", name);
    check(ioctl(fd, SNDRV_CTL_IOCTL_ELEM_INFO, &info) == 0, "info");
    check(info.type == type && info.count == 2, "stereo type");
    return info;
}
static void write_pair(int fd, struct snd_ctl_elem_info *info, long left, long right) {
    struct snd_ctl_elem_value value = {0}; value.id = info->id;
    value.value.integer.value[0] = left; value.value.integer.value[1] = right;
    check(ioctl(fd, SNDRV_CTL_IOCTL_ELEM_WRITE, &value) == 0, "write");
}
static int matches(int fd, struct snd_ctl_elem_info *info, long expected) {
    struct snd_ctl_elem_value value = {0}; value.id = info->id;
    check(ioctl(fd, SNDRV_CTL_IOCTL_ELEM_READ, &value) == 0, "read");
    return value.value.integer.value[0] == expected && value.value.integer.value[1] == expected;
}
static void wait_state(int fd, struct snd_ctl_elem_info *volume, struct snd_ctl_elem_info *sw, long gain, long enabled) {
    struct timespec start, now;
    check(clock_gettime(CLOCK_MONOTONIC, &start) == 0, "clock");
    do {
        if (matches(fd, volume, gain) && matches(fd, sw, enabled)) return;
        usleep(50000);
        check(clock_gettime(CLOCK_MONOTONIC, &now) == 0, "clock");
    } while (now.tv_sec - start.tv_sec < 90);
    check(0, "UI Master value timeout");
}
int main(void) {
    int fd = open("/dev/snd/controlC0", O_RDWR | O_CLOEXEC);
    check(fd >= 0, "open");
    struct snd_ctl_elem_info volume = element(fd, "Master Playback Volume", SNDRV_CTL_ELEM_TYPE_INTEGER);
    struct snd_ctl_elem_info sw = element(fd, "Master Playback Switch", SNDRV_CTL_ELEM_TYPE_BOOLEAN);
    long lo = volume.value.integer.min, hi = volume.value.integer.max;
    check(lo >= 0 && hi > lo && hi <= 0x7fffffffL, "gain range");
    write_pair(fd, &volume, hi, hi); write_pair(fd, &sw, 1, 1);
    puts("VOLUME_CONTROL_OK"); fflush(stdout);
    wait_state(fd, &volume, &sw, lo + (hi - lo) / 2, 1);
    puts("VOLUME_UI_50"); fflush(stdout);
    wait_state(fd, &volume, &sw, lo + (hi - lo) / 2, 0);
    puts("VOLUME_UI_MUTED"); fflush(stdout);
    close(fd); return 0;
}
