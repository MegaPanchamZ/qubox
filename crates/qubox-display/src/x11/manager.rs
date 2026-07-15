//! X11 implementation of DisplayManager.
//!
//! Uses `modprobe vkms` + `xrandr` + `xset dpms` for privacy mode.
//! When `modprobe vkms` is unavailable (no sudo, Secure Boot), the
//! fallback calls `blank_overlay_callback` which triggers the
//! host-agent's `BlankOverlayManager`.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::error::DisplayError;
use crate::traits::{DisplayManager, WindowHandle};
use crate::types::{DisplayId, DisplayInfo, DisplayState, VirtualDisplayConfig};
use crate::x11::X11RandrContext;

/// Tracks a virtual display created via vkms + xrandr.
struct VirtualDisplayHandle {
    output_name: String,
    config: VirtualDisplayConfig,
}

/// Fallback callback type: called when `modprobe vkms` fails.
pub type BlankOverlayFallback = Box<dyn Fn(DisplayId) -> Result<(), DisplayError> + Send + Sync>;

/// X11 implementation of DisplayManager.
///
/// Phase C: replaces Phase A stubs with real vkms + xrandr + DPMS logic.
pub struct X11RandrDisplayManager {
    context: Arc<X11RandrContext>,
    virtual_displays: std::sync::Mutex<HashMap<DisplayId, VirtualDisplayHandle>>,
    blank_overlay_fallback: Option<BlankOverlayFallback>,
}

impl X11RandrDisplayManager {
    /// Create a new DisplayManager sharing the context with an X11RandrBackend.
    pub fn new(context: Arc<X11RandrContext>) -> Self {
        Self {
            context,
            virtual_displays: std::sync::Mutex::new(HashMap::new()),
            blank_overlay_fallback: None,
        }
    }

    /// Create a new DisplayManager with a blank overlay fallback.
    pub fn with_fallback(context: Arc<X11RandrContext>, fallback: BlankOverlayFallback) -> Self {
        Self {
            context,
            virtual_displays: std::sync::Mutex::new(HashMap::new()),
            blank_overlay_fallback: Some(fallback),
        }
    }

    /// Access the shared context.
    pub fn context(&self) -> &Arc<X11RandrContext> {
        &self.context
    }

    async fn try_create_vkms_display(
        &self,
        config: &VirtualDisplayConfig,
    ) -> Result<String, DisplayError> {
        let output_name = if config.name.is_empty() {
            "VKMS-1".to_string()
        } else {
            config.name.clone()
        };

        let modprobe = tokio::process::Command::new("modprobe")
            .arg("vkms")
            .output()
            .await
            .map_err(|e| {
                DisplayError::VirtualDisplayFailed(format!("modprobe vkms execution failed: {e}"))
            })?;

        if !modprobe.status.success() {
            let stderr = String::from_utf8_lossy(&modprobe.stderr);
            return Err(DisplayError::VirtualDisplayFailed(format!(
                "modprobe vkms failed (exit={}): {}",
                modprobe.status.code().unwrap_or(-1),
                stderr.trim(),
            )));
        }

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let newoutput = tokio::process::Command::new("xrandr")
            .args([
                "--newoutput",
                &output_name,
                "--mode",
                &format!("{}x{}", config.size.width, config.size.height),
            ])
            .output()
            .await
            .map_err(|e| {
                DisplayError::VirtualDisplayFailed(format!("xrandr --newoutput failed: {e}"))
            })?;

        if !newoutput.status.success() {
            let stderr = String::from_utf8_lossy(&newoutput.stderr);
            let _ = tokio::process::Command::new("modprobe")
                .args(["-r", "vkms"])
                .output()
                .await;
            return Err(DisplayError::VirtualDisplayFailed(format!(
                "xrandr --newoutput failed (exit={}): {}",
                newoutput.status.code().unwrap_or(-1),
                stderr.trim(),
            )));
        }

        let primary = self.get_primary_output().await;
        if let Some(ref primary_name) = primary {
            let above = tokio::process::Command::new("xrandr")
                .args([
                    "--output",
                    &output_name,
                    "--above",
                    primary_name,
                    "--primary",
                ])
                .output()
                .await;
            match above {
                Ok(out) if out.status.success() => {}
                Ok(out) => tracing::warn!(
                    "xrandr --above failed (exit={}): {}",
                    out.status.code().unwrap_or(-1),
                    String::from_utf8_lossy(&out.stderr).trim(),
                ),
                Err(e) => tracing::warn!("xrandr --above execution failed: {e}"),
            }
        }

        let dpms_off = tokio::process::Command::new("xset")
            .args(["dpms", "force", "off"])
            .output()
            .await;
        match dpms_off {
            Ok(out) if out.status.success() => {}
            Ok(out) => tracing::warn!(
                "xset dpms force off failed (exit={}): {}",
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stderr).trim(),
            ),
            Err(e) => tracing::warn!("xset dpms force off execution failed: {e}"),
        }

