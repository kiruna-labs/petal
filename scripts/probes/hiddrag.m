// #761 live actuator: REAL synthetic title-bar drag of a probe window via
// HID-posted events (leftMouseDown on the title bar -> dragged steps -> up),
// so the gesture tap AND WindowServer's actual drag machinery both engage.
// Prints "DRAG <epoch_ms> <cursor_x> <cursor_y>" per step (epoch so the
// driver can align directly with petal.log wall-clock timestamps), then
// "DONE <epoch_ms> <final_win_x> <final_win_y>".
//
// clang -fobjc-arc -framework Cocoa -o hiddrag hiddrag.m
#import <Cocoa/Cocoa.h>
#include <sys/time.h>

static double epoch_ms(void) {
    struct timeval tv; gettimeofday(&tv, NULL);
    return tv.tv_sec * 1000.0 + tv.tv_usec / 1000.0;
}
static void post(CGEventType t, CGPoint p) {
    CGEventRef e = CGEventCreateMouseEvent(NULL, t, p, kCGMouseButtonLeft);
    CGEventPost(kCGHIDEventTap, e);
    CFRelease(e);
}

@interface D : NSObject @end
@implementation D { NSWindow *_w; }
- (void)go {
    const char *sx = getenv("PROBE_SPOT_X"), *sy = getenv("PROBE_SPOT_Y");
    CGPoint spot = CGPointMake(sx ? atof(sx) : 2300, sy ? atof(sy) : 700);
    CGFloat H = NSScreen.screens.firstObject.frame.size.height;
    // titled window, top-left at spot (global top-left coords)
    CGFloat px = spot.x - 150, py_top = spot.y - 100;
    _w = [[NSWindow alloc] initWithContentRect:NSMakeRect(px, H - py_top - 200, 300, 200)
                                     styleMask:NSWindowStyleMaskTitled|NSWindowStyleMaskClosable
                                       backing:NSBackingStoreBuffered defer:NO];
    _w.title = @"hiddrag target"; _w.releasedWhenClosed = NO;
    _w.backgroundColor = NSColor.systemOrangeColor;
    [_w orderFront:nil];
    [NSApp activateIgnoringOtherApps:YES];
    fprintf(stderr, "TARGET wid=%ld topleft=(%.0f,%.0f)\n",
            (long)_w.windowNumber, px, py_top);

    // settle: let the registry/hover see the window and show a pill
    dispatch_after(dispatch_time(DISPATCH_TIME_NOW, (int64_t)(3.0 * NSEC_PER_SEC)),
                   dispatch_get_main_queue(), ^{
        // grab the title bar: 150px in, 10px down from the top edge
        __block CGPoint c = CGPointMake(px + 150, py_top + 10);
        CGWarpMouseCursorPosition(c);
        post(kCGEventLeftMouseDown, c);
        fprintf(stderr, "DRAG %.0f %.0f %.0f\n", epoch_ms(), c.x, c.y);
        // 50 steps, 8ms apart, moving left+down at ~700 px/s
        dispatch_async(dispatch_get_global_queue(QOS_CLASS_USER_INTERACTIVE, 0), ^{
            for (int i = 0; i < 50; i++) {
                usleep(8000);
                c.x -= 6.0; c.y += 2.0;
                post(kCGEventLeftMouseDragged, c);
                fprintf(stderr, "DRAG %.0f %.0f %.0f\n", epoch_ms(), c.x, c.y);
            }
            usleep(8000);
            post(kCGEventLeftMouseUp, c);
            dispatch_async(dispatch_get_main_queue(), ^{
                // report the REAL final frame as ground truth
                NSRect f = self->_w.frame;
                CGFloat topY = H - NSMaxY(f);
                fprintf(stderr, "DONE %.0f %.0f %.0f\n", epoch_ms(), f.origin.x, topY);
            });
            sleep(2);
            exit(0);
        });
    });
}
@end

int main(void) {
    @autoreleasepool {
        [NSApplication sharedApplication];
        [NSApp setActivationPolicy:NSApplicationActivationPolicyRegular];
        D *d = [[D alloc] init];
        dispatch_async(dispatch_get_main_queue(), ^{ [d go]; });
        [NSApp run];
    }
    return 0;
}
