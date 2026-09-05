// Rung-B actuator (#742): drive a real window under the (stationary) cursor and
// stamp each move, so the driver can diff move-time vs the pill's reposition.
//
// Permission-free: reads the ambient cursor via CGEventGetLocation (no grant),
// creates an 800x600 window centered there, and translates it in small steps
// that keep the cursor inside — so the hover pill must re-follow every step.
// Prints "MOVE <t_ms> <win_left> <win_top_global>" per step to stdout.
//
// It also supports --break to hold the window STILL (positive control: the pill
// must NOT emit spurious follow repositions).
//
// clang -fobjc-arc -framework Cocoa -o pillfollow pillfollow.m

#import <Cocoa/Cocoa.h>
#include <mach/mach_time.h>

static double now_ms(void) {
    static mach_timebase_info_data_t tb; if(!tb.denom) mach_timebase_info(&tb);
    return (double)(mach_absolute_time()*tb.numer/tb.denom)/1e6;
}

@interface A : NSObject @end
@implementation A { NSWindow *_w; BOOL _brk; int _i; NSPoint _origin; }
- (instancetype)initBreak:(BOOL)b { self=[super init]; _brk=b; return self; }
- (void)go {
    // ambient cursor, top-left origin (flip within main screen)
    CGEventRef e = CGEventCreate(NULL);
    CGPoint c = CGEventGetLocation(e); CFRelease(e);
    CGFloat H = NSScreen.screens.firstObject.frame.size.height;
    // window bottom-left in Cocoa coords, centered on cursor
    CGFloat wx = c.x - 400, wy = (H - c.y) - 300;
    _origin = NSMakePoint(wx, wy);
    _w = [[NSWindow alloc] initWithContentRect:NSMakeRect(wx, wy, 800, 600)
                                     styleMask:NSWindowStyleMaskTitled|NSWindowStyleMaskResizable
                                       backing:NSBackingStoreBuffered defer:NO];
    _w.title = @"pillfollow target"; _w.releasedWhenClosed = NO;
    _w.backgroundColor = NSColor.systemIndigoColor;
    [_w orderFront:nil];
    [NSApp activateIgnoringOtherApps:YES];
    fprintf(stderr, "cursor=(%.0f,%.0f) window global-topleft=(%.0f,%.0f) %s\n",
            c.x, c.y, wx, H-(wy+600), _brk?"[BREAK/control]":"[moving]");
    [NSTimer scheduledTimerWithTimeInterval:0.05 target:self selector:@selector(step:) userInfo:nil repeats:YES];
}
- (void)step:(NSTimer*)t {
    if (_i++ >= 40) { [t invalidate]; exit(0); }
    if (_brk) { // control: never move -> no follow reposition should occur
        printf("HOLD %.1f\n", now_ms()); fflush(stdout); return;
    }
    // small oscillation keeps the cursor (screen center of the window) inside
    CGFloat dx = (_i % 8) * 6.0 - 24.0;       // +/-24 px, cursor stays well inside 800 wide
    NSPoint p = NSMakePoint(_origin.x + dx, _origin.y);
    [_w setFrameOrigin:p];
    CGFloat H = NSScreen.screens.firstObject.frame.size.height;
    CGFloat global_top = H - (p.y + 600);
    printf("MOVE %.1f %.0f %.0f\n", now_ms(), p.x, global_top); fflush(stdout);
}
@end

int main(int argc, const char**argv){
    @autoreleasepool{
        BOOL brk = (argc>1 && strcmp(argv[1],"--break")==0);
        [NSApplication sharedApplication];
        [NSApp setActivationPolicy:NSApplicationActivationPolicyRegular];
        A *a=[[A alloc] initBreak:brk];
        dispatch_async(dispatch_get_main_queue(), ^{ [a go]; });
        [NSApp run];
    }
}
