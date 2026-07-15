// SPDX-License-Identifier: MIT
// Minimal DRM/evdev/PTY terminal for the LiteOS reference userspace.

#include <ctype.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define IOC_NONE 0U
#define IOC_WRITE 1U
#define IOC_READ 2U
#define IOC(dir, type, number, size) \
    ((dir) << 30 | (size) << 16 | (type) << 8 | (number))
#define DRM_IOWR(number, type) IOC(IOC_READ | IOC_WRITE, 'd', number, sizeof(type))
#define DRM_IOCTL_MODE_GETRESOURCES DRM_IOWR(0xa0, struct drm_resources)
#define DRM_IOCTL_MODE_GETCONNECTOR DRM_IOWR(0xa7, struct drm_connector)
#define DRM_IOCTL_MODE_SETCRTC DRM_IOWR(0xa2, struct drm_crtc)
#define DRM_IOCTL_MODE_ADDFB DRM_IOWR(0xae, struct drm_fb)
#define DRM_IOCTL_MODE_RMFB DRM_IOWR(0xaf, uint32_t)
#define DRM_IOCTL_MODE_PAGE_FLIP DRM_IOWR(0xb0, struct drm_flip)
#define DRM_IOCTL_MODE_CREATE_DUMB DRM_IOWR(0xb2, struct drm_dumb_create)
#define DRM_IOCTL_MODE_MAP_DUMB DRM_IOWR(0xb3, struct drm_dumb_map)
#define DRM_IOCTL_MODE_DESTROY_DUMB DRM_IOWR(0xb4, struct drm_dumb_destroy)
#ifndef TIOCGPTN
#define TIOCGPTN 0x80045430UL
#endif
#ifndef TIOCSPTLCK
#define TIOCSPTLCK 0x40045431UL
#endif
#ifndef TIOCSCTTY
#define TIOCSCTTY 0x540eUL
#endif
#define EVIOCGNAME(length) IOC(IOC_READ, 'E', 0x06, (length))

#define CELL_WIDTH 16U
#define CELL_HEIGHT 32U
#define CURSOR_HEIGHT 3U
#define PSF2_MAGIC 0x864ab572U
#define PSF2_HAS_UNICODE_TABLE 1U
#define FONT_PATH "/usr/share/consolefonts/spleen-16x32.psfu"
#define DRM_PAGE_FLIP_EVENT 1U
#define EV_KEY 1U
#define MODE_PROBE_INTERVAL_MS 1000

struct drm_mode {
    uint32_t clock;
    uint16_t hdisplay, hsync_start, hsync_end, htotal, hskew;
    uint16_t vdisplay, vsync_start, vsync_end, vtotal, vscan;
    uint32_t vrefresh, flags, type;
    char name[32];
};

struct drm_resources {
    uint64_t fb_id_ptr, crtc_id_ptr, connector_id_ptr, encoder_id_ptr;
    uint32_t count_fbs, count_crtcs, count_connectors, count_encoders;
    uint32_t min_width, max_width, min_height, max_height;
};

struct drm_connector {
    uint64_t encoder_ptr, mode_ptr, property_ptr, property_value_ptr;
    uint32_t count_modes, count_properties, count_encoders, encoder_id;
    uint32_t connector_id, connector_type, connector_type_id, connection;
    uint32_t width_mm, height_mm, subpixel, pad;
};

struct drm_crtc {
    uint64_t connector_ptr;
    uint32_t count_connectors, crtc_id, fb_id, x, y, gamma_size, mode_valid;
    struct drm_mode mode;
};

struct drm_dumb_create {
    uint32_t height, width, bpp, flags, handle, pitch;
    uint64_t size;
};

struct drm_dumb_map {
    uint32_t handle, pad;
    uint64_t offset;
};

struct drm_dumb_destroy {
    uint32_t handle;
};

struct drm_fb {
    uint32_t fb_id, width, height, pitch, bpp, depth, handle;
};

struct drm_flip {
    uint32_t crtc_id, fb_id, flags, reserved;
    uint64_t user_data;
};

struct input_event {
    int64_t seconds, microseconds;
    uint16_t type, code;
    int32_t value;
};

_Static_assert(sizeof(struct drm_mode) == 68, "DRM mode ABI drift");
_Static_assert(sizeof(struct drm_resources) == 64, "DRM resources ABI drift");
_Static_assert(sizeof(struct drm_connector) == 80, "DRM connector ABI drift");
_Static_assert(sizeof(struct drm_crtc) == 104, "DRM CRTC ABI drift");
_Static_assert(sizeof(struct drm_dumb_create) == 32, "DRM dumb ABI drift");
_Static_assert(sizeof(struct drm_flip) == 24, "DRM page-flip ABI drift");
_Static_assert(sizeof(struct input_event) == 24, "evdev event ABI drift");

