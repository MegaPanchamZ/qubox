import ReedSolomonErasure from '@digitaldefiance/reed-solomon-erasure.wasm';

export class FEC {
  rs?: ReedSolomonErasure;
  dataShards: number = 0;
  parityShards: number = 0;

  async init(dataShards: number, parityShards: number) {
    this.dataShards = dataShards;
    this.parityShards = parityShards;
    this.rs = await ReedSolomonErasure.getInstance();
  }

  reconstruct(shards: Uint8Array, shardAvailable: boolean[]) {
    if (!this.rs) throw new Error('FEC not initialized');
    this.rs.reconstruct(shards, this.dataShards, this.parityShards, shardAvailable);
  }
}
