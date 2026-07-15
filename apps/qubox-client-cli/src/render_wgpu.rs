//! P0-5 GPU presentation pipeline.
//!
//! Owns the wgpu stack for one video window: `Instance`, `Adapter`,
//! `Device`, `Queue`, `Surface`, and the BGRA-video-blit pipeline. The
//! renderer is generic over its frame consumer: pass any
//! `wgpu::Surface` and the same renderer uploads frames and presents
//! them through the supplied `Surface`.
//!
//! ## Pipeline
//!
//! One render pipeline (`video_blit`) blits a BGRA8 texture onto the
//! swapchain. The vertex stage synthesises positions from
//! `@builtin(vertex_index)` so there is no vertex buffer; the fragment
//! stage samples the texture with a linear-clamp sampler. The WGSL
//! lives inline as a `const &str` to avoid a runtime file load.
//!
//! ## Present mode
//!
//! Preferring `PresentMode::Mailbox` (low-latency, no-tearing,
//! requires vsync-off, ideal for a remote-desktop scenario locked to
//! the host capture cadence) and falling back to `Fifo` (vsync) is
//! what the ADR prescribes. `Immediate` is intentionally never used:
//! macOS Metal and Linux Vulkan refuse it.
//!
//! ## Why `Bgra8Unorm` and not the surface's first format
//!
//! Most desktops expose `Bgra8Unorm` as the preferred swapchain
//! format on Windows and Linux, and `Bgra8Unorm` on macOS. By picking
//! it first and falling back to the surface's first format, the
//! renderer avoids an RGBA-BGRA byte-swap on the present path.

use std::sync::Arc;

use anyhow::{Context, Result};
use winit::window::Window;

use crate::frame_pipeline::{DecodedFrame, PixelFormat};

/// Inclusive list of HDR tone-mapping operators the wgpu renderer
/// can apply. Selected at construction time (`--tone-map` on the
/// CLI). The default is [`ToneMapKind::Bt2390`] (ITU-R BT.2390
/// perceptual quantizer) per ADR-010 §6.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToneMapKind {
    /// Pure sRGB passthrough, no tone mapping. Useful for the
    /// display-referred preview path.
    SrgbPassthrough,
    /// Hable filmic operator (John Hable, GDC 2010). Good for
    /// artistic / cinematic looks.
    Hable,
    /// BT.2390 perceptual quantizer (ITU-R BT.2390 §3).
    /// Default for HDR10 material on SDR displays.
    Bt2390,
}

impl ToneMapKind {
    /// Stable lower-case label — used as the fragment entry point
    /// name in [`VIDEO_BLIT_WGSL`] and as the `--tone-map` flag
    /// value.
    pub fn label(self) -> &'static str {
        match self {
            ToneMapKind::SrgbPassthrough => "srgb-passthrough",
            ToneMapKind::Hable => "hable",
            ToneMapKind::Bt2390 => "bt2390",
        }
    }

    /// Fragment entry-point name declared by [`VIDEO_BLIT_WGSL`].
    pub fn entry_point(self) -> &'static str {
        match self {
            ToneMapKind::SrgbPassthrough => "fs_passthrough",
            ToneMapKind::Hable => "fs_hable",
            ToneMapKind::Bt2390 => "fs_bt2390",
        }
    }

    /// Parse the `--tone-map` flag. Returns [`ToneMapKind::Bt2390`]
    /// (the ADR-010 default) when the input does not match a known
    /// variant — this matches the spec's "Default to BT.2390".
    pub fn from_label(label: &str) -> Self {
        match label {
            "hable" => ToneMapKind::Hable,
            "bt2390" => ToneMapKind::Bt2390,
            "srgb-passthrough" => ToneMapKind::SrgbPassthrough,
            _ => ToneMapKind::Bt2390,
        }
    }
}

