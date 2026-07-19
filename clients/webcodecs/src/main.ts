import { Transport } from './transport.js';
import { Codec } from './codec.js';
import { FEC } from './fec.js';
import { Render } from './render.js';
import { Input } from './input.js';
import { FecTransform } from './fecTransform.js';

async function main() {
  const canvas = document.getElementById('renderCanvas') as HTMLCanvasElement;
  
  const transport = new Transport();
  const codec = new Codec();
  const fec = new FEC();
  const render = new Render();
  const input = new Input(canvas);
  
  // Initialize FEC
  await fec.init(10, 2);
  const fecTransform = new FecTransform(fec);

  render.init(canvas);
  
  // Setup Codec output to Render
  codec.init((frame) => render.drawFrame(frame));
  await codec.configure('h264');

  // Parse ticket from URL hash or fallback
  const ticket = location.hash.slice(1) || btoa(JSON.stringify({ 
    url: 'https://localhost:4433', 
    hashes: ['0000000000000000000000000000000000000000000000000000000000000000'] 
  }));

  await transport.connect(ticket);
  
  // Initialize Input with WebTransport control stream
  input.init(transport.controlWriter);

  // Wire datagrams -> FEC -> Codec
  const codecSink = new WritableStream<Uint8Array>({
    write: (chunk) => {
      // Very basic Chunk instantiation - in reality needs more metadata
      const type = (chunk[0] & 0x01) === 0 ? 'key' : 'delta';
      const encodedChunk = new EncodedVideoChunk({
        type,
        timestamp: performance.now() * 1000,
        data: chunk
      });
      codec.decode(encodedChunk);
    }
  });

  const fecStream = fecTransform.transform();
  fecStream.readable.pipeTo(codecSink);
  
  // Start reading
  transport.readDatagrams(fecStream.writable).catch(console.error);

  console.log('App initialized and wired');
}

main().catch(console.error);
