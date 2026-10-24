/* 用上游 libxkbcommon 裁决合成器发出的 keymap：必须自包含（禁用默认 include）、可编译，
 * 且 keysym、修饰键索引与 wl_keyboard.modifiers 掩码的假设一致。 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <xkbcommon/xkbcommon.h>

#define EVDEV(code) ((code) + 8)

static int failures;

#define CHECK(condition, message) \
    do { if (!(condition)) { fprintf(stderr, "xkb-check: FAIL: %s\n", message); failures++; } } while (0)

static xkb_keysym_t sym(struct xkb_state *state, int evdev) {
    return xkb_state_key_get_one_sym(state, EVDEV(evdev));
}

static void tap(struct xkb_state *state, int evdev) {
    xkb_state_update_key(state, EVDEV(evdev), XKB_KEY_DOWN);
    xkb_state_update_key(state, EVDEV(evdev), XKB_KEY_UP);
}

int main(int argc, char **argv) {
    if (argc != 2) { fprintf(stderr, "usage: xkb-check <keymap file>\n"); return 2; }
    FILE *file = fopen(argv[1], "r");
    if (!file) { perror("xkb-check: open"); return 2; }
    static char text[1 << 16];
    size_t length = fread(text, 1, sizeof text - 1, file);
    fclose(file);
    text[length] = 0;

    struct xkb_context *context = xkb_context_new(XKB_CONTEXT_NO_DEFAULT_INCLUDES);
    struct xkb_keymap *keymap = xkb_keymap_new_from_string(context, text, XKB_KEYMAP_FORMAT_TEXT_V1, XKB_KEYMAP_COMPILE_NO_FLAGS);
    if (!keymap) { fprintf(stderr, "xkb-check: the keymap does not compile\n"); return 1; }
    struct xkb_state *state = xkb_state_new(keymap);

    /* 修饰键索引必须与 keyboard::xkb::modifier_masks 的位一致。 */
    CHECK(xkb_keymap_mod_get_index(keymap, XKB_MOD_NAME_SHIFT) == 0, "Shift is modifier 0");
    CHECK(xkb_keymap_mod_get_index(keymap, XKB_MOD_NAME_CAPS) == 1, "Lock is modifier 1");
    CHECK(xkb_keymap_mod_get_index(keymap, XKB_MOD_NAME_CTRL) == 2, "Control is modifier 2");
    CHECK(xkb_keymap_mod_get_index(keymap, XKB_MOD_NAME_ALT) == 3, "Mod1 is modifier 3");
    CHECK(xkb_keymap_mod_get_index(keymap, XKB_MOD_NAME_LOGO) == 6, "Mod4 is modifier 6");
    CHECK(xkb_keymap_num_layouts(keymap) == 1, "one layout");

    CHECK(sym(state, 16) == XKB_KEY_q, "key 16 is q");
    CHECK(sym(state, 2) == XKB_KEY_1, "key 2 is 1");
    CHECK(sym(state, 57) == XKB_KEY_space, "key 57 is space");
    CHECK(sym(state, 28) == XKB_KEY_Return, "key 28 is Return");
    CHECK(sym(state, 105) == XKB_KEY_Left, "key 105 is Left");
    CHECK(sym(state, 62) == XKB_KEY_F4, "key 62 is F4");
    CHECK(sym(state, 43) == XKB_KEY_backslash, "key 43 is backslash");

    /* Shift：按住期间字母变大写、数字行变符号；释放后恢复。 */
    xkb_state_update_key(state, EVDEV(42), XKB_KEY_DOWN);
    CHECK(sym(state, 16) == XKB_KEY_Q, "shift+q is Q");
    CHECK(sym(state, 2) == XKB_KEY_exclam, "shift+1 is exclam");
    CHECK(sym(state, 43) == XKB_KEY_bar, "shift+backslash is bar");
    CHECK(sym(state, 15) == XKB_KEY_ISO_Left_Tab, "shift+tab is ISO_Left_Tab");
    CHECK(xkb_state_mod_name_is_active(state, XKB_MOD_NAME_SHIFT, XKB_STATE_MODS_DEPRESSED), "shift is depressed");
    xkb_state_update_key(state, EVDEV(42), XKB_KEY_UP);
    CHECK(sym(state, 16) == XKB_KEY_q, "q after releasing shift");

    /* Caps Lock 锁定字母而不影响数字行。 */
    tap(state, 58);
    CHECK(sym(state, 16) == XKB_KEY_Q, "caps lock makes q uppercase");
    CHECK(sym(state, 2) == XKB_KEY_1, "caps lock leaves the digit row alone");
    CHECK(xkb_state_mod_name_is_active(state, XKB_MOD_NAME_CAPS, XKB_STATE_MODS_LOCKED), "caps is locked");
    tap(state, 58);
    CHECK(sym(state, 16) == XKB_KEY_q, "caps lock toggles back");

    /* 其余修饰键：Control、Alt、Super。 */
    xkb_state_update_key(state, EVDEV(29), XKB_KEY_DOWN);
    CHECK(xkb_state_mod_name_is_active(state, XKB_MOD_NAME_CTRL, XKB_STATE_MODS_DEPRESSED), "control is depressed");
    xkb_state_update_key(state, EVDEV(29), XKB_KEY_UP);
    xkb_state_update_key(state, EVDEV(100), XKB_KEY_DOWN);
    CHECK(xkb_state_mod_name_is_active(state, XKB_MOD_NAME_ALT, XKB_STATE_MODS_DEPRESSED), "right alt is Mod1");
    xkb_state_update_key(state, EVDEV(100), XKB_KEY_UP);
    xkb_state_update_key(state, EVDEV(125), XKB_KEY_DOWN);
    CHECK(xkb_state_mod_name_is_active(state, XKB_MOD_NAME_LOGO, XKB_STATE_MODS_DEPRESSED), "super is Mod4");
    xkb_state_update_key(state, EVDEV(125), XKB_KEY_UP);

    /* 文本转换。 */
    char utf8[8];
    xkb_state_key_get_utf8(state, EVDEV(30), utf8, sizeof utf8);
    CHECK(strcmp(utf8, "a") == 0, "key 30 produces a");

    xkb_state_unref(state);
    xkb_keymap_unref(keymap);
    xkb_context_unref(context);
    if (failures) return 1;
    puts("XKB_OK");
    return 0;
}