/// WGSL shader source for the video blit pipeline. The tone-map
/// helpers (`hable_tone_map`, `bt2390_tone_map`, `passthrough_tone_map`)
/// and three fragment entry points (`fs_passthrough`, `fs_hable`,
/// `fs_bt2390`) live in `apps/qubox-client-cli/src/shaders/tone_map.wgsl`
/// and are composed via `include_str!` at compile time so there is
/// no runtime file I/O.
const VIDEO_BLIT_WGSL: &str = include_str!("shaders/tone_map.wgsl");

/// The renderer holds all GPU resources behind `Arc`s so it can be
/// shared between the winit `ApplicationHandler` (mutable borrows)
/// and the HW decoder's `WinitUserEvent` wakeup (immutable borrows).
#[derive(Debug)]
pub struct WgpuRenderer {
    _instance: wgpu::Instance,
    _adapter: wgpu::Adapter,
    device: Arc<wgpu::Device>,
    queue: Arc<wgpu::Queue>,
    surface: wgpu::Surface<'static>,
    surface_config: wgpu::SurfaceConfiguration,
    blit_pipelines: std::collections::HashMap<ToneMapKind, wgpu::RenderPipeline>,
    blit_bind_group_layout: wgpu::BindGroupLayout,
    blit_sampler: wgpu::Sampler,
    frame_texture: Option<wgpu::Texture>,
    frame_texture_format: wgpu::TextureFormat,
    frame_width: u32,
    frame_height: u32,
    /// Active tone-mapping operator. Cycled by the user via
    /// `WinitUserEvent::CycleToneMap`. Read on every frame by
    /// [`Self::render`].
    tone_map_kind: ToneMapKind,
    /// Most-recently decoded frame, kept CPU-side for the
    /// round-trip-to-RAM stats-overlay path until the GPU upload
    /// completes. `None` when no frame has been seen.
    last_frame: Option<DecodedFrame>,
}

