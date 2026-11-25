#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

// The two entry points of Voiceless's macOS bridge
// (rust/src/desktop/macos/bridge.m) that deliver text: the identity of the
// focused input, and the guarded paste into that same input.
uint64_t vl_focus(char **app, char **title);
int vl_paste(const uint8_t *bytes, size_t length, uint64_t key);

// The keyboard tap of the bridge calls back into the daemon for each key
// event. Empty here: the fixture never runs that tap, and without them the
// linker could not take the two entry points alone from the bridge's archive.
void vl_key(void *ctx, uint16_t key, bool down, uint8_t modifiers);
void vl_key_reset(void *ctx);
bool vl_keys_tick(void *ctx);
void vl_keys_ready(void *ctx, bool installed);
