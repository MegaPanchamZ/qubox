# ScreenCaptureKit Backend (macOS)

## Implementation Status

Phase A: compile-only stub.

## TODO

1. `SCShareableContent::current().await` for display enumeration.
2. `SCStreamConfiguration` + `SCStream` per display for capture.
3. `SCStreamDelegate` callback for frame delivery.
4. `CVPixelBuffer` → BGRA byte conversion.
5. Color space detection from `CGDisplay` properties.

## Virtual Displays

Virtual display creation via `CGVirtualDisplay` requires Apple entitlements
and is not available to third-party apps without explicit Apple approval.

Workaround for v1: use a third-party app like BetterDummy or DisplayDummy.

## Privacy Mode

Not supported in v1. `set_display_state(Privacy)` returns `Err(NotSupported)`.

## References

- [ScreenCaptureKit framework](https://developer.apple.com/documentation/screencapturekit)
- [CGVirtualDisplay](https://developer.apple.com/documentation/coregraphics/cgvirtualdisplay)