struct cell {
    uint8_t character, foreground, background;
    uint8_t dirty;
};

struct terminal {
    struct cell *cells;
    size_t columns, rows, column, row;
    uint8_t foreground, background;
    unsigned parser;
    unsigned parameters[8], parameter_count;
};

struct display {
    int fd;
    uint32_t crtc_id, connector_id, framebuffer_id, handle;
    uint32_t width, height, pitch;
    uint64_t size, sequence;
    uint32_t *pixels;
    struct drm_mode mode;
};

struct font {
    void *mapping;
    size_t size;
    const uint8_t *glyphs[128];
};

static const uint32_t palette[16] = {
    0x00101418, 0x00c0392b, 0x0038a169, 0x00d69e2e,
    0x003b82f6, 0x00a855f7, 0x000ea5a8, 0x00cbd5e1,
    0x00475569, 0x00ef4444, 0x0022c55e, 0x00facc15,
    0x0060a5fa, 0x00c084fc, 0x002dd4bf, 0x00f8fafc,
};

static uint32_t read_le32(const uint8_t *bytes) {
    return (uint32_t)bytes[0] | (uint32_t)bytes[1] << 8 |
           (uint32_t)bytes[2] << 16 | (uint32_t)bytes[3] << 24;
}

static const uint8_t *decode_utf8(const uint8_t *cursor, const uint8_t *end,
                                  uint32_t *codepoint) {
    if (cursor == end)
        return NULL;
    uint8_t first = *cursor++;
    if (first < 0x80) {
        *codepoint = first;
        return cursor;
    }
    unsigned continuation_count = first < 0xe0 ? 1U : first < 0xf0 ? 2U : 3U;
    uint32_t value = first & (0x7fU >> continuation_count);
    if ((size_t)(end - cursor) < continuation_count)
        return NULL;
    for (unsigned index = 0; index < continuation_count; ++index) {
        if ((cursor[index] & 0xc0) != 0x80)
            return NULL;
        value = value << 6 | (cursor[index] & 0x3f);
    }
    *codepoint = value;
    return cursor + continuation_count;
}

static int font_open(struct font *font) {
    memset(font, 0, sizeof(*font));
    int fd = open(FONT_PATH, O_RDONLY | O_CLOEXEC);
    if (fd < 0)
        return -1;
    struct stat status;
    if (fstat(fd, &status) < 0 || status.st_size < 32)
        goto failure;
    size_t size = (size_t)status.st_size;
    uint8_t *mapping = mmap(NULL, size, PROT_READ, MAP_PRIVATE, fd, 0);
    if (mapping == MAP_FAILED)
        goto failure;
    close(fd);

    uint32_t header_size = read_le32(mapping + 8);
    uint32_t flags = read_le32(mapping + 12);
    uint32_t glyph_count = read_le32(mapping + 16);
    uint32_t glyph_size = read_le32(mapping + 20);
    if (read_le32(mapping) != PSF2_MAGIC || header_size < 32 || header_size > size ||
        (flags & PSF2_HAS_UNICODE_TABLE) == 0 || read_le32(mapping + 24) != CELL_HEIGHT ||
        read_le32(mapping + 28) != CELL_WIDTH || glyph_size != CELL_HEIGHT * 2U ||
        glyph_count == 0 || glyph_count > (size - header_size) / glyph_size)
        goto invalid;

    const uint8_t *glyphs = mapping + header_size;
    const uint8_t *cursor = glyphs + (size_t)glyph_count * glyph_size;
    const uint8_t *end = mapping + size;
    for (uint32_t glyph = 0; glyph < glyph_count; ++glyph) {
        int sequence = 0;
        while (cursor < end && *cursor != 0xff) {
            if (*cursor == 0xfe) {
                sequence = 1;
                ++cursor;
                continue;
            }
            uint32_t codepoint;
            cursor = decode_utf8(cursor, end, &codepoint);
            if (!cursor)
                goto invalid;
            if (!sequence && codepoint < 128)
                font->glyphs[codepoint] = glyphs + (size_t)glyph * glyph_size;
        }
        if (cursor == end)
            goto invalid;
        ++cursor;
    }
    for (unsigned character = 0x20; character < 0x7f; ++character)
        if (!font->glyphs[character])
            goto invalid;
    font->mapping = mapping;
    font->size = size;
    return 0;

invalid:
    munmap(mapping, size);
    errno = EINVAL;
    return -1;
failure:
    close(fd);
    return -1;
}

