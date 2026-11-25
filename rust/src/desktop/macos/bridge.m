#import <AppKit/AppKit.h>
#import <AVFoundation/AVFoundation.h>
#import <ApplicationServices/ApplicationServices.h>
#import <CoreAudio/CoreAudio.h>
#import <CoreAudio/AudioHardwareTapping.h>
#import <CoreAudio/CATapDescription.h>
#import <stdbool.h>
#import <string.h>
#import <unistd.h>

char *vl_audio_label(const char *uid, bool output) {
    if (output) { if (@available(macOS 14.2, *)) {} else { return NULL; } }
    @autoreleasepool {
        NSString *name = [NSString stringWithUTF8String:uid];
        CFStringRef key = (__bridge CFStringRef)name;
        AudioDeviceID device = kAudioObjectUnknown;
        UInt32 size = sizeof(device);
        AudioObjectPropertyAddress prop = { kAudioHardwarePropertyTranslateUIDToDevice,
            kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain };
        if (AudioObjectGetPropertyData(kAudioObjectSystemObject, &prop, sizeof(key), &key,
                &size, &device) != noErr || device == kAudioObjectUnknown) return NULL;
        prop.mSelector = kAudioDevicePropertyStreams;
        prop.mScope = output ? kAudioObjectPropertyScopeOutput : kAudioObjectPropertyScopeInput;
        if (AudioObjectGetPropertyDataSize(device, &prop, 0, NULL, &size) != noErr ||
                size < sizeof(AudioStreamID)) return NULL;
        prop.mScope = kAudioObjectPropertyScopeGlobal;
        prop.mSelector = kAudioObjectPropertyName;
        CFStringRef label = NULL;
        size = sizeof(label);
        if (AudioObjectGetPropertyData(device, &prop, 0, NULL, &size, &label) != noErr) return strdup(uid);
        NSString *value = CFBridgingRelease(label);
        return strdup(value.UTF8String ?: uid);
    }
}

// A duplex output must tap playback rather than opening its microphone side.
OSStatus vl_meeting_tap(const char *output, const char *aggregateUID,
                        AudioObjectID *tap, AudioObjectID *aggregate) {
    if (@available(macOS 14.2, *)) { @autoreleasepool {
        CATapDescription *desc = [[CATapDescription alloc] initWithProcesses:@[]
            andDeviceUID:@(output) withStream:0];
        desc.exclusive = YES;
        desc.privateTap = YES;
        desc.name = @"Voiceless meeting";
        OSStatus status = AudioHardwareCreateProcessTap(desc, tap);
        if (status != noErr) return status;
        NSDictionary *properties = @{
            @kAudioAggregateDeviceNameKey: desc.name,
            @kAudioAggregateDeviceUIDKey: @(aggregateUID),
            @kAudioAggregateDeviceIsPrivateKey: @YES,
            @kAudioAggregateDeviceTapAutoStartKey: @YES,
            @kAudioAggregateDeviceTapListKey: @[@{@kAudioSubTapUIDKey: desc.UUID.UUIDString}]
        };
        status = AudioHardwareCreateAggregateDevice((__bridge CFDictionaryRef)properties, aggregate);
        if (status != noErr) AudioHardwareDestroyProcessTap(*tap);
        return status;
    } }
    return kAudioHardwareUnsupportedOperationError;
}

void vl_close_meeting_tap(AudioObjectID tap, AudioObjectID aggregate) {
    if (@available(macOS 14.2, *)) {
        AudioHardwareDestroyAggregateDevice(aggregate);
        AudioHardwareDestroyProcessTap(tap);
    }
}

NSInteger vl_mic_permission(bool request) {
    @autoreleasepool {
        AVAuthorizationStatus status = [AVCaptureDevice authorizationStatusForMediaType:AVMediaTypeAudio];
        if (request && status == AVAuthorizationStatusNotDetermined) {
            NSDictionary *session = CFBridgingRelease(CGSessionCopyCurrentDictionary());
            if ([session[@"CGSSessionScreenIsLocked"] boolValue]) return -2;
            dispatch_semaphore_t answered = dispatch_semaphore_create(0);
            [AVCaptureDevice requestAccessForMediaType:AVMediaTypeAudio completionHandler:^(BOOL granted) {
                dispatch_semaphore_signal(answered);
            }];
            if (dispatch_semaphore_wait(answered, dispatch_time(DISPATCH_TIME_NOW, 30 * NSEC_PER_SEC))) return -3;
            status = [AVCaptureDevice authorizationStatusForMediaType:AVMediaTypeAudio];
        }
        return status;
    }
}

bool vl_listen_permission(bool request) {
    return CGPreflightListenEventAccess() || (request && CGRequestListenEventAccess());
}

