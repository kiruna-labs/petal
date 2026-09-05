// One-window hover probe (#747 stage-1). arg1 = "popup" (borderless, no chrome)
// or "normal" (titled). Warps the cursor to a fixed spot, opens ONE window
// centered there, and CONTINUOUSLY posts mouseMoved over it (Petal's hover
// re-evaluates on movement, not a static cursor). Window stays open the whole
// run. Prints wid + frame; exits after 14s.
//   clang -fobjc-arc -framework Cocoa -o onewin onewin.m
#import <Cocoa/Cocoa.h>
#import <ApplicationServices/ApplicationServices.h>
@interface P : NSObject @end
@implementation P { NSWindow *_w; NSTimer *_mv; BOOL _popup; BOOL _morph; }
- (instancetype)initPopup:(BOOL)p morph:(BOOL)m { if ((self=[super init])) { _popup=p; _morph=m; } return self; }
- (void)go {
    // Spot comes from env so the harness can aim at an empty screen region
    // on any display topology (default matches the 2560-wide dev rig).
    const char *sx = getenv("PROBE_SPOT_X"), *sy = getenv("PROBE_SPOT_Y");
    CGPoint spot = CGPointMake(sx ? atof(sx) : 2300, sy ? atof(sy) : 800);
    CGWarpMouseCursorPosition(spot);
    CGFloat H = NSScreen.screens.firstObject.frame.size.height;
    CGFloat px = spot.x - 150, py = (H - spot.y) - 100;
    NSUInteger style = _popup ? NSWindowStyleMaskBorderless
        : (NSWindowStyleMaskTitled|NSWindowStyleMaskClosable|NSWindowStyleMaskResizable);
    _w = [[NSWindow alloc] initWithContentRect:NSMakeRect(px, py, 300, 200)
                                     styleMask:style backing:NSBackingStoreBuffered defer:NO];
    _w.title = _popup ? @"onewin popup" : @"onewin normal";
    _w.releasedWhenClosed = NO;
    _w.backgroundColor = _popup ? NSColor.systemPinkColor : NSColor.systemTealColor;
    _w.level = NSNormalWindowLevel;
    [_w orderFront:nil];
    [NSApp activateIgnoringOtherApps:YES];
    [_w orderFront:nil];
    fprintf(stderr, "%s wid=%ld frame=(%.0f,%.0f 300x200) cursorspot=(%.0f,%.0f)\n",
            _popup?"POPUP":"NORMAL", (long)_w.windowNumber, px, py, spot.x, spot.y);
    // Self-inspect (no trust needed for own process): does this window appear
    // in our OWN kAXWindows array, and with what subrole? Tests the hypothesis
    // that borderless windows are absent from AX windows lists.
    dispatch_after(dispatch_time(DISPATCH_TIME_NOW, (int64_t)(2.0 * NSEC_PER_SEC)),
                   dispatch_get_main_queue(), ^{
        AXUIElementRef app = AXUIElementCreateApplication(getpid());
        CFTypeRef wins = NULL;
        AXError e = AXUIElementCopyAttributeValue(app, kAXWindowsAttribute, &wins);
        if (e || !wins) { fprintf(stderr, "SELF-AX: kAXWindows err=%d\n", e); return; }
        CFArrayRef arr = (CFArrayRef)wins;
        fprintf(stderr, "SELF-AX: kAXWindows count=%ld (my wid=%ld)\n",
                CFArrayGetCount(arr), (long)self->_w.windowNumber);
        for (CFIndex i = 0; i < CFArrayGetCount(arr); i++) {
            AXUIElementRef w = (AXUIElementRef)CFArrayGetValueAtIndex(arr, i);
            CGWindowID wid = 0;
            extern AXError _AXUIElementGetWindow(AXUIElementRef, CGWindowID *);
            AXError ge = _AXUIElementGetWindow(w, &wid);
            CFTypeRef sub = NULL;
            AXUIElementCopyAttributeValue(w, kAXSubroleAttribute, &sub);
            CFTypeRef btn = NULL;
            bool chrome = (AXUIElementCopyAttributeValue(w, kAXCloseButtonAttribute, &btn) == 0 && btn);
            if (btn) CFRelease(btn);
            fprintf(stderr, "SELF-AX:  [%ld] wid=%u(err=%d) subrole=%s chrome=%d\n",
                    i, wid, ge,
                    sub && CFGetTypeID(sub) == CFStringGetTypeID()
                        ? [(__bridge NSString *)sub UTF8String] : "<none>",
                    chrome);
            if (sub) CFRelease(sub);
        }
        CFRelease(wins);
        CFRelease(app);
    });
    __block int tick = 0;
    _mv = [NSTimer scheduledTimerWithTimeInterval:0.08 repeats:YES block:^(NSTimer *t){
        CGPoint p = CGPointMake(spot.x + ((tick%2)?1.0:-1.0), spot.y);
        CGEventRef e = CGEventCreateMouseEvent(NULL, kCGEventMouseMoved, p, kCGMouseButtonLeft);
        CGEventPost(kCGHIDEventTap, e); CFRelease(e); tick++;
    }];
    // morph mode: after 8s, gain title-bar chrome (subrole mutation live test:
    // AX state events must trigger a recheck; Popup -> Standard).
    if (_morph) {
        dispatch_after(dispatch_time(DISPATCH_TIME_NOW,(int64_t)(8.0*NSEC_PER_SEC)),
                       dispatch_get_main_queue(), ^{
            self->_w.styleMask = NSWindowStyleMaskTitled|NSWindowStyleMaskClosable|NSWindowStyleMaskResizable;
            fprintf(stderr, "MORPHED to titled (wid may persist=%ld)\n", (long)self->_w.windowNumber);
        });
    }
    dispatch_after(dispatch_time(DISPATCH_TIME_NOW,(int64_t)(18.0*NSEC_PER_SEC)),
                   dispatch_get_main_queue(), ^{ exit(0); });
}
@end
int main(int argc, char **argv) {
    @autoreleasepool {
        BOOL morph = (argc>1 && strcmp(argv[1],"morph")==0);
        BOOL popup = morph || (argc>1 && strcmp(argv[1],"popup")==0);
        [NSApplication sharedApplication];
        [NSApp setActivationPolicy:NSApplicationActivationPolicyRegular];
        P *p = [[P alloc] initPopup:popup morph:morph];
        dispatch_async(dispatch_get_main_queue(), ^{ [p go]; });
        [NSApp run];
    }
}