static void clear_cell(struct terminal *terminal, size_t index) {
    terminal->cells[index] =
        (struct cell){' ', terminal->foreground, terminal->background, 1};
}

static void dirty_all_cells(struct terminal *terminal) {
    for (size_t index = 0; index < terminal->columns * terminal->rows; ++index)
        terminal->cells[index].dirty = 1;
}

static void dirty_cursor_cell(struct terminal *terminal) {
    terminal->cells[terminal->row * terminal->columns + terminal->column].dirty = 1;
}

static void clear_screen(struct terminal *terminal) {
    for (size_t index = 0; index < terminal->columns * terminal->rows; ++index)
        clear_cell(terminal, index);
    terminal->column = terminal->row = 0;
}

static int terminal_resize(struct terminal *terminal, size_t columns, size_t rows) {
    if (!columns || !rows || columns > SIZE_MAX / rows)
        return -1;
    struct cell *cells = calloc(columns * rows, sizeof(*cells));
    if (!cells)
        return -1;
    for (size_t index = 0; index < columns * rows; ++index)
        cells[index] = (struct cell){' ', terminal->foreground, terminal->background, 1};
    size_t source_row = terminal->row >= rows ? terminal->row + 1 - rows : 0;
    size_t copy_rows = rows < terminal->rows - source_row ? rows : terminal->rows - source_row;
    size_t copy_columns = columns < terminal->columns ? columns : terminal->columns;
    for (size_t row = 0; row < copy_rows; ++row)
        memcpy(cells + row * columns,
               terminal->cells + (source_row + row) * terminal->columns,
               copy_columns * sizeof(*cells));
    free(terminal->cells);
    terminal->cells = cells;
    terminal->columns = columns;
    terminal->rows = rows;
    terminal->row -= source_row;
    if (terminal->column >= columns)
        terminal->column = columns - 1;
    dirty_all_cells(terminal);
    return 0;
}

static int set_window_size(int master, size_t columns, size_t rows) {
    struct {
        uint16_t rows, columns, xpixel, ypixel;
    } size = {(uint16_t)rows, (uint16_t)columns, 0, 0};
    return ioctl(master, TIOCSWINSZ, &size);
}

static void line_feed(struct terminal *terminal) {
    terminal->column = 0;
    if (++terminal->row < terminal->rows)
        return;
    memmove(terminal->cells, terminal->cells + terminal->columns,
            (terminal->rows - 1) * terminal->columns * sizeof(*terminal->cells));
    terminal->row = terminal->rows - 1;
    for (size_t column = 0; column < terminal->columns; ++column)
        clear_cell(terminal, terminal->row * terminal->columns + column);
    dirty_all_cells(terminal);
}

static unsigned parameter(const struct terminal *terminal, unsigned index, unsigned fallback) {
    return index < terminal->parameter_count && terminal->parameters[index]
               ? terminal->parameters[index]
               : fallback;
}

static void erase_line(struct terminal *terminal, unsigned mode) {
    size_t begin = mode == 0 ? terminal->column : 0;
    size_t end = mode == 1 ? terminal->column + 1 : terminal->columns;
    for (size_t column = begin; column < end; ++column)
        clear_cell(terminal, terminal->row * terminal->columns + column);
}

static void sgr(struct terminal *terminal) {
    for (unsigned index = 0; index < terminal->parameter_count; ++index) {
        unsigned value = terminal->parameters[index];
        if (value == 0) {
            terminal->foreground = 7;
            terminal->background = 0;
        } else if (value >= 30 && value <= 37) {
            terminal->foreground = (uint8_t)(value - 30);
        } else if (value >= 40 && value <= 47) {
            terminal->background = (uint8_t)(value - 40);
        } else if (value >= 90 && value <= 97) {
            terminal->foreground = (uint8_t)(value - 90 + 8);
        } else if (value >= 100 && value <= 107) {
            terminal->background = (uint8_t)(value - 100 + 8);
        } else if (value == 1) {
            terminal->foreground |= 8;
        } else if (value == 39) {
            terminal->foreground = 7;
        } else if (value == 49) {
            terminal->background = 0;
        }
    }
}