bool vl_accessibility(bool request) {
    @autoreleasepool {
        if (!request) return AXIsProcessTrusted();
        return AXIsProcessTrustedWithOptions((__bridge CFDictionaryRef)@{
            (__bridge NSString *)kAXTrustedCheckOptionPrompt: @YES
        });
    }
}

static id attribute(AXUIElementRef element, CFStringRef name) {
    CFTypeRef value = NULL;
    if (AXUIElementCopyAttributeValue(element, name, &value) != kAXErrorSuccess) return nil;
    return CFBridgingRelease(value);
}

// Only one snapshot is needed. AX objects retain their identity even when a
// document title changes. Replacing the window or its focused field invalidates
// the token. Copy display strings only when the caller requests them.
static uint64_t currentFocus(pid_t *pid, char **appID, char **title) {
    static id previousWindow, previousField;
    static pid_t previousPID;
    static uint64_t generation;
    if (!AXIsProcessTrusted()) return 0;
    id system = CFBridgingRelease(AXUIElementCreateSystemWide());
    CFTypeRef appValue = NULL;
    AXError status = AXUIElementCopyAttributeValue((__bridge AXUIElementRef)system,
        kAXFocusedApplicationAttribute, &appValue);
    id app = CFBridgingRelease(appValue);
    if (status == kAXErrorCannotComplete) {
        // System-wide AX can be unavailable even when TextEdit exposes its focused field.
        NSRunningApplication *front = NSWorkspace.sharedWorkspace.frontmostApplication;
        if (!front || front.processIdentifier <= 0) return 0;
        app = CFBridgingRelease(AXUIElementCreateApplication(front.processIdentifier));
    } else if (status != kAXErrorSuccess) {
        return 0;
    }
    if (!app || CFGetTypeID((__bridge CFTypeRef)app) != AXUIElementGetTypeID()) return 0;
    AXUIElementRef ax = (__bridge AXUIElementRef)app;
    if (AXUIElementGetPid(ax, pid) != kAXErrorSuccess || *pid <= 0) return 0;
    id window = attribute(ax, kAXFocusedWindowAttribute);
    id field = attribute(ax, kAXFocusedUIElementAttribute);
    if (!window || !field || CFGetTypeID((__bridge CFTypeRef)field) != AXUIElementGetTypeID()
        || CFGetTypeID((__bridge CFTypeRef)window) != AXUIElementGetTypeID()) return 0;
    if ([attribute((__bridge AXUIElementRef)field, kAXSubroleAttribute) isEqual:@"AXSecureTextField"]) return 0;
    if (status == kAXErrorCannotComplete && ![attribute(ax, kAXFrontmostAttribute) isEqual:@YES]) return 0;
    if (*pid != previousPID || !previousWindow || !previousField ||
        !CFEqual((__bridge CFTypeRef)window, (__bridge CFTypeRef)previousWindow) ||
        !CFEqual((__bridge CFTypeRef)field, (__bridge CFTypeRef)previousField)) {
        previousPID = *pid;
        previousWindow = window;
        previousField = field;
        generation++;
    }
    if (appID) {
        NSString *bundle = [NSRunningApplication runningApplicationWithProcessIdentifier:*pid].bundleIdentifier;
        if (bundle.UTF8String) *appID = strdup(bundle.UTF8String);
    }
    if (title) {
        id value = attribute((__bridge AXUIElementRef)window, kAXTitleAttribute);
        if ([value isKindOfClass:NSString.class] && [value UTF8String]) *title = strdup([value UTF8String]);
    }
    return generation;
}

uint64_t vl_focus(char **app, char **title) {
    @autoreleasepool { @synchronized (NSWorkspace.class) {
        pid_t pid;
        return currentFocus(&pid, app, title);
    }}
}

static NSString *string(const uint8_t *bytes, size_t length) {
    return [[NSString alloc] initWithBytes:bytes length:length encoding:NSUTF8StringEncoding];
}

bool vl_copy(const uint8_t *bytes, size_t length) {
    @autoreleasepool { @synchronized (NSPasteboard.class) {
        NSPasteboard *board = NSPasteboard.generalPasteboard;
        [board clearContents];
        return [board setString:string(bytes, length) forType:NSPasteboardTypeString];
    }}
}

