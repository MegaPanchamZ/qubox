import { FEC } from './fec.js';

interface FrameBuffer {
  shards: Uint8Array[];
  available: boolean[];
  received: number;
  total: number;
  timer?: ReturnType<typeof setTimeout>;
}

export class FecTransform {
  fec: FEC;
  buffers: Map<number, FrameBuffer> = new Map();

  constructor(fec: FEC) {
    this.fec = fec;
  }

  transform(): TransformStream<Uint8Array, Uint8Array> {
    return new TransformStream({
      transform: (chunk, controller) => {
        // Assume simple header: [4 bytes frame_id][1 byte shard_index][1 byte total_shards][...payload]
        if (chunk.length < 6) return;
        const view = new DataView(chunk.buffer, chunk.byteOffset, chunk.byteLength);
        const frameId = view.getUint32(0, true);
        const shardIdx = view.getUint8(4);
        const totalShards = view.getUint8(5);
        const payload = chunk.slice(6);

        let buf = this.buffers.get(frameId);
        if (!buf) {
          buf = {
            shards: new Array(totalShards),
            available: new Array(totalShards).fill(false),
            received: 0,
            total: totalShards,
            timer: setTimeout(() => this.flush(frameId, controller), 50)
          };
          this.buffers.set(frameId, buf);
        }

        if (!buf.available[shardIdx]) {
          buf.shards[shardIdx] = payload;
          buf.available[shardIdx] = true;
          buf.received++;
        }

        // If we have enough shards to reconstruct (received >= dataShards)
        // For simplicity, just flush when fully received or on timeout.
        if (buf.received >= this.fec.dataShards) {
          this.flush(frameId, controller);
        }
      }
    });
  }

  flush(frameId: number, controller: TransformStreamDefaultController<Uint8Array>) {
    const buf = this.buffers.get(frameId);
    if (!buf) return;
    clearTimeout(buf.timer);
    this.buffers.delete(frameId);

    try {
      const payloadSize = buf.shards.find(s => s)?.length || 0;
      if (!payloadSize) return;

      const continuousBuffer = new Uint8Array(payloadSize * buf.total);
      for (let i = 0; i < buf.total; i++) {
        if (buf.available[i] && buf.shards[i]) {
          continuousBuffer.set(buf.shards[i], i * payloadSize);
        }
      }

      this.fec.reconstruct(continuousBuffer, buf.available);

      // Extract the original data chunks (dataShards * payloadSize)
      const dataSize = payloadSize * this.fec.dataShards;
      controller.enqueue(continuousBuffer.slice(0, dataSize));
    } catch (e) {
      console.error('FEC reconstruct failed', e);
    }
  }
}
