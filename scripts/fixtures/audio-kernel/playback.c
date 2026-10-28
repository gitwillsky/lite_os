/* 上游 alsa-lib 的八路 RW consumer：样本只由公开 API 送入标准 PCM 设备。 */
#include <alsa/asoundlib.h>
#include <errno.h>
#include <sys/ioctl.h>
#include <unistd.h>
#include <math.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>

#define STREAMS 8
#define RATE 48000
#define PERIOD 256
#define PHASES 6

static void check(int step, int result)
{
    if (result < 0) {
        fprintf(stderr, "ALSA_PROBE_FAILED playback step=%d error=%d %s\n", step, result, snd_strerror(result));
        exit(1);
    }
}

static void control(snd_ctl_t *ctl, unsigned numid, long left, long right)
{
    snd_ctl_elem_value_t *value;
    snd_ctl_elem_value_alloca(&value);
    snd_ctl_elem_value_set_numid(value, numid);
    snd_ctl_elem_value_set_integer(value, 0, left);
    snd_ctl_elem_value_set_integer(value, 1, right);
    check(30, snd_ctl_elem_write(ctl, value));
}

static void configure(snd_pcm_t **pcm, unsigned index)
{
    char name[32];
    snprintf(name, sizeof(name), "hw:0,0,%u", index);
    check(1, snd_pcm_open(pcm, name, SND_PCM_STREAM_PLAYBACK, SND_PCM_NONBLOCK));
    snd_pcm_hw_params_t *hardware;
    snd_pcm_hw_params_alloca(&hardware);
    check(2, snd_pcm_hw_params_any(*pcm, hardware));
    check(3, snd_pcm_hw_params_set_access(*pcm, hardware, SND_PCM_ACCESS_RW_INTERLEAVED));
    check(4, snd_pcm_hw_params_set_format(*pcm, hardware, SND_PCM_FORMAT_FLOAT_LE));
    check(5, snd_pcm_hw_params_set_channels(*pcm, hardware, 2));
    check(6, snd_pcm_hw_params_set_rate(*pcm, hardware, RATE, 0));
    check(7, snd_pcm_hw_params_set_period_size(*pcm, hardware, PERIOD, 0));
    check(8, snd_pcm_hw_params_set_buffer_size(*pcm, hardware, 1024));
    check(9, snd_pcm_hw_params(*pcm, hardware));
    snd_pcm_sw_params_t *software;
    snd_pcm_sw_params_alloca(&software);
    check(10, snd_pcm_sw_params_current(*pcm, software));
    snd_pcm_uframes_t boundary;
    check(11, snd_pcm_sw_params_get_boundary(software, &boundary));
    check(12, snd_pcm_sw_params_set_avail_min(*pcm, software, PERIOD));
    check(13, snd_pcm_sw_params_set_start_threshold(*pcm, software, boundary));
    check(14, snd_pcm_sw_params_set_tstamp_mode(*pcm, software, SND_PCM_TSTAMP_ENABLE));
    check(15, snd_pcm_sw_params(*pcm, software));
    snd_pcm_uframes_t available;
    snd_htimestamp_t timestamp;
    check(16, snd_pcm_htimestamp(*pcm, &available, &timestamp));
    if (timestamp.tv_sec == 0 && timestamp.tv_nsec == 0) check(17, -EIO);
    /* alsa-lib 的 HW_PARAMS 已准备流；mpv 等 consumer 仍会在启动前再次 PREPARE。 */
    check(18, snd_pcm_prepare(*pcm));
    snd_pcm_status_t *status;
    snd_pcm_status_alloca(&status);
    check(19, snd_pcm_status(*pcm, status));
    float short_frames[17][2] = {{0}};
    check(60, snd_pcm_writei(*pcm, short_frames, 17) == 17 ? 0 : -EIO);
    check(61, snd_pcm_prepare(*pcm));
}

static void samples(float output[PERIOD][2], unsigned stream, uint64_t from)
{
    for (unsigned i = 0; i < PERIOD; ++i) {
        uint64_t frame = from + i;
        unsigned phase = frame / RATE;
        float value = 0.0f;
        if (phase == 5) value = 0.25f;
        else if (phase < 5) {
            double amplitude = phase == 0 ? (stream == 0 ? 0.16 : 0.0) : 0.04;
            value = amplitude * sin(2.0 * 3.14159265358979323846 * (440 + stream * 110) * frame / RATE);
        }
        output[i][0] = output[i][1] = value;
    }
}

