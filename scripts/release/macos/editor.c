#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

// The two entry points of Voiceless's macOS bridge
// (rust/src/desktop/macos/bridge.m) that deliver text: the identity of the
// focused input, and the guarded paste into that same input.
uint64_t vl_focus(char **app, char **title);
int vl_paste(const uint8_t *bytes, size_t length, uint64_t key);

// The bridge's keyboard tap calls back into the daemon for each key event.
// The fixture never runs that tap, so the callbacks it would need are empty
// here; without them the linker could not take the two entry points alone.
void vl_key(void *ctx, uint16_t key, bool down, uint8_t modifiers) {
    (void)ctx;
    (void)key;
    (void)down;
    (void)modifiers;
}

void vl_key_reset(void *ctx) {
    (void)ctx;
}

bool vl_keys_tick(void *ctx) {
    (void)ctx;
    return false;
}

void vl_keys_ready(void *ctx, bool installed) {
    (void)ctx;
    (void)installed;
}
