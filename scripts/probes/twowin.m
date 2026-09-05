// Two-sibling-window fixture for the #779 AX window-identity regression guard.
//
// The guard (`real_ax_window_identity_accepts_exact_window_and_refuses_same_app_sibling`
// in remote_control.rs) needs ONE application that is serving at least two
// visible, layer-0, real sibling windows with real AX descendants, so the
// production identity resolver can be asked to tell them apart. Before this
// fixture existed the guard depended on whatever the operator happened to have
// open -- and quietly SKIPPED when nothing qualified, which is how a guard for
// a shipped P0 fix ends up proving nothing.
//
// Sibling of scripts/probes/onewin.m; opens TWO titled windows instead of one
// and prints both CGWindowIDs plus this process's pid on stderr in a single
// machine-readable line. Driven by scripts/verify-rc-window-identity.sh.
//
//   clang -fobjc-arc -framework Cocoa -o twowin twowin.m
//
// Env:
//   PROBE_SPOT_X / PROBE_SPOT_Y  top-left of the first window in CG (flipped)
//                                screen coordinates; default matches the
//                                2560-wide dev rig's empty region.
//   TWOWIN_SECONDS               how long to stay up (default 300). The runner
//                                kills it by PID as soon as the guard has run,
//                                so this is only a backstop against an
//                                orphaned fixture -- never rely on it.
#import <Cocoa/Cocoa.h>

@interface TwoWin : NSObject
@end

@implementation TwoWin {
    NSWindow *_a;
    NSWindow *_b;
}

// Titled, resizable, normal-level, comfortably larger than the registry's
// 40pt "is_real" floor, and each carrying a real subview so the window's
// AXChildren array is never empty (the guard forces the production frame
// fallback through a descendant, so a childless window is not a usable
// fixture).
- (NSWindow *)makeWindowAt:(NSRect)rect title:(NSString *)title color:(NSColor *)color {
    NSWindow *window = [[NSWindow alloc]
        initWithContentRect:rect
                  styleMask:NSWindowStyleMaskTitled | NSWindowStyleMaskClosable |
                            NSWindowStyleMaskMiniaturizable | NSWindowStyleMaskResizable
                    backing:NSBackingStoreBuffered
                      defer:NO];
    window.title = title;
    window.releasedWhenClosed = NO;
    window.backgroundColor = color;
    window.level = NSNormalWindowLevel;
    NSTextField *label = [NSTextField labelWithString:title];
    label.frame = NSMakeRect(20, 40, 220, 24);
    [window.contentView addSubview:label];
    NSButton *button = [NSButton buttonWithTitle:@"twowin" target:nil action:nil];
    button.frame = NSMakeRect(20, 80, 120, 32);
    [window.contentView addSubview:button];
    [window orderFront:nil];
    return window;
}

- (void)go {
    const char *sx = getenv("PROBE_SPOT_X"), *sy = getenv("PROBE_SPOT_Y");
    CGFloat spotX = sx ? atof(sx) : 2300;
    CGFloat spotY = sy ? atof(sy) : 800;
    CGFloat screenHeight = NSScreen.screens.firstObject.frame.size.height;
    // AppKit's origin is bottom-left; the spot is expressed top-left like the
    // rest of the probe family.
    CGFloat baseY = screenHeight - spotY - 220;

    _a = [self makeWindowAt:NSMakeRect(spotX - 320, baseY, 300, 220)
                      title:@"twowin A"
                      color:NSColor.systemTealColor];
    _b = [self makeWindowAt:NSMakeRect(spotX + 20, baseY, 300, 220)
                      title:@"twowin B"
                      color:NSColor.systemIndigoColor];
    // Deliberately NOT `activateIgnoringOtherApps:` -- unlike onewin.m this
    // fixture only has to be ON SCREEN and in the CG window list for the AX
    // resolver, and yanking the operator's focus mid-run is both rude and a
    // way to perturb whatever else is being measured on the machine.
    [_a orderFront:nil];
    [_b orderFront:nil];

    // One line, one format. The runner refuses to report a pass unless it can
    // parse this AND see the guard name this pid, so a fixture that failed to
    // open can never read as a clean gate.
    fprintf(stderr, "TWOWIN pid=%d wid_a=%ld wid_b=%ld\n",
            (int)getpid(), (long)_a.windowNumber, (long)_b.windowNumber);
    fflush(stderr);

    const char *secs = getenv("TWOWIN_SECONDS");
    double lifetime = secs ? atof(secs) : 300.0;
    dispatch_after(dispatch_time(DISPATCH_TIME_NOW, (int64_t)(lifetime * NSEC_PER_SEC)),
                   dispatch_get_main_queue(), ^{
        fprintf(stderr, "TWOWIN exiting after %.0fs backstop\n", lifetime);
        exit(0);
    });
}
@end

int main(void) {
    @autoreleasepool {
        [NSApplication sharedApplication];
        [NSApp setActivationPolicy:NSApplicationActivationPolicyRegular];
        TwoWin *fixture = [[TwoWin alloc] init];
        dispatch_async(dispatch_get_main_queue(), ^{ [fixture go]; });
        [NSApp run];
    }
}