static void execute_csi(struct terminal *terminal, uint8_t final) {
    size_t amount = parameter(terminal, 0, 1);
    switch (final) {
    case 'A': terminal->row = amount > terminal->row ? 0 : terminal->row - amount; break;
    case 'B':
        terminal->row = terminal->row + amount >= terminal->rows
                            ? terminal->rows - 1
                            : terminal->row + amount;
        break;
    case 'C':
        terminal->column = terminal->column + amount >= terminal->columns
                               ? terminal->columns - 1
                               : terminal->column + amount;
        break;
    case 'D': terminal->column = amount > terminal->column ? 0 : terminal->column - amount; break;
    case 'H':
    case 'f': {
        unsigned row = parameter(terminal, 0, 1);
        unsigned column = parameter(terminal, 1, 1);
        terminal->row = row > terminal->rows ? terminal->rows - 1 : row - 1;
        terminal->column = column > terminal->columns ? terminal->columns - 1 : column - 1;
        break;
    }
    case 'J':
        if (parameter(terminal, 0, 0) >= 2)
            clear_screen(terminal);
        else
            for (size_t index = terminal->row * terminal->columns + terminal->column;
                 index < terminal->rows * terminal->columns; ++index)
                clear_cell(terminal, index);
        break;
    case 'K': erase_line(terminal, parameter(terminal, 0, 0)); break;
    case 'm': sgr(terminal); break;
    default: break;
    }
}

static void put_character(struct terminal *terminal, uint8_t character) {
    terminal->cells[terminal->row * terminal->columns + terminal->column] =
        (struct cell){character, terminal->foreground, terminal->background, 1};
    if (++terminal->column == terminal->columns)
        line_feed(terminal);
}

static void terminal_feed(struct terminal *terminal, const uint8_t *bytes, size_t length) {
    dirty_cursor_cell(terminal);
    for (size_t index = 0; index < length; ++index) {
        uint8_t byte = bytes[index];
        if (terminal->parser == 1) {
            terminal->parser = byte == '[' ? 2 : 0;
            if (terminal->parser == 2) {
                memset(terminal->parameters, 0, sizeof(terminal->parameters));
                terminal->parameter_count = 1;
            } else if (byte == 'c') {
                terminal->foreground = 7;
                terminal->background = 0;
                clear_screen(terminal);
            }
            continue;
        }
        if (terminal->parser == 2) {
            if (byte >= '0' && byte <= '9') {
                unsigned *value = &terminal->parameters[terminal->parameter_count - 1];
                *value = *value > 100000U ? 100000U : *value * 10 + byte - '0';
            } else if (byte == ';' && terminal->parameter_count < 8) {
                ++terminal->parameter_count;
            } else if (byte != '?') {
                execute_csi(terminal, byte);
                terminal->parser = 0;
            }
            continue;
        }
        switch (byte) {
        case 0x1b: terminal->parser = 1; break;
        case '\r': terminal->column = 0; break;
        case '\n': line_feed(terminal); break;
        case '\b': if (terminal->column) --terminal->column; break;
        case '\t':
            do put_character(terminal, ' '); while (terminal->column % 8);
            break;
        default:
            if (byte >= 0x20 && byte < 0x7f)
                put_character(terminal, byte);
            else if (byte >= 0x80)
                put_character(terminal, '?');
            break;
        }
    }
    dirty_cursor_cell(terminal);
}

static void replay_boot_log(struct terminal *terminal) {
    int descriptor = open("/dev/kmsg", O_RDONLY | O_NONBLOCK | O_CLOEXEC);
    if (descriptor < 0)
        return;
    uint8_t record[256];
    for (;;) {
        ssize_t length = read(descriptor, record, sizeof(record));
        if (length < 0 && errno == EPIPE)
            continue;
        if (length <= 0)
            break;
        uint8_t *message = memchr(record, ';', (size_t)length);
        if (!message)
            continue;
        ++message;
        size_t message_length = (size_t)(record + length - message);
        if (message_length && message[message_length - 1] == '\n')
            --message_length;
        terminal_feed(terminal, message, message_length);
        terminal_feed(terminal, (const uint8_t *)"\n", 1);
    }
    close(descriptor);
}

static void render(struct terminal *terminal, struct display *display, const struct font *font) {
    for (size_t row = 0; row < terminal->rows; ++row) {
        for (size_t column = 0; column < terminal->columns; ++column) {
            struct cell *cell = &terminal->cells[row * terminal->columns + column];
            if (!cell->dirty)
                continue;
            uint8_t character = cell->character;
            const uint8_t *glyph = font->glyphs[character < 128 ? character : '?'];
            uint32_t foreground = palette[cell->foreground & 15];
            uint32_t background = palette[cell->background & 15];
            for (unsigned y = 0; y < CELL_HEIGHT; ++y) {
                uint32_t *pixels = (uint32_t *)((uint8_t *)display->pixels +
                    (row * CELL_HEIGHT + y) * display->pitch) + column * CELL_WIDTH;
                for (unsigned x = 0; x < CELL_WIDTH; ++x) {
                    int stroke = glyph[y * 2U + x / 8U] & (0x80U >> (x % 8U));
                    pixels[x] = stroke ? foreground : background;
                }
            }
            cell->dirty = 0;
        }
    }
    uint32_t *cursor = (uint32_t *)((uint8_t *)display->pixels +
        (terminal->row * CELL_HEIGHT + CELL_HEIGHT - CURSOR_HEIGHT) * display->pitch) +
        terminal->column * CELL_WIDTH;
    for (unsigned y = 0; y < CURSOR_HEIGHT; ++y) {
        for (unsigned x = 1; x < CELL_WIDTH - 1; ++x)
            cursor[x] = palette[terminal->foreground & 15];
        cursor = (uint32_t *)((uint8_t *)cursor + display->pitch);
    }
}

