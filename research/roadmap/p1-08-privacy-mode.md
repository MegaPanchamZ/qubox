# P1-8: Privacy Mode (Virtual Displays)

Status: research complete, implementation pending.
Owner: `apps/host-agent` (host-side privacy), with a new `privacy` module.
Depends on: P1-7 (multi-monitor; the virtual display is one of the streams), audio routing (P1-10).
Blockers: Windows IddCx requires a signed kernel driver (substantial work); macOS CGVirtualDisplay requires entitlements (constrained). Linux vkms is straightforward.

## Goal

Provide a "privacy mode" where the host's local monitor(s) are blanked (or show a static "locked" image) while the actual game/desktop is rendered to a virtual display and streamed to the remote client. The local user cannot see the real desktop even if they alt-tab or close the streaming client. The virtual display is the right architecture (vs. a blank overlay) because it actually hides the desktop from the OS's compositor, not just from the user's eyes.

## Research Summary

### Why privacy mode (and why a real virtual display, not an overlay)

A **blank overlay** (Parsec's current method) captures the real desktop and paints a fullscreen black image on top. This works for "casual" privacy — the boss walking by doesn't see the game — but is bypassable:
- The user can close the streaming client; the overlay disappears.
- The user can press Win+Tab or alt-tab to bring another window to the front; the overlay is on top of the foreground window but not on top of the focus.
- A determined onlooker can read the screen via OS-level access (screenshot, screen recording, accessibility tools).

A **virtual display** solves all of these:
- The game renders to a display the OS treats as real, but the physical monitor is blanked.
- The local user cannot see the real desktop because the real desktop isn't on the local monitor.
- The streaming client (which captures the virtual display) is the only way to see the game.
- Closing the streaming client just loses the connection; the game continues to render to the virtual display.

This is the architecture Moonlight, NVIDIA GameStream, and most streaming competitors use (or aspire to).

### Linux: vkms (Virtual Kernel Mode Setting)

vkms is a **kernel module** that creates a virtual display device. It has been in mainline Linux since 4.6 and is included in the standard kernel. It is **software-only** — it does not provide GPU acceleration on its own; the game still renders to the real GPU, and vkms provides the "headless monitor" the OS sees.

```bash
# Load vkms
sudo modprobe vkms

# Verify
lsmod | grep vkms
ls /dev/dri/  # should show a renderD128 node for vkms

# Use as a display: with X11, set it as the active output
xrandr --listmonitors
xrandr --output VKMS-1 --primary
```

In Rust, talk to vkms through the standard **DRM** API. The `drm` crate (https://docs.rs/drm) provides ioctls for `drmModeGetCrtc`, `drmModeGetConnector`, `drmModeAddFB`, etc.

**Latency overhead**: ~1-2 ms. Negligible.

**Multiple outputs**: vkms supports multiple connectors; configure one per virtual display.

**VRR support**: not yet in mainline (as of 6.x kernels); in development for 6.10+.

**The trick**: vkms is a headless display that the game sees as a real monitor. The game's window is moved to the virtual display via xrandr or compositor configuration. The physical monitor (if any) is set to a "blank" mode (DPMS off, or the "privacy image" overlay).

### Windows: IddCx (Indirect Display Driver Class Extension)

Microsoft's modern virtual display API, available since Windows 10 2004. It is the successor to the legacy Mirror Driver approach.

- **It's a real Windows driver project**: IddCx requires a kernel-mode driver (UMDF or KMDF) signed by Microsoft. The reference implementation is `IddSampleDriver` on GitHub.
- **Rust bindings**: `iddcx-rs` is a third-party crate; for production use, hand-written FFI to the IddCx C API is the path.
- **Microsoft signing**: a driver must be signed by Microsoft (via the Windows Hardware Dev Center program) to load on a default-configured Windows install. This is a substantial undertaking and the first-time cost is measured in weeks, not days.

For the first release, **a simpler alternative** is a **dummy HDMI plug** (a $10 hardware dongle that emulates a monitor's EDID). It requires no driver work, but doesn't allow software control of the virtual display's lifecycle.

### macOS: CGVirtualDisplay (macOS 14+)

Apple's official virtual display API, but constrained:
- Requires the app to have specific entitlements.
- Only works for screen-capture-aware apps (which we are).
- The CGVirtualDisplay API is private in some macOS versions; public API in macOS 14+.

In Rust, use `objc2` to call the CGVirtualDisplay Objective-C API. The macOS implementation is the highest-risk of the three; defer to a follow-up.

### How the big products do it

- **Parsec**: blank-screen overlay on the host's physical monitor. Captures the real display, encodes it, paints a black/fake-lock image on top. Not a real virtual display.
- **Moonlight**: the client side. On the host, the user typically sets up a virtual display manually (vkms on Linux, IddCx driver on Windows, dummy HDMI plug). Moonlight doesn't provide a built-in privacy mode.
- **Steam Remote Play**: similar to Parsec; no virtual display by default.
- **NVIDIA GameStream** (discontinued 2023 but instructive): had a hardware-level privacy mode where the GPU's local display output could be disabled while the capture output remained active. Closest to a true hardware-level solution.

### Audio privacy

The host's local speakers should be silent. The user hears the game's audio over the stream (from the client's speakers/headphones). Two approaches:

- **Linux**: route audio to a virtual sink in PipeWire (or PulseAudio). `pactl load-module module-null-sink sink_name=bp_privacy`, then route the game's audio to it. The host's speakers hear nothing.
- **Windows**: use a virtual audio device (Virtual Audio Cable, VB-Audio, or a custom WASAPI loopback sink). Route the game to the virtual device; the host's speakers are silent.
- **macOS**: use a `null` audio device or a custom aggregate device. Use `coreaudio-rs` or `coreaudio` crate.

### Multi-display privacy

If the host has 2+ physical monitors, the user can see what's on the others. The options:

- **Blank the physical monitor with the game** (Parsec's approach): use DPMS off or a fullscreen overlay. Easier, but bypassable.
- **Move the game to a virtual display** (Moonlight's approach): the game renders to vkms/IddCx; the physical monitor is blanked. More secure.
- **Blank all physical monitors** (the strict approach): use DPMS off on all physical monitors, run the game on virtual displays only. Most secure; user can use the host for other tasks on a different physical monitor.

For the first release, support the first two. The strict approach is a follow-up.

### Rust crate matrix (2024-2026)

- **Linux (vkms)**: `drm` crate for the user-mode API. The kernel module is built-in.
- **Windows (IddCx)**: `iddcx-rs` (third-party) or hand-written FFI. Reference: Microsoft IddSampleDriver.
- **Windows (fallback)**: dummy HDMI plug (hardware, no Rust).
- **macOS (CGVirtualDisplay)**: `objc2` + Apple framework FFI. Private API in some macOS versions; entitlement-gated in others.
- **Audio (Linux)**: `libpulse-binding` or `pipewire` crate.
- **Audio (Windows)**: `windows` crate's WASAPI bindings.
- **Audio (macOS)**: `coreaudio-rs` or `coreaudio` crate.

### Anti-cheat compatibility

Virtual displays are often treated as suspicious by anti-cheat systems:
- **EAC (Easy Anti-Cheat)**: typically OK with vkms on Linux; may flag IddCx on Windows.
- **BattlEye**: similar; some users report false positives with IddCx.
- **Riot Vanguard (kernel-mode)**: most restrictive; may block any virtual display.

The safe assumption: **virtual display privacy mode may break anti-cheat compatibility**. Document the limitation; provide a fallback to the blank-overlay mode for anti-cheat-restricted titles.

## Implementation Plan

### Step 1: Privacy configuration

`apps/host-agent/src/privacy/mod.rs`:
- `pub enum PrivacyMode { Off, Overlay, VirtualDisplay }`.
- `pub struct PrivacyConfig { mode: PrivacyMode, blank_physical: bool, audio_routing: AudioRouting }`.
- `pub trait PrivacyBackend { fn enable(&self) -> Result<()>; fn disable(&self) -> Result<()>; fn status(&self) -> PrivacyStatus; }`.

### Step 2: Linux backend (vkms)

`apps/host-agent/src/privacy/linux.rs` (new, behind `cfg(target_os = "linux")`):
- `pub struct VkmsBackend`.
- `enable`: `modprobe vkms`, then move the game's window to the vkms output via xrandr, then DPMS-off the physical monitor via xrandr. Move the mouse to the vkms output.
- `disable`: xrandr to re-enable the physical monitor, then `modprobe -r vkms`.
- The xrandr commands are run via `std::process::Command` (xrandr is on every Linux desktop).
- For the audio path, `pactl load-module module-null-sink sink_name=bp_privacy` and route the game.
- Add `drm` crate to `host-agent/Cargo.toml` (for the future direct vkms API).

### Step 3: Windows backend (IddCx or dummy plug)

`apps/host-agent/src/privacy/windows.rs` (new, behind `cfg(target_os = "windows")`):
- **First cut: detect a dummy HDMI plug**. The user installs the dongle, the host-agent sees a new monitor in the DXGI enumeration (P1-7), the user moves the game to it.
- **Later: IddCx driver** (a separate project; not part of the host-agent). Ship the IddCx driver as a separate signed driver package.
- Audio: virtual audio device. The host-agent detects if VAC/VB-Audio is installed; if so, route the game to it. If not, fall back to the blank-overlay mode.

### Step 4: macOS backend (defer)

`apps/host-agent/src/privacy/macos.rs` (new, behind `cfg(target_os = "macos")`):
- Stub: returns "not implemented in this release".
- Document the workaround: use a third-party virtual display app (BetterDummy / DisplayDummy) on the host.

### Step 5: Overlay fallback

`apps/host-agent/src/privacy/overlay.rs` (new):
- The current "blank overlay" approach. Cross-platform.
- Capture the real desktop, render a fullscreen black or "lock screen" image on top using wgpu or a separate compositing layer.
- The user can close the overlay; the privacy is "casual" only.

### Step 6: CLI / GUI

Add `--privacy {off,overlay,virtual}` to the host-agent's `start-session` command. The GUI (P2) exposes a "Privacy" toggle in the session UI.

### Step 7: Tests

- Unit test: `PrivacyConfig` parsing.
- Integration test on Linux (with vkms loaded): verify the game can be moved to the vkms output and the physical monitor is blanked.
- Manual: tri-monitor Linux host (the dev box), enable privacy mode, verify only the virtual display has the game and the physical monitors are blank.

## Risks and Open Questions

- **IddCx driver signing**: requires the Windows Hardware Dev Center program and a Microsoft-signed driver. Substantial cost. For the first release, document the dummy HDMI plug workaround.
- **macOS CGVirtualDisplay**: entitlement-gated. Requires Apple's cooperation. Probably out of scope for v1.
- **vkms and GPU drivers**: vkms is software-only. The game still renders to the real GPU, but the OS treats vkms as a "real" display. Some games (especially those that detect the display topology) may behave oddly. Steam Deck users report vkms works for most games but not all.
- **Audio privacy on Linux**: PipeWire's null-sink works, but some games route audio through PulseAudio directly; need to verify the routing actually silences the host's speakers.
- **Multi-display with virtual displays**: if the host has 1 physical monitor and 1 vkms, the OS sees 2 monitors. Some games refuse to launch unless they see a specific display configuration. Document the limitations.
- **DPMS off vs. blank image**: DPMS off actually turns off the monitor's backlight; a blank image is just a black screen. DPMS is more secure. The host-agent should default to DPMS off.
- **Anti-cheat compatibility**: documented limitation. The user is responsible for knowing their game's anti-cheat policy.
- **Hot-plug**: if the user disconnects a physical monitor during privacy mode, the OS may rearrange the display topology and the game may lose its virtual display. Document the limitation.
- **vkms latency**: the game renders to the real GPU, captured via x11grab/DXGI. The vkms layer adds ~1-2 ms but the captured frame is from the real GPU, not vkms. The latency is unaffected by vkms.

## References

- vkms kernel docs: https://docs.kernel.org/gpu/vkms.html
- vkms in infradead: https://www.infradead.org/~mchehab/rst_conversion/gpu/vkms.html
- vkms LWN article: https://lwn.net/Articles/997850/
- Collabora on vkms testing: https://www.collabora.com/news-and-blog/blog/2020/08/07/testing-weston-drm-kms-backends-with-virtme-and-vkms/
- vkms source: https://github.com/torvalds/linux/blob/master/drivers/gpu/drm/vkms/vkms_drv.c
- drm crate: https://docs.rs/drm
- IddCx overview: https://learn.microsoft.com/en-us/windows-hardware/drivers/display/indirect-display-driver-model-overview
- IddCx sample driver: https://github.com/microsoft/Windows-classic-samples (search "IddSampleDriver")
- Virtual Display Driver (community): https://github.com/VirtualDrivers/Virtual-Display-Driver
- MolotovCherry/virtual-display-rs: https://github.com/MolotovCherry/virtual-display-rs
- Apple CGVirtualDisplay: macOS 14+ API, requires entitlements
- PulseAudio null-sink: https://www.freedesktop.org/wiki/Software/PulseAudio/Documentation/User/Modules/#module-null-sink
- PipeWire null-sink: https://docs.pipewire.org/page_module_pipewire_null_sink.html
- Perplexity research, 2026-07-02: vkms, IddCx, CGVirtualDisplay, audio privacy, anti-cheat.