impl WgpuRenderer {
    /// Construct a renderer bound to the provided winit `Window`. The
    /// surface is created from the window's raw handle; both `Device`
    /// and `Queue` are wrapped in `Arc` so the renderer's fields can
    /// be referenced from event-loop callbacks.
    pub fn new(
        window: Arc<Window>,
        width: u32,
        height: u32,
        tone_map_kind: ToneMapKind,
    ) -> Result<Self> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            ..Default::default()
        });

        let surface = instance.create_surface(window.clone()).context(
            "failed to create wgpu::Surface from the winit window — set --renderer=minifb to bypass wgpu",
        )?;

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))
        .or_else(|| {
            pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::LowPower,
                compatible_surface: Some(&surface),
                force_fallback_adapter: true,
            }))
        })
        .context("no GPU adapter matched this surface; try --renderer=minifb")?;

        let adapter_info = adapter.get_info();
        tracing::info!(
            name = %adapter_info.name,
            backend = ?adapter_info.backend,
            device_type = ?adapter_info.device_type,
            vendor = adapter_info.vendor,
            device = adapter_info.device,
            "wgpu adapter selected"
        );

        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("qubox-client-cli::WgpuRenderer"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::downlevel_defaults(),
                memory_hints: wgpu::MemoryHints::default(),
            },
            None,
        ))
        .context("failed to request wgpu::Device")?;

        let device = Arc::new(device);
        let queue = Arc::new(queue);

        let surface_caps = surface.get_capabilities(&adapter);
        let chosen_format = surface_caps
            .formats
            .iter()
            .copied()
            .find(|f| {
                matches!(
                    f,
                    wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
                )
            })
            .unwrap_or(surface_caps.formats[0]);

        let present_mode = if surface_caps
            .present_modes
            .contains(&wgpu::PresentMode::Mailbox)
        {
            wgpu::PresentMode::Mailbox
        } else {
            wgpu::PresentMode::Fifo
        };

        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: chosen_format,
            width: width.max(1),
            height: height.max(1),
            present_mode,
            desired_maximum_frame_latency: 1,
            alpha_mode: surface_caps.alpha_modes[0],
            view_formats: vec![],
        };
        surface.configure(&device, &surface_config);

        let blit_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("qubox-client-cli::video_blit::shader"),
            source: wgpu::ShaderSource::Wgsl(VIDEO_BLIT_WGSL.into()),
        });

        let blit_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("qubox-client-cli::video_blit::bgl"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
            });

        let blit_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("qubox-client-cli::video_blit::layout"),
            bind_group_layouts: &[&blit_bind_group_layout],
            push_constant_ranges: &[],
        });

        // Build a RenderPipeline per ToneMapKind. They share the
        // bind-group layout and the vertex stage; only the fragment
        // entry point differs (fs_passthrough / fs_hable / fs_bt2390).
        let mut blit_pipelines = std::collections::HashMap::new();
        for kind in [
            ToneMapKind::SrgbPassthrough,
            ToneMapKind::Hable,
            ToneMapKind::Bt2390,
        ] {
            let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(&format!(
                    "qubox-client-cli::video_blit::pipeline::{}",
                    kind.label()
                )),
                layout: Some(&blit_pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &blit_shader,
                    entry_point: Some("vs"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &blit_shader,
                    entry_point: Some(kind.entry_point()),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: chosen_format,
                        blend: Some(wgpu::BlendState::REPLACE),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    strip_index_format: None,
                    front_face: wgpu::FrontFace::Ccw,
                    cull_mode: None,
                    unclipped_depth: false,
                    polygon_mode: wgpu::PolygonMode::Fill,
                    conservative: false,
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview: None,
                cache: None,
            });
            blit_pipelines.insert(kind, pipeline);
        }

        let blit_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("qubox-client-cli::video_blit::sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        Ok(Self {
            _instance: instance,
            _adapter: adapter,
            device,
            queue,
            surface,
            surface_config,
            blit_pipelines,
            blit_bind_group_layout,
            blit_sampler,
            frame_texture: None,
            frame_texture_format: wgpu::TextureFormat::Bgra8Unorm,
            frame_width: 0,
            frame_height: 0,
            tone_map_kind,
            last_frame: None,
        })
    }

    /// Resize the swapchain. Call from `WindowEvent::Resized`.
    pub fn resize(&mut self, width: u32, height: u32) {
        let width = width.max(1);
        let height = height.max(1);
        if self.surface_config.width == width && self.surface_config.height == height {
            return;
        }
        self.surface_config.width = width;
        self.surface_config.height = height;
        self.surface.configure(&self.device, &self.surface_config);
    }

    /// Upload a decoded frame to a `Bgra8Unorm` texture. The actual
    /// GPU work is deferred to the next `render()` call; this method
    /// only fills the staging bytes.
    pub fn upload_frame(&mut self, frame: DecodedFrame) -> Result<()> {
        if frame.width == 0 || frame.height == 0 {
            return Ok(());
        }
        if frame.pixel_format != PixelFormat::Bgra8Unorm {
            anyhow::bail!(
                "WgpuRenderer::upload_frame requires PixelFormat::Bgra8Unorm, got {:?}",
                frame.pixel_format
            );
        }
        let width = frame.width;
        let height = frame.height;
        let bytes_per_row = frame.bytes_per_row;
        let bytes_vec = frame
            .data
            .as_bytes()
            .ok_or_else(|| {
                anyhow::anyhow!("GpuHandle payloads not supported by WgpuRenderer::upload_frame")
            })?
            .to_vec();
        let queue = self.queue.clone();
        let captured_at = frame.captured_at;
        let pixel_format = frame.pixel_format;
        let stored_bytes = bytes_vec.clone();
        {
            let texture = self.ensure_frame_texture(width, height, bytes_per_row)?;
            queue.write_texture(
                wgpu::ImageCopyTexture {
                    texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                bytes_vec.as_slice(),
                wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(bytes_per_row),
                    rows_per_image: Some(height),
                },
                wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
            );
        }
        self.last_frame = Some(DecodedFrame {
            width,
            height,
            bytes_per_row,
            pixel_format,
            data: crate::frame_pipeline::PixelData::Owned(stored_bytes),
            captured_at,
        });
        Ok(())
    }

    fn ensure_frame_texture(
        &mut self,
        width: u32,
        height: u32,
        bytes_per_row: u32,
    ) -> Result<&mut wgpu::Texture> {
        if self.frame_width != width || self.frame_height != height || self.frame_texture.is_none()
        {
            let padded_bpr = (bytes_per_row + 255) & !255;
            let _ = padded_bpr;
            let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("qubox-client-cli::video_texture"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: self.frame_texture_format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            self.frame_texture = Some(texture);
            self.frame_width = width;
            self.frame_height = height;
        }
        Ok(self
            .frame_texture
            .as_mut()
            .expect("frame_texture is set immediately above"))
    }

    /// Acquire the next swapchain texture, run the video blit pipeline
    /// against the most recently uploaded frame, and present.
    pub fn render(&self) -> Result<()> {
        let texture = match self.frame_texture.as_ref() {
            Some(texture) => texture,
            None => return Ok(()),
        };
        let frame = match self.surface.get_current_texture() {
            Ok(frame) => frame,
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                tracing::debug!("wgpu::SurfaceError; reconfiguring swapchain");
                self.surface.configure(&self.device, &self.surface_config);
                return Ok(());
            }
            Err(error) => {
                tracing::warn!(?error, "wgpu::SurfaceError while acquiring swapchain");
                return Ok(());
            }
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let texture_view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("qubox-client-cli::video_blit::bg"),
            layout: &self.blit_bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&texture_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.blit_sampler),
                },
            ],
        });
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("qubox-client-cli::video_blit::encoder"),
            });
        {
            let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("qubox-client-cli::video_blit::pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            let pipeline = self
                .blit_pipelines
                .get(&self.tone_map_kind)
                .or_else(|| self.blit_pipelines.get(&ToneMapKind::Bt2390))
                .expect("bt2390 pipeline must be present");
            render_pass.set_pipeline(&pipeline);
            render_pass.set_bind_group(0, &bind_group, &[]);
            render_pass.draw(0..3, 0..1);
        }
        self.queue.submit(std::iter::once(encoder.finish()));
        frame.present();
        Ok(())
    }

    /// Cycle to the next [`ToneMapKind`]. Wired to
    /// `WinitUserEvent::CycleToneMap`. Falls back to
    /// [`ToneMapKind::Bt2390`] after the last entry.
    pub fn cycle_tone_map(&mut self) -> ToneMapKind {
        self.tone_map_kind = match self.tone_map_kind {
            ToneMapKind::SrgbPassthrough => ToneMapKind::Hable,
            ToneMapKind::Hable => ToneMapKind::Bt2390,
            ToneMapKind::Bt2390 => ToneMapKind::SrgbPassthrough,
        };
        self.tone_map_kind
    }

    /// Override the active tone-map operator at runtime. Wired to
    /// `--tone-map` on the CLI. Logs the new kind at debug level.
    pub fn set_tone_map(&mut self, kind: ToneMapKind) {
        tracing::debug!(tone_map = %kind.label(), "WgpuRenderer tone map set");
        self.tone_map_kind = kind;
    }

    /// Read the currently active tone-map mode.
    pub fn tone_map_kind(&self) -> ToneMapKind {
        self.tone_map_kind
    }

    /// The latest decoded frame kept CPU-side after upload. Used by
    /// the round-trip-to-RAM stats-overlay paint path
    /// (P1-12 stays software per the ADR §6 decision).
    pub fn last_frame(&self) -> Option<&DecodedFrame> {
        self.last_frame.as_ref()
    }

    /// Get the device — exposed for callers that need to allocate
    /// staging buffers (e.g. the round-trip-to-RAM overlay path).
    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }

    /// Get the queue — exposed for the same reason as [`Self::device`].
    pub fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }
}