static int display_query_mode(const struct display *display, struct drm_mode *mode) {
    memset(mode, 0, sizeof(*mode));
    struct drm_connector connector = {
        .mode_ptr = (uintptr_t)mode,
        .count_modes = 1,
        .connector_id = display->connector_id,
    };
    if (ioctl(display->fd, DRM_IOCTL_MODE_GETCONNECTOR, &connector) < 0 ||
        !connector.count_modes || !mode->hdisplay || !mode->vdisplay)
        return -1;
    return 0;
}

static int display_set_mode(struct display *display, const struct drm_mode *mode,
                            struct terminal *terminal, const struct font *font) {
    struct drm_dumb_create create = {
        .height = mode->vdisplay,
        .width = mode->hdisplay,
        .bpp = 32,
    };
    uint32_t framebuffer_id = 0;
    void *pixels = MAP_FAILED;
    if (ioctl(display->fd, DRM_IOCTL_MODE_CREATE_DUMB, &create) < 0)
        return -1;
    struct drm_dumb_map map = {.handle = create.handle};
    if (ioctl(display->fd, DRM_IOCTL_MODE_MAP_DUMB, &map) < 0)
        goto failure;
    pixels = mmap(NULL, create.size, PROT_READ | PROT_WRITE, MAP_SHARED,
                  display->fd, (off_t)map.offset);
    if (pixels == MAP_FAILED)
        goto failure;
    struct drm_fb framebuffer = {
        .width = mode->hdisplay,
        .height = mode->vdisplay,
        .pitch = create.pitch,
        .bpp = 32,
        .depth = 24,
        .handle = create.handle,
    };
    if (ioctl(display->fd, DRM_IOCTL_MODE_ADDFB, &framebuffer) < 0)
        goto failure;
    framebuffer_id = framebuffer.fb_id;

    struct display next = {
        .fd = display->fd,
        .crtc_id = display->crtc_id,
        .connector_id = display->connector_id,
        .framebuffer_id = framebuffer_id,
        .handle = create.handle,
        .width = mode->hdisplay,
        .height = mode->vdisplay,
        .pitch = create.pitch,
        .size = create.size,
        .sequence = display->sequence,
        .pixels = pixels,
        .mode = *mode,
    };
    for (uint32_t y = 0; y < next.height; ++y) {
        uint32_t *row =
            (uint32_t *)((uint8_t *)next.pixels + (size_t)y * next.pitch);
        for (uint32_t x = 0; x < next.width; ++x)
            row[x] = palette[0];
    }
    dirty_all_cells(terminal);
    render(terminal, &next, font);

    uint32_t connector_id = display->connector_id;
    struct drm_crtc crtc = {
        .connector_ptr = (uintptr_t)&connector_id,
        .count_connectors = 1,
        .crtc_id = display->crtc_id,
        .fb_id = framebuffer_id,
        .mode_valid = 1,
        .mode = *mode,
    };
    if (ioctl(display->fd, DRM_IOCTL_MODE_SETCRTC, &crtc) < 0)
        goto failure;

    void *old_pixels = display->pixels;
    uint64_t old_size = display->size;
    uint32_t old_framebuffer = display->framebuffer_id;
    uint32_t old_handle = display->handle;
    *display = next;
    if (old_pixels) {
        munmap(old_pixels, old_size);
        (void)ioctl(display->fd, DRM_IOCTL_MODE_RMFB, &old_framebuffer);
        struct drm_dumb_destroy destroy = {.handle = old_handle};
        (void)ioctl(display->fd, DRM_IOCTL_MODE_DESTROY_DUMB, &destroy);
    }
    return 0;

failure:
    if (framebuffer_id)
        (void)ioctl(display->fd, DRM_IOCTL_MODE_RMFB, &framebuffer_id);
    if (pixels != MAP_FAILED)
        munmap(pixels, create.size);
    struct drm_dumb_destroy destroy = {.handle = create.handle};
    (void)ioctl(display->fd, DRM_IOCTL_MODE_DESTROY_DUMB, &destroy);
    return -1;
}

