// AX window-identity probe (#779). Answers ONE question: for a REAL foreign app
// window, which element->CGWindowID mapping primitives actually resolve?
//
//   _AXUIElementGetWindow   (private, via dlsym)   <- what the #779 fix will use
//   AXWindowNumber          (AX attribute)         <- what remote_control.rs uses today
//
// and cross-checks both against the CGWindowList ids for the same pid, which is
// where remote_control's `window_id` actually comes from. A mapping that resolves
// but disagrees with CGWindowList is just as broken as one that fails.
//
// ⚠ DEGRADED-TRUST GUARD. Under inherited (responsible-process) Accessibility
// trust, AXIsProcessTrusted() returns a FALSE-POSITIVE true while every element
// in kAXWindows collapses to the APPLICATION element (role AXApplication) --
// see platform/ax.rs:181 and internal/docs/WINDOW_REGISTRY_PLAN.md §9.14. In that
// state both primitives fail for reasons that have nothing to do with #779, so
// this probe reports the role of every element and refuses to render a verdict
// when it sees the degraded shape. Without this guard the probe would confidently
// report the wrong answer.
//
//   clang -fobjc-arc -framework Cocoa -framework ApplicationServices -o /tmp/axwinid scripts/probes/axwinid.m
//   /tmp/axwinid <pid> [<pid> ...]      # or: /tmp/axwinid --name iTerm2
#import <Cocoa/Cocoa.h>
#import <ApplicationServices/ApplicationServices.h>
#import <dlfcn.h>

typedef AXError (*AxGetWindowFn)(AXUIElementRef, CGWindowID *);

static NSString *roleOf(AXUIElementRef el) {
    CFTypeRef role = NULL;
    if (AXUIElementCopyAttributeValue(el, kAXRoleAttribute, &role) == kAXErrorSuccess && role) {
        NSString *r = [(__bridge NSString *)role copy];
        CFRelease(role);
        return r;
    }
    return @"<unreadable>";
}

static NSString *titleOf(AXUIElementRef el) {
    CFTypeRef t = NULL;
    if (AXUIElementCopyAttributeValue(el, kAXTitleAttribute, &t) == kAXErrorSuccess && t) {
        NSString *s = [(__bridge NSString *)t copy];
        CFRelease(t);
        return s.length ? s : @"<empty>";
    }
    return @"<none>";
}

// CGWindowList ids for a pid — the id space remote_control actually authorizes in.
// `onScreenOnly` matters: an off-screen/minimised window is absent from the
// on-screen list, so cross-checking against that list alone reports a false
// mismatch for a mapping that is actually correct. We check both lists and only
// call it a real mismatch when the id is in NEITHER.
static NSArray<NSNumber *> *cgWindowIdsForPid(pid_t pid, BOOL onScreenOnly) {
    NSMutableArray *out = [NSMutableArray array];
    CGWindowListOption opt = kCGWindowListExcludeDesktopElements
        | (onScreenOnly ? kCGWindowListOptionOnScreenOnly : kCGWindowListOptionAll);
    CFArrayRef list = CGWindowListCopyWindowInfo(opt, kCGNullWindowID);
    if (!list) return out;
    for (NSDictionary *w in (__bridge NSArray *)list) {
        if ([w[(id)kCGWindowOwnerPID] intValue] == pid) {
            [out addObject:w[(id)kCGWindowNumber]];
        }
    }
    CFRelease(list);
    return out;
}