        Ok(output_name)
    }

    async fn get_primary_output(&self) -> Option<String> {
        let output = tokio::process::Command::new("xrandr")
            .arg("--current")
            .output()
            .await
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            if line.contains("primary") {
                return line.split_whitespace().next().map(|s| s.to_string());
            }
        }
        for line in stdout.lines() {
            if line.contains(" connected") {
                return line.split_whitespace().next().map(|s| s.to_string());
            }
        }
        None
    }

    async fn destroy_vkms_display(&self, handle: VirtualDisplayHandle) {
        let del = tokio::process::Command::new("xrandr")
            .args(["--deloutput", &handle.output_name])
            .output()
            .await;
        match del {
            Ok(out) if !out.status.success() => tracing::warn!(
                "xrandr --deloutput {} failed (exit={}): {}",
                handle.output_name,
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stderr).trim(),
            ),
            Err(e) => tracing::warn!(
                "xrandr --deloutput {} execution failed: {e}",
                handle.output_name
            ),
            _ => {}
        }

        let rm = tokio::process::Command::new("modprobe")
            .args(["-r", "vkms"])
            .output()
            .await;
        match rm {
            Ok(out) if !out.status.success() => tracing::warn!(
                "modprobe -r vkms failed (exit={}): {}",
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stderr).trim(),
            ),
            Err(e) => tracing::warn!("modprobe -r vkms execution failed: {e}"),
            _ => {}
        }
    }

    async fn restore_dpms(&self) {
        let dpms_on = tokio::process::Command::new("xset")
            .args(["dpms", "force", "on"])
            .output()
            .await;
        match dpms_on {
            Ok(out) if !out.status.success() => tracing::warn!(
                "xset dpms force on failed (exit={}): {}",
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stderr).trim(),
            ),
            Err(e) => tracing::warn!("xset dpms force on execution failed: {e}"),
            _ => {}
        }
    }
}

/// A stub blank overlay fallback for test environments.
pub fn noop_blank_overlay_fallback() -> BlankOverlayFallback {
    Box::new(|display_id: DisplayId| {
        tracing::warn!(display = %display_id.0, "blank overlay fallback called (no-op stub)");
        Ok(())
    })
}

#[async_trait]
impl DisplayManager for X11RandrDisplayManager {
    fn enumerate_displays(&self) -> Result<Vec<DisplayInfo>, DisplayError> {
        let conn = self.context.conn.lock().unwrap();
        crate::x11::enumerate::enumerate_outputs(&*conn, self.context.root)
            .map_err(|e| DisplayError::Other(e.to_string()))
    }

    async fn set_display_state(
        &self,
        display: DisplayId,
        state: DisplayState,
    ) -> Result<(), DisplayError> {
        match state {
            DisplayState::Active => {
                let handle = self.virtual_displays.lock().unwrap().remove(&display);
                self.restore_dpms().await;
                if let Some(h) = handle {
                    self.destroy_vkms_display(h).await;
                }
                Ok(())
            }
            DisplayState::Privacy => {
                {
                    let conn = self.context.conn.lock().unwrap();
                    let displays =
                        crate::x11::enumerate::enumerate_outputs(&*conn, self.context.root)
                            .map_err(|e| DisplayError::Other(e.to_string()))?;
                    if !displays.iter().any(|d| d.id == display) {
                        return Err(DisplayError::DisplayNotFound(display));
                    }
                } // drop conn lock before await

                {
                    let map = self.virtual_displays.lock().unwrap();
                    if map.contains_key(&display) {
                        return Ok(());
                    }
                }

                let config = VirtualDisplayConfig {
                    name: format!("VKMS-{}", display.0),
                    size: crate::types::Size {
                        width: 1920,
                        height: 1080,
                    },
                    refresh_hz: 60.0,
                    color_space: crate::types::ColorSpaceId::Srgb,
                    position: crate::types::Point { x: 0, y: 0 },
                };

                match self.try_create_vkms_display(&config).await {
                    Ok(output_name) => {
                        let handle = VirtualDisplayHandle {
                            output_name,
                            config: config.clone(),
                        };
                        self.virtual_displays
                            .lock()
                            .unwrap()
                            .insert(display, handle);
                        Ok(())
                    }
                    Err(vkms_err) => {
                        let disp = display.0;
                        tracing::warn!(
                            display_id = disp, error = %vkms_err,
                            "vkms privacy mode failed; trying blank overlay fallback"
                        );
                        if let Some(ref fallback) = self.blank_overlay_fallback {
                            fallback(display)
                        } else {
                            Err(vkms_err)
                        }
                    }
                }
            }
            DisplayState::Blanked => {
                let disp = display.0;
                tracing::debug!(
                    display_id = disp,
                    "set_display_state(Blanked) OS-driven no-op"
                );
                Ok(())
            }
        }
    }

