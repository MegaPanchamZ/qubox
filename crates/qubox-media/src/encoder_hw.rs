use async_trait::async_trait;
use qubox_display::CapturedFrame;
use crate::{EncodedVideoAccessUnit, MediaRuntimeError};
#[cfg(windows)]
use crate::HostVideoPipelineConfig;

#[async_trait]
pub trait HardwareEncoder: Send + Sync + 'static {
    #[cfg(windows)]
    async fn initialize(
        &mut self,
        device: windows::Win32::Graphics::Direct3D11::ID3D11Device,
        context: windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext,
        config: HostVideoPipelineConfig,
    ) -> Result<(), MediaRuntimeError>;

    async fn encode_frame(
        &mut self,
        frame: CapturedFrame,
    ) -> Result<EncodedVideoAccessUnit, MediaRuntimeError>;
}

pub struct InProcessFfmpegEncoder {
    #[cfg(windows)]
    encoder: Option<ffmpeg_next::encoder::Video>,
    #[cfg(windows)]
    device: Option<windows::Win32::Graphics::Direct3D11::ID3D11Device>,
    #[cfg(windows)]
    context: Option<windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext>,
    #[cfg(not(windows))]
    _unused: (),
}

unsafe impl Send for InProcessFfmpegEncoder {}
unsafe impl Sync for InProcessFfmpegEncoder {}

#[cfg(windows)]
unsafe fn bind_d3d11_hw_context(
    d3d11_device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
) -> Result<*mut ffmpeg_sys_next::AVBufferRef, MediaRuntimeError> {
    use windows::core::Interface;

    // Allocate HW device context
    let mut buf_ref = ffmpeg_sys_next::av_hwdevice_ctx_alloc(
        ffmpeg_sys_next::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA,
    );
    if buf_ref.is_null() {
        return Err(MediaRuntimeError {
            message: "av_hwdevice_ctx_alloc returned null".to_string(),
        });
    }

    // Dereference buffer reference to access AVHWDeviceContext
    let hw_device_ctx = (*buf_ref).data as *mut ffmpeg_sys_next::AVHWDeviceContext;
    if hw_device_ctx.is_null() {
        ffmpeg_sys_next::av_buffer_unref(&mut buf_ref);
        return Err(MediaRuntimeError {
            message: "AVHWDeviceContext is null".to_string(),
        });
    }

    // Cast the hwctx field to AVD3D11VADeviceContext
    let d3d11va_ctx = (*hw_device_ctx).hwctx as *mut ffmpeg_sys_next::AVD3D11VADeviceContext;
    if d3d11va_ctx.is_null() {
        ffmpeg_sys_next::av_buffer_unref(&mut buf_ref);
        return Err(MediaRuntimeError {
            message: "AVD3D11VADeviceContext is null".to_string(),
        });
    }

    // Assign the ID3D11Device pointer (cast to void* as expected by FFmpeg)
    let device_ptr = d3d11_device.as_raw();
    (*d3d11va_ctx).device = device_ptr as *mut _;

    // Initialize the hardware device context
    let ret = ffmpeg_sys_next::av_hwdevice_ctx_init(buf_ref);
    if ret < 0 {
        ffmpeg_sys_next::av_buffer_unref(&mut buf_ref);
        return Err(MediaRuntimeError {
            message: format!("av_hwdevice_ctx_init failed: {}", ret),
        });
    }

    Ok(buf_ref)
}

#[cfg(windows)]
impl InProcessFfmpegEncoder {
    pub fn new(
        d3d11_device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
        _d3d11_context: &windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext,
        config: &HostVideoPipelineConfig,
    ) -> Result<Self, MediaRuntimeError> {
        ffmpeg_next::init().map_err(|e| MediaRuntimeError {
            message: format!("FFmpeg init failed: {e}"),
        })?;

        // Find encoder
        let codec = ffmpeg_next::encoder::find(ffmpeg_next::codec::Id::H264)
            .ok_or_else(|| MediaRuntimeError {
                message: "Could not find H264 encoder".to_string(),
            })?;

        // Create context
        let context = ffmpeg_next::codec::context::Context::new_with_codec(codec);
        let mut encoder = context.encoder().video().map_err(|e| MediaRuntimeError {
            message: format!("Failed to create video encoder context: {e}"),
        })?;

        encoder.set_width(config.width);
        encoder.set_height(config.height);
        encoder.set_bit_rate(config.bitrate_kbps as usize * 1000);
        encoder.set_frame_rate(Some((config.framerate as i32, 1)));
        encoder.set_time_base((1, 1000000));
        encoder.set_format(ffmpeg_next::format::Pixel::D3D11);

        // Bind the hardware device context
        let hw_device_ctx = unsafe { bind_d3d11_hw_context(d3d11_device)? };
        
        unsafe {
            let raw_ctx = encoder.as_mut_ptr();
            (*raw_ctx).hw_device_ctx = hw_device_ctx;
        }

        // Open encoder
        let encoder = encoder.open().map_err(|e| MediaRuntimeError {
            message: format!("Failed to open encoder: {e}"),
        })?;

        Ok(Self {
            encoder: Some(encoder),
            device: Some(d3d11_device.clone()),
            context: Some(_d3d11_context.clone()),
        })
    }
}

#[cfg(not(windows))]
impl InProcessFfmpegEncoder {
    pub fn new() -> Self {
        Self { _unused: () }
    }
}

