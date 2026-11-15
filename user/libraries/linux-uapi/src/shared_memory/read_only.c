/* Recovery stays within C so a SIGBUS never unwinds a Rust frame. */
#define _POSIX_C_SOURCE 200809L
#include <errno.h>
#include <setjmp.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <string.h>

struct shm_access {
    sigjmp_buf recovery;
    uintptr_t start;
    uintptr_t end;
};

/* Only this thread's C stack record is visible to the synchronous handler.
 * Locking in the handler could deadlock an interrupted operation. */
_Static_assert(ATOMIC_POINTER_LOCK_FREE == 2, "SIGBUS recovery requires lock-free pointers");
static _Thread_local _Atomic(struct shm_access *) active;
static struct sigaction previous_bus;
static struct sigaction default_bus;

static void handle_bus(int signal, siginfo_t *info, void *context)
{
    struct shm_access *access = atomic_load_explicit(&active, memory_order_acquire);
    uintptr_t address = (uintptr_t)info->si_addr;
    if (access && info->si_code > 0 && address >= access->start && address < access->end)
        siglongjmp(access->recovery, 1);

    /* Preserve the runtime's stack-overflow handler and unrelated SIGBUS behavior. */
    if (previous_bus.sa_handler == SIG_IGN && info->si_code <= 0)
        return;
    if (previous_bus.sa_handler == SIG_DFL || previous_bus.sa_handler == SIG_IGN) {
        sigaction(SIGBUS, &default_bus, NULL);
        raise(signal);
    } else if (previous_bus.sa_flags & SA_SIGINFO) {
        previous_bus.sa_sigaction(signal, info, context);
    } else {
        previous_bus.sa_handler(signal);
    }
}

int liteos_install_shm_bus_handler(void)
{
    if (sigaction(SIGBUS, NULL, &previous_bus) < 0)
        return -1;
    if (previous_bus.sa_flags & SA_RESETHAND) {
        errno = ENOTSUP;
        return -1;
    }
    default_bus.sa_handler = SIG_DFL;
    sigemptyset(&default_bus.sa_mask);
    struct sigaction action = previous_bus;
    action.sa_sigaction = handle_bus;
    action.sa_flags |= SA_SIGINFO;
    return sigaction(SIGBUS, &action, NULL);
}

/* Both extents are checked by the caller and stay live for this call. */
int liteos_copy_shm_pixels(const unsigned char *source, size_t source_stride,
                           uint32_t *output, size_t output_stride,
                           size_t width, size_t height, uint32_t alpha)
{
    struct shm_access access;
    size_t row_bytes = width * sizeof(uint32_t);
    access.start = (uintptr_t)source;
    access.end = access.start + (height - 1) * source_stride + row_bytes;
    if (sigsetjmp(access.recovery, 1)) {
        atomic_store_explicit(&active, NULL, memory_order_release);
        return -1;
    }
    atomic_store_explicit(&active, &access, memory_order_release);
    if (alpha == 0 && source_stride == row_bytes && output_stride == width) {
        memcpy(output, source, height * row_bytes);
    } else {
        for (size_t y = 0; y < height; ++y) {
            const unsigned char *row = source + y * source_stride;
            uint32_t *destination = output + y * output_stride;
            if (alpha == 0) {
                memcpy(destination, row, row_bytes);
            } else {
                for (size_t x = 0; x < width; ++x) {
                    uint32_t pixel;
                    memcpy(&pixel, row + x * sizeof(pixel), sizeof(pixel));
                    destination[x] = pixel | alpha;
                }
            }
        }
    }
    atomic_store_explicit(&active, NULL, memory_order_release);
    return 0;
}