static int display_open(struct display *display) {
    memset(display, 0, sizeof(*display));
    display->fd = -1;
    display->fd = open("/dev/dri/card0", O_RDWR | O_CLOEXEC);
    if (display->fd < 0)
        return -1;

    uint32_t crtc_id = 0, connector_id = 0;
    struct drm_resources resources = {
        .crtc_id_ptr = (uintptr_t)&crtc_id,
        .connector_id_ptr = (uintptr_t)&connector_id,
        .count_crtcs = 1,
        .count_connectors = 1,
    };
    if (ioctl(display->fd, DRM_IOCTL_MODE_GETRESOURCES, &resources) < 0 ||
        !resources.count_crtcs || !resources.count_connectors)
        goto failure;
    display->crtc_id = crtc_id;
    display->connector_id = connector_id;
    struct drm_mode mode;
    if (display_query_mode(display, &mode) < 0)
        goto failure;
    display->width = mode.hdisplay;
    display->height = mode.vdisplay;
    display->mode = mode;
    return 0;

failure:
    if (display->pixels)
        munmap(display->pixels, display->size);
    close(display->fd);
    display->fd = -1;
    display->pixels = NULL;
    return -1;
}

static int display_present(struct display *display) {
    struct drm_flip flip = {
        .crtc_id = display->crtc_id,
        .fb_id = display->framebuffer_id,
        .flags = DRM_PAGE_FLIP_EVENT,
        .user_data = ++display->sequence,
    };
    if (ioctl(display->fd, DRM_IOCTL_MODE_PAGE_FLIP, &flip) < 0)
        return -1;
    uint8_t event[32];
    ssize_t count;
    do count = read(display->fd, event, sizeof(event)); while (count < 0 && errno == EINTR);
    return count == (ssize_t)sizeof(event) ? 0 : -1;
}

static int spawn_shell(size_t columns, size_t rows, int *master_out) {
    int master = open("/dev/ptmx", O_RDWR | O_NONBLOCK | O_CLOEXEC);
    if (master < 0)
        return -1;
    unsigned index;
    int unlocked = 0;
    if (ioctl(master, TIOCGPTN, &index) < 0 || ioctl(master, TIOCSPTLCK, &unlocked) < 0) {
        close(master);
        return -1;
    }
    char path[32];
    snprintf(path, sizeof(path), "/dev/pts/%u", index);
    int slave = open(path, O_RDWR | O_CLOEXEC);
    if (slave < 0) {
        close(master);
        return -1;
    }
    if (set_window_size(master, columns, rows) < 0) {
        close(slave);
        close(master);
        return -1;
    }

    pid_t child = fork();
    if (child < 0) {
        close(slave);
        close(master);
        return -1;
    }
    if (child == 0) {
        close(master);
        if (setsid() < 0 || ioctl(slave, TIOCSCTTY, 0) < 0 ||
            dup2(slave, STDIN_FILENO) < 0 || dup2(slave, STDOUT_FILENO) < 0 ||
            dup2(slave, STDERR_FILENO) < 0)
            _exit(126);
        if (slave > STDERR_FILENO)
            close(slave);
        setenv("TERM", "linux", 1);
        setenv("HOME", "/root", 1);
        setenv("PATH", "/sbin:/usr/sbin:/bin:/usr/bin", 1);
        (void)chdir("/root");
        execl("/bin/sh", "-sh", (char *)NULL);
        _exit(127);
    }
    close(slave);
    *master_out = master;
    return child;
}

static int contains_keyboard(const char *name) {
    const char needle[] = "keyboard";
    size_t matched = 0;
    for (; *name; ++name) {
        char value = (char)tolower((unsigned char)*name);
        matched = value == needle[matched] ? matched + 1 : value == needle[0];
        if (matched == sizeof(needle) - 1)
            return 1;
    }
    return 0;
}

static int open_keyboard(void) {
    for (unsigned index = 0; index < 16; ++index) {
        char path[32], name[128] = {0};
        snprintf(path, sizeof(path), "/dev/input/event%u", index);
        int fd = open(path, O_RDONLY | O_NONBLOCK | O_CLOEXEC);
        if (fd < 0)
            continue;
        if (ioctl(fd, EVIOCGNAME(sizeof(name)), name) >= 0 && contains_keyboard(name)) {
            (void)ioctl(fd, IOC(IOC_WRITE, 'E', 0x90, sizeof(int)), 1);
            return fd;
        }
        close(fd);
    }
    return -1;
}

