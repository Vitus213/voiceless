// Operate visible WKWebView controls through the installed application's accessibility tree.
#import <AppKit/AppKit.h>
#import <ApplicationServices/ApplicationServices.h>

static id attribute(AXUIElementRef element, CFStringRef key) {
    CFTypeRef value = NULL;
    if (AXUIElementCopyAttributeValue(element, key, &value) != kAXErrorSuccess) return nil;
    return CFBridgingRelease(value);
}

int main(int argc, const char **argv) {
    @autoreleasepool {
        if (argc < 3 || !AXIsProcessTrusted()) return 2;
        AXUIElementRef app = AXUIElementCreateApplication(atoi(argv[1]));
        NSString *operation = @(argv[2]);
        NSMutableArray *pending = [NSMutableArray arrayWithArray:attribute(app, kAXWindowsAttribute) ?: @[]];
        CFRelease(app);
        NSMutableSet *seen = [NSMutableSet set];
        NSMutableArray *labels = [NSMutableArray array];
        BOOL dump = [operation isEqualToString:@"dump"];
        if (!dump && argc < 5) return 2;
        while (pending.count && seen.count < 2000) {
            id item = pending.lastObject;
            [pending removeLastObject];
            if ([seen containsObject:item]) continue;
            [seen addObject:item];
            AXUIElementRef element = (__bridge AXUIElementRef)item;
            NSString *role = attribute(element, kAXRoleAttribute) ?: @"";
            NSArray *children = attribute(element, kAXChildrenAttribute);
            if (children) [pending addObjectsFromArray:children];
            if (!dump && ![role isEqualToString:@(argv[3])]) continue;
            NSMutableArray *names = [NSMutableArray array];
            for (id name in @[attribute(element, kAXTitleAttribute) ?: @"",
                              attribute(element, kAXDescriptionAttribute) ?: @"",
                              attribute(element, kAXValueAttribute) ?: @""]) {
                if ([name isKindOfClass:NSString.class] && [name length]) [names addObject:name];
            }
            if (dump) {
                if (names.count || [role isEqualToString:@"AXWebArea"])
                    [labels addObject:@[role, names]];
                continue;
            }
            BOOL matches = NO;
            for (NSString *name in names) for (int i = 4; i < argc; i++)
                if ([name localizedCaseInsensitiveContainsString:@(argv[i])]) matches = YES;
            if (!matches) continue;
            if ([operation isEqualToString:@"value"]) {
                id value = attribute(element, kAXValueAttribute);
                if ([value isKindOfClass:NSString.class]) fputs([value UTF8String], stdout);
                return 0;
            }
            if ([operation isEqualToString:@"seen"]) return 0;
            NSNumber *enabled = attribute(element, kAXEnabledAttribute);
            if (enabled && !enabled.boolValue) return 1;
            if ([operation isEqualToString:@"press"])
                return AXUIElementPerformAction(element, kAXPressAction) == kAXErrorSuccess ? 0 : 1;
            return 2;
        }
        if (dump) {
            NSData *json = [NSJSONSerialization dataWithJSONObject:labels options:0 error:nil];
            fwrite(json.bytes, 1, json.length, stdout);
            return 0;
        }
        return 1;
    }
}