int main(int argc, const char *argv[]) { @autoreleasepool {
    NSMutableArray<NSNumber *> *pids = [NSMutableArray array];
    if (argc >= 3 && strcmp(argv[1], "--name") == 0) {
        NSString *want = [NSString stringWithUTF8String:argv[2]];
        for (NSRunningApplication *a in NSWorkspace.sharedWorkspace.runningApplications) {
            if ([a.localizedName isEqualToString:want]) [pids addObject:@(a.processIdentifier)];
        }
        if (!pids.count) { printf("no running app named %s\n", argv[2]); return 2; }
    } else if (argc >= 2) {
        for (int i = 1; i < argc; i++) [pids addObject:@(atoi(argv[i]))];
    } else {
        printf("usage: axwinid <pid>... | --name <AppName>\n");
        return 2;
    }

    AxGetWindowFn getWindow = (AxGetWindowFn)dlsym(RTLD_DEFAULT, "_AXUIElementGetWindow");
    printf("AXIsProcessTrusted()          = %s\n", AXIsProcessTrusted() ? "true" : "false");
    printf("_AXUIElementGetWindow symbol  = %s\n", getWindow ? "RESOLVED" : "MISSING");
    printf("  (a false-positive trust=true is possible under inherited trust -- see role column)\n\n");

    int totalWindows = 0, getWindowOk = 0, axNumberOk = 0, cgMatch = 0, degraded = 0, nonWindow = 0;

    for (NSNumber *p in pids) {
        pid_t pid = (pid_t)p.intValue;
        NSArray<NSNumber *> *cgIds = cgWindowIdsForPid(pid, YES);
        NSArray<NSNumber *> *cgIdsAll = cgWindowIdsForPid(pid, NO);
        printf("=== pid %d ===\n", pid);
        printf("CGWindowList ids (on-screen): %s\n", cgIds.count ? cgIds.description.UTF8String : "<none>");

        AXUIElementRef app = AXUIElementCreateApplication(pid);
        if (!app) { printf("  AXUIElementCreateApplication failed\n\n"); continue; }
        CFTypeRef wins = NULL;
        AXError e = AXUIElementCopyAttributeValue(app, kAXWindowsAttribute, &wins);
        if (e != kAXErrorSuccess || !wins) {
            printf("  kAXWindows read FAILED (AXError %d) -- app serves no AX windows\n\n", (int)e);
            CFRelease(app);
            continue;
        }
        NSArray *windows = (__bridge NSArray *)wins;
        for (NSUInteger i = 0; i < windows.count; i++) {
            AXUIElementRef w = (__bridge AXUIElementRef)windows[i];
            totalWindows++;
            NSString *role = roleOf(w);
            BOOL isDegraded = [role isEqualToString:@"AXApplication"];
            if (isDegraded) degraded++;
            // kAXWindows is NOT guaranteed to contain only AXWindow elements:
            // Finder serves its desktop as an AXScrollArea here. Those are not
            // windows and have no CGWindowID -- scoring them as identity
            // failures produces a false FAIL. Counted separately; the fix must
            // skip non-AXWindow elements rather than treat them as a mismatch.
            BOOL isWindow = [role isEqualToString:@"AXWindow"];
            if (!isWindow && !isDegraded) nonWindow++;

            // primitive 1: the private symbol
            NSString *gw = @"n/a (symbol missing)";
            CGWindowID wid = 0;
            if (getWindow) {
                AXError ge = getWindow(w, &wid);
                if (ge == kAXErrorSuccess) { gw = [NSString stringWithFormat:@"%u", wid]; if (isWindow) getWindowOk++; }
                else gw = [NSString stringWithFormat:@"FAILED(AXError %d)", (int)ge];
            }

            // primitive 2: the attribute remote_control.rs uses today
            NSString *axn = @"FAILED(absent)";
            CFTypeRef num = NULL;
            AXError ne = AXUIElementCopyAttributeValue(w, CFSTR("AXWindowNumber"), &num);
            if (ne == kAXErrorSuccess && num) {
                long long v = 0;
                CFNumberGetValue((CFNumberRef)num, kCFNumberLongLongType, &v);
                axn = [NSString stringWithFormat:@"%lld", v];
                if (isWindow) axNumberOk++;
                CFRelease(num);
            } else {
                axn = [NSString stringWithFormat:@"FAILED(AXError %d)", (int)ne];
            }

            BOOL inCg = wid && [cgIds containsObject:@(wid)];
            BOOL inCgAll = wid && [cgIdsAll containsObject:@(wid)];
            if (inCgAll && isWindow) cgMatch++;
            const char *cgState = inCg ? "YES" : (inCgAll ? "offscreen" : "NOT-FOUND");
            printf("  win[%lu] role=%-14s title=%-28s _AXUIElementGetWindow=%-18s AXWindowNumber=%-18s inCGWindowList=%s\n",
                   (unsigned long)i, role.UTF8String,
                   [titleOf(w) substringToIndex:MIN(28u, (unsigned)titleOf(w).length)].UTF8String,
                   gw.UTF8String, axn.UTF8String, cgState);
        }
        CFRelease(wins);
        CFRelease(app);
        printf("\n");
    }

    int realWindows = totalWindows - nonWindow - degraded;
    printf("=== VERDICT ===\n");
    printf("elements inspected             : %d (real AXWindow: %d, non-window: %d)\n", totalWindows, realWindows, nonWindow);
    printf("_AXUIElementGetWindow resolved : %d/%d real windows\n", getWindowOk, realWindows);
    printf("  ...and matched CGWindowList  : %d/%d real windows\n", cgMatch, realWindows);
    printf("AXWindowNumber resolved        : %d/%d real windows\n", axNumberOk, realWindows);
    if (degraded > 0) {
        printf("\n!! DEGRADED TRUST: %d/%d elements had role AXApplication.\n", degraded, totalWindows);
        printf("!! kAXWindows collapsed to app-element copies -- this process does NOT have\n");
        printf("!! direct Accessibility trust. NO VERDICT: rerun from a directly-granted\n");
        printf("!! binary. Both primitives fail here for reasons unrelated to #779.\n");
        return 3;
    }
    if (realWindows == 0) { printf("\nNO VERDICT: no real AXWindow elements served.\n"); return 3; }
    printf("\n%s\n", (getWindowOk == realWindows && cgMatch == realWindows)
        ? "PASS: _AXUIElementGetWindow is a sound identity primitive in the CGWindowList id space."
        : "FAIL: _AXUIElementGetWindow did NOT resolve+match for every window -- the #779 fix\n      cannot rely on it alone and needs the (pid,frame) correlation fallback.");
    return 0;
} }
