export class Codec {
  decoder?: VideoDecoder;
  onFrame?: (frame: VideoFrame) => void;

  init(onFrame: (frame: VideoFrame) => void) {
    this.onFrame = onFrame;
    this.decoder = new VideoDecoder({
      output: (frame) => this.onFrame?.(frame),
      error: (e) => console.error('Decoder error:', e)
    });
  }

  async configure(type: 'h264' | 'hevc' | 'av1') {
    let codec = '';
    switch (type) {
      case 'h264': codec = 'avc1.42E01E'; break;
      case 'hevc': codec = 'hev1.1.6.L93.B0'; break;
      case 'av1': codec = 'av01.0.01M.08'; break;
    }

    const config: VideoDecoderConfig = {
      codec,
      hardwareAcceleration: 'prefer-hardware'
    };

    const support = await VideoDecoder.isConfigSupported(config);
    if (!support.supported) {
      console.warn(`Codec config not supported for ${type}`, config);
    }
    
    this.decoder?.configure(config);
  }

  decode(chunk: EncodedVideoChunk) {
    if (this.decoder?.state === 'configured') {
      this.decoder.decode(chunk);
    }
  }
}
