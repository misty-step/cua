// Generated-only private X11 target. Never run on a physical display.
#include <X11/Xlib.h>
#include <X11/Xutil.h>
#include <X11/Xlib-xcb.h>
#include <xcb/xcb.h>
#include <X11/Xatom.h>
#include <X11/extensions/scrnsaver.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <sys/select.h>

static int presses, releases, held, synthetic, siblings, toggled, destroyed;
static void drain(Display *d, Window target, Window sibling) {
    while (XPending(d)) {
        XEvent e;
        XNextEvent(d, &e);
        if (e.type != ButtonPress && e.type != ButtonRelease) continue;
        if (e.xbutton.window == target) {
            synthetic += e.xbutton.send_event != 0;
            if (e.type == ButtonPress) {
                presses++; held = 1;
                printf("{\"event\":\"press\"}\n");
            } else {
                releases++; held = 0; toggled = !toggled;
                XSetWindowBackground(d, target, toggled ? 0x00aa00 : 0x333333);
                XClearWindow(d, target);
                printf("{\"event\":\"release\"}\n");
            }
            XFlush(d);
            fflush(stdout);
        } else if (e.xbutton.window == sibling) siblings++;
    }
}

int main(void) {
    setvbuf(stdin, NULL, _IONBF, 0);
    Display *d = XOpenDisplay(NULL);
    if (!d) return 2;
    Window root = DefaultRootWindow(d);
    Window target = XCreateSimpleWindow(d, root, 20, 20, 120, 80, 0, 0, 0x333333);
    Window sibling = XCreateSimpleWindow(d, root, 200, 20, 120, 80, 0, 0, 0x555555);
    Atom pid_atom = XInternAtom(d, "_NET_WM_PID", False);
    unsigned long pid = getpid();
    for (int i = 0; i < 2; ++i) {
        Window w = i == 0 ? target : sibling;
        XChangeProperty(d, w, pid_atom, XA_CARDINAL, 32, PropModeReplace, (unsigned char *)&pid, 1);
        XSelectInput(d, w, ButtonPressMask | ButtonReleaseMask | ExposureMask);
        XMapWindow(d, w);
    }
    XSetInputFocus(d, sibling, RevertToParent, CurrentTime);
    XSync(d, False);
    printf("{\"pid\":%lu,\"target\":%lu,\"sibling\":%lu}\n", pid, target, sibling);
    fflush(stdout);
    for (;;) {
        drain(d, target, sibling);
        fd_set fds;
        FD_ZERO(&fds); FD_SET(0, &fds); FD_SET(ConnectionNumber(d), &fds);
        if (select(ConnectionNumber(d) + 1, &fds, NULL, NULL, NULL) < 0) break;
        if (!FD_ISSET(0, &fds)) continue;
        char command[32];
        if (!fgets(command, sizeof(command), stdin) || command[0] == 'q') break;
        if (!strncmp(command, "grab", 4)) {
            XGrabServer(d); XSync(d, False);
            puts("{\"grabbed\":true}"); fflush(stdout); continue;
        }
        if (!strncmp(command, "ungrab", 6)) {
            XUngrabServer(d); XSync(d, False);
            puts("{\"grabbed\":false}"); fflush(stdout); continue;
        }
        if (!strncmp(command, "destroy", 7)) {
            XDestroyWindow(d, target); destroyed = 1; held = 0;
            XSync(d, False);
            puts("{\"destroyed\":true}"); fflush(stdout); continue;
        }
        if (!strncmp(command, "reuse", 5)) {
            XDestroyWindow(d, target); XSync(d, False);
            xcb_connection_t *xcb = XGetXCBConnection(d);
            uint32_t values[] = {0x555555, XCB_EVENT_MASK_BUTTON_PRESS | XCB_EVENT_MASK_BUTTON_RELEASE};
            xcb_create_window(xcb, XCB_COPY_FROM_PARENT, target, root, 20, 20, 120, 80, 0,
                              XCB_WINDOW_CLASS_INPUT_OUTPUT, XCB_COPY_FROM_PARENT,
                              XCB_CW_BACK_PIXEL | XCB_CW_EVENT_MASK, values);
            xcb_map_window(xcb, target); xcb_flush(xcb); XSync(d, False);
            destroyed = 1; held = 0;
            puts("{\"reused\":true}"); fflush(stdout); continue;
        }
        if (!strncmp(command, "unmap", 5)) {
            XUnmapWindow(d, target); XSync(d, False);
            puts("{\"unmapped\":true}"); fflush(stdout); continue;
        }
        if (!strncmp(command, "map", 3)) {
            XMapWindow(d, target); XSync(d, False);
            puts("{\"mapped\":true}"); fflush(stdout); continue;
        }
        XSync(d, False); drain(d, target, sibling);
        XScreenSaverInfo *idle = XScreenSaverAllocInfo();
        int valid = XScreenSaverQueryInfo(d, root, idle);
        Window focus, qr, qc;
        int revert, rx, ry, wx, wy;
        unsigned int mask;
        XGetInputFocus(d, &focus, &revert);
        XQueryPointer(d, root, &qr, &qc, &rx, &ry, &wx, &wy, &mask);
        unsigned long pixel = 0;
        if (!destroyed) {
            XImage *image = XGetImage(d, target, 30, 30, 1, 1, AllPlanes, ZPixmap);
            if (!image) return 3;
            pixel = XGetPixel(image, 0, 0); XDestroyImage(image);
        }
        printf("{\"idle_valid\":%d,\"idle_ms\":%lu,\"press\":%d,\"release\":%d,\"held\":%d,\"synthetic_events\":%d,\"sibling_events\":%d,\"focus\":%lu,\"root_mask\":%u,\"root_xy\":[%d,%d],\"toggled\":%d,\"pixel\":%lu}\n",
               valid, idle->idle, presses, releases, held, synthetic, siblings, focus, mask, rx, ry, toggled, pixel);
        XFree(idle); fflush(stdout);
    }
    XCloseDisplay(d);
    return 0;
}
