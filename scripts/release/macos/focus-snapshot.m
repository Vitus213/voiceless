#import <AppKit/AppKit.h>
#import <ApplicationServices/ApplicationServices.h>
#import "editor.h"

// Inspect only the foreground TextEdit document on this disposable guest.
static id readAX(AXUIElementRef element, CFStringRef name, NSMutableDictionary *log, NSString *label) {
    if (!element) return nil;
    CFTypeRef value = NULL;
    AXError status = AXUIElementCopyAttributeValue(element, name, &value);
    log[label] = @(status);
    return CFBridgingRelease(value);
}

static NSDictionary *snapshot(void) {
    @autoreleasepool {
        NSRunningApplication *front = NSWorkspace.sharedWorkspace.frontmostApplication;
        NSMutableDictionary *log = [@{@"front": front.bundleIdentifier ?: @"", @"trusted": @(AXIsProcessTrusted())} mutableCopy];
        if (![front.bundleIdentifier isEqualToString:@"com.apple.TextEdit"]) return log;
        AXUIElementRef system = AXUIElementCreateSystemWide();
        id app = readAX(system, kAXFocusedApplicationAttribute, log, @"app_error");
        id systemField = readAX(system, kAXFocusedUIElementAttribute, log, @"system_field_error");
        if (systemField && CFGetTypeID((__bridge CFTypeRef)systemField) == AXUIElementGetTypeID()) {
            log[@"system_role"] = readAX((__bridge AXUIElementRef)systemField, kAXRoleAttribute, log, @"system_role_error");
        }
        if (app && CFGetTypeID((__bridge CFTypeRef)app) == AXUIElementGetTypeID()) {
            AXUIElementRef process = (__bridge AXUIElementRef)app;
            pid_t pid = 0;
            AXUIElementGetPid(process, &pid);
            log[@"matches_front_process"] = @(pid == front.processIdentifier);
            if (pid == front.processIdentifier) {
                id window = readAX(process, kAXFocusedWindowAttribute, log, @"window_error");
                id field = readAX(process, kAXFocusedUIElementAttribute, log, @"app_field_error");
                if (window && CFGetTypeID((__bridge CFTypeRef)window) == AXUIElementGetTypeID()) {
                    log[@"title"] = readAX((__bridge AXUIElementRef)window, kAXTitleAttribute, log, @"title_error");
                }
                if (field && CFGetTypeID((__bridge CFTypeRef)field) == AXUIElementGetTypeID()) {
                    log[@"app_role"] = readAX((__bridge AXUIElementRef)field, kAXRoleAttribute, log, @"app_role_error");
                }
            }
        }
        CFRelease(system);
        log[@"bridge_confirmed_field"] = @(vl_focus(NULL, NULL) != 0);
        return log;
    }
}

int main(void) {
    @autoreleasepool {
        NSDictionary *mainThread = snapshot();
        __block NSDictionary *worker;
        dispatch_semaphore_t done = dispatch_semaphore_create(0);
        dispatch_async(dispatch_get_global_queue(QOS_CLASS_USER_INITIATED, 0), ^{
            worker = snapshot();
            dispatch_semaphore_signal(done);
        });
        if (dispatch_semaphore_wait(done, dispatch_time(DISPATCH_TIME_NOW, 6 * NSEC_PER_SEC))) return 2;
        NSData *data = [NSJSONSerialization dataWithJSONObject:@{@"main":mainThread, @"worker":worker}
                                                       options:0 error:NULL];
        puts([[NSString alloc] initWithData:data encoding:NSUTF8StringEncoding].UTF8String);
    }
    return 0;
}