int main(void)
{
    snd_ctl_t *ctl;
    check(20, snd_ctl_open(&ctl, "hw:0", 0));
    snd_pcm_t *pcm[STREAMS];
    uint64_t written[STREAMS] = {0};
    float period[PERIOD][2];
    struct pollfd descriptors[STREAMS];
    for (unsigned i = 0; i < STREAMS; ++i) {
        configure(&pcm[i], i);
        control(ctl, 3 + i, 100, 100);
        for (unsigned prefill = 0; prefill < 3; ++prefill) {
            samples(period, i, written[i]);
            int result = snd_pcm_writei(pcm[i], period, PERIOD);
            check(21, result == PERIOD ? 0 : (result < 0 ? result : -EIO));
            written[i] += PERIOD;
        }
        check(22, snd_pcm_poll_descriptors(pcm[i], &descriptors[i], 1));
    }
    for (unsigned i = 0; i < STREAMS; ++i) check(23, snd_pcm_start(pcm[i]));
    unsigned next_phase = 1;
    for (;;) {
        uint64_t minimum = written[0];
        for (unsigned i = 1; i < STREAMS; ++i) if (written[i] < minimum) minimum = written[i];
        if (minimum >= PHASES * RATE) break;
        if (minimum >= next_phase * RATE) {
            if (next_phase == 2) control(ctl, 3, 50, 100);
            if (next_phase == 3) control(ctl, 1, 50, 75);
            if (next_phase == 4) control(ctl, 2, 0, 1);
            if (next_phase == 5) {
                control(ctl, 1, 100, 100);
                control(ctl, 2, 1, 1);
                control(ctl, 3, 100, 100);
            }
            ++next_phase;
        }
        int progress = 0;
        for (unsigned i = 0; i < STREAMS; ++i) {
            snd_pcm_sframes_t available = snd_pcm_avail_update(pcm[i]);
            check(24, available);
            if (available < PERIOD) continue;
            samples(period, i, written[i]);
            int result = snd_pcm_writei(pcm[i], period, PERIOD);
            if (result == -EAGAIN) continue;
            check(25, result == PERIOD ? 0 : (result < 0 ? result : -EIO));
            written[i] += PERIOD;
            progress = 1;
        }
        if (!progress) check(26, poll(descriptors, STREAMS, 1000));
    }
    for (unsigned i = 0; i < STREAMS; ++i) {
        check(27, snd_pcm_drop(pcm[i]));
        check(28, snd_pcm_close(pcm[i]));
    }
    // 在硬件队列超过容量后实际阻塞 WRITEI，不能只覆盖用户态 poll 的等待路径。
    snd_pcm_t *blocking;
    configure(&blocking, 0);
    check(31, snd_pcm_nonblock(blocking, 0));
    float silence[32 * PERIOD][2] = {{0}};
    check(32, snd_pcm_writei(blocking, silence, 3 * PERIOD) == 3 * PERIOD ? 0 : -EIO);
    check(33, snd_pcm_start(blocking));
    check(34, snd_pcm_writei(blocking, silence, 32 * PERIOD) == 32 * PERIOD ? 0 : -EIO);
    check(35, snd_pcm_drop(blocking));
    check(37, snd_pcm_prepare(blocking));
    check(38, snd_pcm_writei(blocking, silence, PERIOD) == PERIOD ? 0 : -EIO);
    check(39, snd_pcm_start(blocking));
    snd_pcm_status_t *status;
    snd_pcm_status_alloca(&status);
    for (unsigned attempt = 0; attempt < 2000; ++attempt) {
        check(40, snd_pcm_status(blocking, status));
        if (snd_pcm_status_get_state(status) == SND_PCM_STATE_XRUN) break;
        usleep(1000);
    }
    check(41, snd_pcm_status_get_state(status) == SND_PCM_STATE_XRUN ? 0 : -EIO);
    /* 直接进入实际 WRITEI：alsa-lib 自身的 cached-state 检查不能替内核挡住 XRUN 忙循环。 */
    struct pollfd blocking_descriptor;
    check(44, snd_pcm_poll_descriptors(blocking, &blocking_descriptor, 1));
    /* native snd_xferi 布局；内核 asound.h 与 alsa-lib 公共头的同名 typedef 不能共用一个 TU。 */
    struct pcm_transfer {
        snd_pcm_sframes_t result;
        void *buf;
        snd_pcm_uframes_t frames;
    } transfer = {.buf = silence, .frames = PERIOD};
    check(42, ioctl(blocking_descriptor.fd, _IOW('A', 0x50, struct pcm_transfer), &transfer) == -1 && errno == EPIPE ? 0 : -EIO);
    check(43, snd_pcm_prepare(blocking));
    puts("ALSA_XRUN_RECOVERY_READY");
    check(36, snd_pcm_close(blocking));
    check(29, snd_ctl_close(ctl));
    printf("ALSA_PLAYBACK_READY streams=%u frames=%u xruns=0\n", STREAMS, PHASES * RATE);
    return 0;
}
