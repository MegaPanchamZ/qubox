# PipeWire Portal Backend (Linux Wayland)

## Implementation Status

Phase A: compile-only stub (gated behind `--features pipewire`).

## TODO

1. Connect to PipeWire via `pw_init` and `pw_context_new`.
2. Enumerate nodes for video sources via `pw_core_enum_params`.
3. Create a PipeWire stream, negotiate `SpaVideoFormat` (BGRA or NV12).
4. Loop reading frame buffers.

## Virtual Displays

Virtual display creation on Wayland uses the `wlr-output-management` protocol
(available on Sway, Hyprland, Wayfire). If the compositor does not support
this protocol, `create_virtual_display` returns `Err(NotSupported)`.

## Privacy Mode

Privacy mode on Wayland depends on the compositor:
- `ext-image-copy-capture` protocol for frame capture.
- `xdg-shell` for window management.

## References

- [PipeWire API](https://docs.pipewire.org/)
- [wlr-output-management protocol](https://wayland.app/protocols/wlr-output-management)