static const char plain_keys[128] = {
    [2] = '1', [3] = '2', [4] = '3', [5] = '4', [6] = '5', [7] = '6',
    [8] = '7', [9] = '8', [10] = '9', [11] = '0', [12] = '-', [13] = '=',
    [16] = 'q', [17] = 'w', [18] = 'e', [19] = 'r', [20] = 't', [21] = 'y',
    [22] = 'u', [23] = 'i', [24] = 'o', [25] = 'p', [26] = '[', [27] = ']',
    [30] = 'a', [31] = 's', [32] = 'd', [33] = 'f', [34] = 'g', [35] = 'h',
    [36] = 'j', [37] = 'k', [38] = 'l', [39] = ';', [40] = '\'', [41] = '`',
    [43] = '\\', [44] = 'z', [45] = 'x', [46] = 'c', [47] = 'v', [48] = 'b',
    [49] = 'n', [50] = 'm', [51] = ',', [52] = '.', [53] = '/', [57] = ' ',
};

static const char shifted_keys[128] = {
    [2] = '!', [3] = '@', [4] = '#', [5] = '$', [6] = '%', [7] = '^',
    [8] = '&', [9] = '*', [10] = '(', [11] = ')', [12] = '_', [13] = '+',
    [26] = '{', [27] = '}', [39] = ':', [40] = '"', [41] = '~', [43] = '|',
    [51] = '<', [52] = '>', [53] = '?',
};

struct keyboard_state {
    int shift, control, alt, caps_lock;
};

static void write_key(int master, const char *bytes, size_t length) {
    while (length) {
        ssize_t count = write(master, bytes, length);
        if (count > 0) {
            bytes += count;
            length -= (size_t)count;
        } else if (count < 0 && errno == EINTR) {
            continue;
        } else {
            return;
        }
    }
}

static uint64_t monotonic_milliseconds(void) {
    struct timespec time;
    if (clock_gettime(CLOCK_MONOTONIC, &time) < 0)
        return 0;
    return (uint64_t)time.tv_sec * 1000U + (uint64_t)time.tv_nsec / 1000000U;
}

static void handle_key(int master, struct keyboard_state *state,
                       const struct input_event *event) {
    if (event->type != EV_KEY)
        return;
    int pressed = event->value != 0;
    switch (event->code) {
    case 42: case 54: state->shift = pressed; return;
    case 29: case 97: state->control = pressed; return;
    case 56: case 100: state->alt = pressed; return;
    case 58: if (event->value == 1) state->caps_lock = !state->caps_lock; return;
    default: break;
    }
    if (!pressed)
        return;
    const char *sequence = NULL;
    switch (event->code) {
    case 1: sequence = "\033"; break;
    case 14: sequence = "\177"; break;
    case 15: sequence = "\t"; break;
    case 28: sequence = "\r"; break;
    case 102: sequence = "\033[H"; break;
    case 103: sequence = "\033[A"; break;
    case 104: sequence = "\033[5~"; break;
    case 105: sequence = "\033[D"; break;
    case 106: sequence = "\033[C"; break;
    case 107: sequence = "\033[F"; break;
    case 108: sequence = "\033[B"; break;
    case 109: sequence = "\033[6~"; break;
    case 111: sequence = "\033[3~"; break;
    default: break;
    }
    if (sequence) {
        write_key(master, sequence, strlen(sequence));
        return;
    }
    if (event->code >= sizeof(plain_keys) || !plain_keys[event->code])
        return;
    char character = plain_keys[event->code];
    if (isalpha((unsigned char)character)) {
        if (state->shift != state->caps_lock)
            character = (char)toupper((unsigned char)character);
    } else if (state->shift && shifted_keys[event->code]) {
        character = shifted_keys[event->code];
    }
    if (state->control && character >= '@' && character <= '_')
        character &= 0x1f;
    else if (state->control && character >= 'a' && character <= 'z')
        character = (char)(character - 'a' + 1);
    if (state->alt)
        write_key(master, "\033", 1);
    write_key(master, &character, 1);
}

