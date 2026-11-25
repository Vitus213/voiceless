#include "editor.h"

// The keyboard tap of the bridge calls back into the daemon for each key
// event. Empty here: the fixture never runs that tap, and without them the
// linker could not take the two entry points alone from the bridge's archive.

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
