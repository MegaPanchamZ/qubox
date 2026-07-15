# DXGI Backend (Windows)

## Implementation Status

Phase A: compile-only stub.

## TODO

1. `IDXGIFactory1::EnumAdapters1` for adapter enumeration.
2. `IDXGIAdapter1::EnumOutputs` for per-adapter output enumeration.
3. `IDXGIOutput1::DuplicateOutput` for frame capture.
4. `ID3D11Device` + `ID3D11DeviceContext` for GPU texture mapping.
5. Cursor compositing via `DXGI_OUTDUPL_FRAME_INFO`.
6. HDR detection via `IDXGIOutput6::GetDesc1`.

## Privacy Mode

Privacy mode on Windows requires either:
- A signed IddCx driver (significant cost and signing complexity).
- A dummy HDMI plug (the recommended v1 workaround).

For v1, `set_display_state(Privacy)` returns `Err(NotSupported)`.

## References

- [IDXGIOutput1::DuplicateOutput documentation](https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_2/nf-dxgi1_2-idxgioutput1-duplicateoutput)
- [IddCx driver model](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/indirect-display-driver-model)