static int terminal_run(struct display *display, const struct font *font) {
    struct terminal terminal = {
        .columns = display->width / CELL_WIDTH,
        .rows = display->height / CELL_HEIGHT,
        .foreground = 7,
        .background = 0,
    };
    if (!terminal.columns || !terminal.rows)
        return -1;
    terminal.cells = calloc(terminal.columns * terminal.rows, sizeof(*terminal.cells));
    if (!terminal.cells)
        return -1;
    clear_screen(&terminal);

    int keyboard = open_keyboard();
    static const uint8_t boot_message[] =
        "\033[2J\033[HLiteOS\n\n";
    terminal_feed(&terminal, boot_message, sizeof(boot_message) - 1);
    replay_boot_log(&terminal);
    static const uint8_t display_ready[] = "[ OK ] DRM/KMS display session acquired\n";
    terminal_feed(&terminal, display_ready, sizeof(display_ready) - 1);
    static const uint8_t input_ready[] = "[ OK ] Keyboard input ready\n";
    static const uint8_t input_missing[] = "[WARN] Keyboard input unavailable\n";
    terminal_feed(&terminal, keyboard >= 0 ? input_ready : input_missing,
                  keyboard >= 0 ? sizeof(input_ready) - 1 : sizeof(input_missing) - 1);
    static const uint8_t shell_message[] = "[....] Starting shell\n\n";
    terminal_feed(&terminal, shell_message, sizeof(shell_message) - 1);
    if (display_set_mode(display, &display->mode, &terminal, font) < 0) {
        if (keyboard >= 0)
            close(keyboard);
        free(terminal.cells);
        return -1;
    }

    int master;
    pid_t child = spawn_shell(terminal.columns, terminal.rows, &master);
    if (child < 0) {
        if (keyboard >= 0)
            close(keyboard);
        free(terminal.cells);
        return -1;
    }
    struct keyboard_state keyboard_state = {0};
    struct pollfd descriptors[2] = {
        {.fd = master, .events = POLLIN},
        {.fd = keyboard, .events = POLLIN},
    };
    uint64_t next_mode_probe = monotonic_milliseconds() + MODE_PROBE_INTERVAL_MS;
    for (;;) {
        uint64_t before_poll = monotonic_milliseconds();
        uint64_t remaining = next_mode_probe > before_poll ? next_mode_probe - before_poll : 0;
        int timeout = remaining > INT_MAX ? INT_MAX : (int)remaining;
        int ready;
        do ready = poll(descriptors, 2, timeout); while (ready < 0 && errno == EINTR);
        if (ready < 0)
            break;
        if (descriptors[0].revents & (POLLIN | POLLHUP | POLLERR)) {
            uint8_t bytes[4096];
            int changed = 0, closed = 0;
            for (;;) {
                ssize_t count = read(master, bytes, sizeof(bytes));
                if (count > 0) {
                    terminal_feed(&terminal, bytes, (size_t)count);
                    changed = 1;
                } else if (count < 0 && errno == EINTR) {
                    continue;
                } else if (count < 0 && errno == EAGAIN) {
                    break;
                } else {
                    closed = 1;
                    break;
                }
            }
            if (!changed && closed)
                break;
            if (changed) {
                render(&terminal, display, font);
                if (display_present(display) < 0)
                    break;
            }
            if (closed)
                break;
        }
        if (keyboard >= 0 && descriptors[1].revents & POLLIN) {
            struct input_event events[32];
            ssize_t count = read(keyboard, events, sizeof(events));
            if (count > 0)
                for (size_t index = 0; index < (size_t)count / sizeof(events[0]); ++index)
                    handle_key(master, &keyboard_state, &events[index]);
        }
        uint64_t now = monotonic_milliseconds();
        if (now >= next_mode_probe) {
            next_mode_probe = now + MODE_PROBE_INTERVAL_MS;
            struct drm_mode mode;
            if (display_query_mode(display, &mode) < 0)
                break;
            if (mode.hdisplay != display->mode.hdisplay ||
                mode.vdisplay != display->mode.vdisplay) {
                size_t columns = mode.hdisplay / CELL_WIDTH;
                size_t rows = mode.vdisplay / CELL_HEIGHT;
                if (terminal_resize(&terminal, columns, rows) < 0)
                    break;
                if (display_set_mode(display, &mode, &terminal, font) < 0)
                    break;
                if (set_window_size(master, columns, rows) < 0)
                    break;
            }
        }
    }
    close(master);
    if (keyboard >= 0)
        close(keyboard);
    (void)waitpid(child, NULL, 0);
    free(terminal.cells);
    return -1;
}

int main(void) {
    struct font font;
    if (font_open(&font) < 0) {
        perror("liteos-terminal: font");
        return 1;
    }
    struct display display;
    if (display_open(&display) < 0) {
        perror("liteos-terminal: display");
        munmap(font.mapping, font.size);
        return 1;
    }
    int result = terminal_run(&display, &font);
    if (display.pixels)
        munmap(display.pixels, display.size);
    close(display.fd);
    munmap(font.mapping, font.size);
    return result < 0 ? 1 : 0;
}