#[async_trait]
impl HardwareEncoder for InProcessFfmpegEncoder {
    #[cfg(windows)]
    async fn initialize(
        &mut self,
        device: windows::Win32::Graphics::Direct3D11::ID3D11Device,
        context: windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext,
        config: HostVideoPipelineConfig,
    ) -> Result<(), MediaRuntimeError> {
        let other = Self::new(&device, &context, &config)?;
        self.encoder = other.encoder;
        self.device = other.device;
        self.context = other.context;
        Ok(())
    }

    async fn encode_frame(
        &mut self,
        frame: CapturedFrame,
    ) -> Result<EncodedVideoAccessUnit, MediaRuntimeError> {
        match frame {
            #[cfg(windows)]
            CapturedFrame::D3D11Texture { display_id, width, height, texture: capture_tex, captured_at, frame_index, .. } => {
                use windows::core::Interface;

                let encoder = self.encoder.as_mut().ok_or_else(|| MediaRuntimeError {
                    message: "Encoder not initialized".to_string(),
                })?;
                let d3d11_context = self.context.as_ref().ok_or_else(|| MediaRuntimeError {
                    message: "D3D11 context not available".to_string(),
                })?;

                // Create an empty ffmpeg frame
                let mut ffmpeg_frame = ffmpeg_next::frame::Video::empty();

                // Allocate hardware frame buffer from the context
                let raw_ctx = unsafe { encoder.as_mut_ptr() };
                let hw_frames_ctx = unsafe { (*raw_ctx).hw_frames_ctx };
                if hw_frames_ctx.is_null() {
                    return Err(MediaRuntimeError {
                        message: "hw_frames_ctx is null".to_string(),
                    });
                }

                let ret = unsafe {
                    ffmpeg_sys_next::av_hwframe_get_buffer(
                        hw_frames_ctx,
                        ffmpeg_frame.as_mut_ptr(),
                        0,
                    )
                };
                if ret < 0 {
                    return Err(MediaRuntimeError {
                        message: format!("av_hwframe_get_buffer failed: {}", ret),
                    });
                }

                // The data[0] pointer of this AVFrame is actually a pointer to an ID3D11Texture2D
                let raw_frame = unsafe { ffmpeg_frame.as_ptr() };
                let ffmpeg_texture_ptr = unsafe { (*raw_frame).data[0] as *mut std::ffi::c_void };
                if ffmpeg_texture_ptr.is_null() {
                    return Err(MediaRuntimeError {
                        message: "AVFrame data[0] is null".to_string(),
                    });
                }

                // Cast raw pointer to ID3D11Texture2D (as reference so we don't drop/release it)
                let ffmpeg_texture: &windows::Win32::Graphics::Direct3D11::ID3D11Texture2D = unsafe {
                    std::mem::transmute(&ffmpeg_texture_ptr)
                };

                // Cast to ID3D11Resource for CopyResource
                let dst_resource: windows::Win32::Graphics::Direct3D11::ID3D11Resource = ffmpeg_texture
                    .cast()
                    .map_err(|e| MediaRuntimeError {
                        message: format!("Failed to cast destination texture to ID3D11Resource: {e}"),
                    })?;
                let src_resource: windows::Win32::Graphics::Direct3D11::ID3D11Resource = capture_tex
                    .cast()
                    .map_err(|e| MediaRuntimeError {
                        message: format!("Failed to cast source texture to ID3D11Resource: {e}"),
                    })?;

                // Perform GPU-to-GPU copy
                unsafe {
                    d3d11_context.CopyResource(&dst_resource, &src_resource);
                }

                // Send frame to encoder
                encoder.send_frame(&ffmpeg_frame).map_err(|e| MediaRuntimeError {
                    message: format!("send_frame failed: {e}"),
                })?;

                // Receive packet
                let mut packet = ffmpeg_next::Packet::empty();
                match encoder.receive_packet(&mut packet) {
                    Ok(()) => {
                        let bytes = packet.data().unwrap_or(&[]).to_vec();
                        let timestamp_micros = captured_at.elapsed().as_micros() as u64;

                        Ok(EncodedVideoAccessUnit {
                            codec: qubox_proto::VideoCodec::H264,
                            frame_id: frame_index,
                            timestamp_micros,
                            keyframe: packet.is_key(),
                            nal_units: vec![],
                            bytes,
                            display_id: display_id.0,
                            stream_id: 0,
                            width,
                            height,
                            color_space: None,
                            bit_depth: 8,
                        })
                    }
                    Err(ffmpeg_next::Error::Other { errno: 11 }) => {
                        // Return empty/placeholder indicating more input required
                        let timestamp_micros = captured_at.elapsed().as_micros() as u64;
                        Ok(EncodedVideoAccessUnit {
                            codec: qubox_proto::VideoCodec::H264,
                            frame_id: frame_index,
                            timestamp_micros,
                            keyframe: false,
                            nal_units: vec![],
                            bytes: vec![],
                            display_id: display_id.0,
                            stream_id: 0,
                            width,
                            height,
                            color_space: None,
                            bit_depth: 8,
                        })
                    }
                    Err(e) => {
                        Err(MediaRuntimeError {
                            message: format!("receive_packet failed: {e}"),
                        })
                    }
                }
            }
            CapturedFrame::Cpu { .. } => {
                Err(MediaRuntimeError {
                    message: "CPU frames not supported in zero-copy pipeline".to_string(),
                })
            }
        }
    }
}