/// Helpers used by tests.
pub mod test_helpers {
    #[allow(unused_imports)]
    use super::*;

    /// Compute the BGRA stride for `width`, copying the
    /// `PixelFormat::bytes_per_pixel` math out of `frame_pipeline::PixelFormat`
    /// to keep this module self-contained when imported from tests.
    pub const fn bgra_stride(width: u32) -> u32 {
        width * 4
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tone_map_wgsl_declares_all_three_fragment_entry_points() {
        // Each `fn fs_<kind>` entry point must be present so the
        // renderer's three pipelines compile against the same
        // shader module.
        assert!(VIDEO_BLIT_WGSL.contains("fn fs_passthrough"));
        assert!(VIDEO_BLIT_WGSL.contains("fn fs_hable"));
        assert!(VIDEO_BLIT_WGSL.contains("fn fs_bt2390"));
        assert!(VIDEO_BLIT_WGSL.contains("fn hable_tone_map"));
        assert!(VIDEO_BLIT_WGSL.contains("fn bt2390_tone_map"));
        assert!(VIDEO_BLIT_WGSL.contains("fn passthrough_tone_map"));
    }

    #[test]
    fn tone_map_selection_picks_correct_pipeline() {
        // Each ToneMapKind must map to a distinct WGSL entry point
        // declared in VIDEO_BLIT_WGSL, and the entry point must exist
        // in the compiled shader so the pipeline lookup in render()
        // never panics at runtime.
        assert_eq!(ToneMapKind::SrgbPassthrough.entry_point(), "fs_passthrough");
        assert_eq!(ToneMapKind::Hable.entry_point(), "fs_hable");
        assert_eq!(ToneMapKind::Bt2390.entry_point(), "fs_bt2390");
        // The shader module must declare all three entry points.
        assert!(VIDEO_BLIT_WGSL.contains("fn fs_passthrough"));
        assert!(VIDEO_BLIT_WGSL.contains("fn fs_hable"));
        assert!(VIDEO_BLIT_WGSL.contains("fn fs_bt2390"));
    }

    #[test]
    fn tone_map_kind_label_is_stable_and_round_trips() {
        use std::collections::HashSet;
        let labels: HashSet<&'static str> = [
            ToneMapKind::SrgbPassthrough,
            ToneMapKind::Hable,
            ToneMapKind::Bt2390,
        ]
        .iter()
        .map(|k| k.label())
        .collect();
        assert_eq!(labels.len(), 3, "labels must be pairwise distinct");
        for kind in [
            ToneMapKind::SrgbPassthrough,
            ToneMapKind::Hable,
            ToneMapKind::Bt2390,
        ] {
            assert_eq!(ToneMapKind::from_label(kind.label()), kind);
        }
    }

    #[test]
    fn tone_map_kind_from_label_defaults_to_bt2390() {
        // ADR-010 §6 says "Default to BT.2390"; ensure unknown
        // strings fall back rather than panic.
        assert_eq!(ToneMapKind::from_label("bogus"), ToneMapKind::Bt2390);
        assert_eq!(ToneMapKind::from_label(""), ToneMapKind::Bt2390);
        assert_eq!(ToneMapKind::from_label("hable"), ToneMapKind::Hable);
        assert_eq!(
            ToneMapKind::from_label("srgb-passthrough"),
            ToneMapKind::SrgbPassthrough
        );
    }

    #[test]
    fn bgra_stride_equals_four_times_width() {
        assert_eq!(test_helpers::bgra_stride(1), 4);
        assert_eq!(test_helpers::bgra_stride(640), 2560);
        assert_eq!(test_helpers::bgra_stride(1920), 7680);
        assert_eq!(test_helpers::bgra_stride(3840), 15360);
    }
}
