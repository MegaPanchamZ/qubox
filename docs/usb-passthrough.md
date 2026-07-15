# USB passthrough

**Status: out of scope for desktop 1.0.**

If product requires USB IP / USB/IP-style passthrough later:

1. Research Linux `usbip` and Windows USBIP-Win  
2. New stream purpose (do not reuse FileSync)  
3. Explicit session permission bit `usb`  
4. Never auto-enable on managed hosts  

Until then, remote desktop input is keyboard/mouse/gamepad/pen only.