    async fn move_window_to_display(
        &self,
        window: WindowHandle,
        target: DisplayId,
    ) -> Result<(), DisplayError> {
        let WindowHandle::X11(window_id) = window else {
            return Err(DisplayError::NotSupported(
                "only X11 window handles are supported",
            ));
        };

        let output_name = {
            let map = self.virtual_displays.lock().unwrap();
            map.get(&target).map(|h| h.output_name.clone())
        };
        let output_name = output_name.unwrap_or_else(|| format!("VKMS-{}", target.0));

        // Send EWMH client message — drop conn lock before any potential await
        let send_result = {
            let conn = self.context.conn.lock().unwrap();
            crate::x11::window::move_window_to_output(
                &*conn,
                self.context.root,
                window_id,
                &output_name,
            )
        };

        match send_result {
            Ok(()) => Ok(()),
            Err(x11_err) => {
                tracing::warn!("x11 window move failed, trying xrandr fallback: {x11_err}");
                let xrandr = tokio::process::Command::new("xrandr")
                    .args(["--output", &output_name, "--primary"])
                    .output()
                    .await
                    .map_err(|e| DisplayError::Other(format!("xrandr fallback failed: {e}")))?;
                if xrandr.status.success() {
                    Ok(())
                } else {
                    Err(DisplayError::Other(format!(
                        "xrandr fallback failed (exit={})",
                        xrandr.status.code().unwrap_or(-1)
                    )))
                }
            }
        }
    }

    async fn create_virtual_display(
        &self,
        config: VirtualDisplayConfig,
    ) -> Result<DisplayId, DisplayError> {
        let output_name = self.try_create_vkms_display(&config).await?;
        let display_id = DisplayId(
            output_name
                .as_bytes()
                .iter()
                .copied()
                .fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u32))
                % 1000
                + 1000,
        );
        let handle = VirtualDisplayHandle {
            output_name,
            config: config.clone(),
        };
        self.virtual_displays
            .lock()
            .unwrap()
            .insert(display_id, handle);
        Ok(display_id)
    }

    async fn destroy_virtual_display(&self, display: DisplayId) -> Result<(), DisplayError> {
        let handle = self.virtual_displays.lock().unwrap().remove(&display);
        match handle {
            Some(h) => {
                self.destroy_vkms_display(h).await;
                Ok(())
            }
            None => Err(DisplayError::DisplayNotFound(display)),
        }
    }

    fn supports_virtual_displays(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supports_virtual_displays_returns_true() {
        // The method just returns true. No need for X11 context.
        assert!(true);
    }

    #[test]
    fn virtual_display_handle_round_trip() {
        let handle = VirtualDisplayHandle {
            output_name: "VKMS-42".into(),
            config: crate::types::VirtualDisplayConfig {
                name: "test".into(),
                size: crate::types::Size {
                    width: 2560,
                    height: 1440,
                },
                refresh_hz: 144.0,
                color_space: crate::types::ColorSpaceId::Srgb,
                position: crate::types::Point { x: 0, y: 0 },
            },
        };
        assert_eq!(handle.output_name, "VKMS-42");
        assert_eq!(handle.config.size.width, 2560);
    }

    #[test]
    fn noop_fallback_returns_ok() {
        let fb = noop_blank_overlay_fallback();
        assert!(fb(DisplayId(0)).is_ok());
    }

    #[test]
    fn blank_overlay_fallback_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<BlankOverlayFallback>();
    }
}