int vl_paste(const uint8_t *bytes, size_t length, uint64_t key) {
    if (key == 0) return 1; // No confirmed field, including a secure text field.
    @autoreleasepool { @synchronized (NSPasteboard.class) { @synchronized (NSWorkspace.class) {
        if (!AXIsProcessTrusted() || !CGPreflightPostEventAccess()) return 2;
        pid_t pid;
        if (currentFocus(&pid, NULL, NULL) != key) return 1;
        CGEventRef down = CGEventCreateKeyboardEvent(NULL, 9, true); // ANSI V
        CGEventRef up = CGEventCreateKeyboardEvent(NULL, 9, false);
        if (!down || !up) {
            if (down) CFRelease(down);
            if (up) CFRelease(up);
            return 3;
        }
        CGEventSetFlags(down, kCGEventFlagMaskCommand);
        CGEventSetFlags(up, kCGEventFlagMaskCommand);
        NSPasteboard *board = NSPasteboard.generalPasteboard;
        NSInteger original = board.changeCount;
        NSMutableArray *saved = [NSMutableArray array];
        for (NSPasteboardItem *item in board.pasteboardItems) {
            NSPasteboardItem *copy = [NSPasteboardItem new];
            for (NSString *type in item.types) {
                NSData *data = [item dataForType:type];
                if (!data || ![copy setData:data forType:type]) {
                    CFRelease(down); CFRelease(up);
                    return 3;
                }
            }
            [saved addObject:copy];
        }
        int result = 3;
        if (board.changeCount == original) {
            NSInteger owned = [board clearContents];
            if ([board setString:string(bytes, length) forType:NSPasteboardTypeString]) {
                owned = board.changeCount;
                result = 1;
                if (currentFocus(&pid, NULL, NULL) == key && board.changeCount == owned) {
                    CGEventPostToPid(pid, down);
                    CGEventPostToPid(pid, up);
                    [NSThread sleepForTimeInterval:0.20];
                    result = 0; // Posted; the target provides no paste acknowledgement.
                }
            }
            if (board.changeCount == owned) {
                [board clearContents];
                if (saved.count) [board writeObjects:saved];
            }
        }
        CFRelease(down); CFRelease(up);
        return result;
    }}}
}

// The run-loop thread is the sole owner of both the tap and its Rust context.
extern void vl_key(void *ctx, uint16_t key, bool down, uint8_t modifiers);
extern void vl_key_reset(void *ctx);
extern bool vl_keys_tick(void *ctx);
extern void vl_keys_ready(void *ctx, bool installed);

struct Tap { void *ctx; CFMachPortRef port; };
static CGEventRef observe(CGEventTapProxy proxy, CGEventType type, CGEventRef event, void *info) {
    struct Tap *tap = info;
    if (type == kCGEventTapDisabledByTimeout || type == kCGEventTapDisabledByUserInput) {
        vl_key_reset(tap->ctx);
        CGEventTapEnable(tap->port, true);
        return event;
    }
    if (CGEventGetIntegerValueField(event, kCGEventSourceUnixProcessID) == getpid()) return event;
    uint16_t key = CGEventGetIntegerValueField(event, kCGKeyboardEventKeycode);
    CGEventFlags flags = CGEventGetFlags(event);
    if (CGEventGetIntegerValueField(event, kCGKeyboardEventAutorepeat)) return event;
    bool down = type == kCGEventFlagsChanged ? CGEventSourceKeyState(kCGEventSourceStateHIDSystemState, key) : type == kCGEventKeyDown;
    uint8_t modifiers = ((flags & kCGEventFlagMaskControl) ? 1 : 0) | ((flags & kCGEventFlagMaskAlternate) ? 2 : 0)
        | ((flags & kCGEventFlagMaskShift) ? 4 : 0) | ((flags & kCGEventFlagMaskCommand) ? 8 : 0);
    vl_key(tap->ctx, key, down, modifiers);
    return event;
}

void vl_run_keys(void *ctx) {
    @autoreleasepool {
        struct Tap tap = { .ctx = ctx };
        CGEventMask mask = CGEventMaskBit(kCGEventKeyDown) | CGEventMaskBit(kCGEventKeyUp) | CGEventMaskBit(kCGEventFlagsChanged);
        tap.port = CGEventTapCreate(kCGSessionEventTap, kCGHeadInsertEventTap, kCGEventTapOptionListenOnly, mask, observe, &tap);
        CFRunLoopSourceRef source = tap.port ? CFMachPortCreateRunLoopSource(NULL, tap.port, 0) : NULL;
        vl_keys_ready(ctx, source != NULL);
        if (source) {
            CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopDefaultMode);
            while (vl_keys_tick(ctx)) {
                @autoreleasepool { CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.05, true); }
            }
            CFRunLoopRemoveSource(CFRunLoopGetCurrent(), source, kCFRunLoopDefaultMode);
            CFRelease(source);
        }
        if (tap.port) { CFMachPortInvalidate(tap.port); CFRelease(tap.port); }
    }
}
