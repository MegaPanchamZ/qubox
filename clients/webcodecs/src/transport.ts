export class Transport {
  public transport?: WebTransport;
  public controlWriter?: WritableStreamDefaultWriter<Uint8Array>;
  public datagramReader?: ReadableStreamDefaultReader<Uint8Array>;

  async connect(ticketBase64: string) {
    // Parse the ticket
    // Format assumed: JSON base64 with { url, hashes: string[] }
    const ticketStr = atob(ticketBase64);
    const ticket = JSON.parse(ticketStr);
    
    const hashes = ticket.hashes.map((hash: string) => {
      const bytes = new Uint8Array(hash.match(/.{1,2}/g)!.map(byte => parseInt(byte, 16)));
      return {
        algorithm: 'sha-256',
        value: bytes as unknown as BufferSource
      };
    });
    
    this.transport = new WebTransport(ticket.url, {
      serverCertificateHashes: hashes
    });
    
    await this.transport.ready;
    console.log('WebTransport connected');

    // Create bidirectional stream for control
    const controlStream = await this.transport.createBidirectionalStream();
    this.controlWriter = controlStream.writable.getWriter();
    
    // Setup datagram reader
    this.datagramReader = this.transport.datagrams.readable.getReader();
  }

  async readDatagrams(sink: WritableStream<Uint8Array>) {
    if (!this.datagramReader) return;
    const writer = sink.getWriter();
    try {
      while (true) {
        const { value, done } = await this.datagramReader.read();
        if (done) break;
        if (value) {
          writer.write(value);
        }
      }
    } catch (e) {
      console.error('Datagram read error', e);
    } finally {
      writer.releaseLock();
    }
  }
}
