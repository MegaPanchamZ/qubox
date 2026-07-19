export class Input {
  canvas: HTMLCanvasElement;
  controlWriter?: WritableStreamDefaultWriter<Uint8Array>;

  constructor(canvas: HTMLCanvasElement) {
    this.canvas = canvas;
  }

  init(writer?: WritableStreamDefaultWriter<Uint8Array>) {
    this.controlWriter = writer;
    
    this.canvas.addEventListener('click', () => {
      this.canvas.requestPointerLock();
    });

    this.canvas.addEventListener('mousemove', (e) => {
      if (document.pointerLockElement === this.canvas) {
        this.sendPointerDelta(e.movementX, e.movementY);
      }
    });

    window.addEventListener('keydown', (e) => this.sendKeyEvent(e.code, true));
    window.addEventListener('keyup', (e) => this.sendKeyEvent(e.code, false));

    this.pollGamepads();
  }

  sendPointerDelta(dx: number, dy: number) {
    if (!this.controlWriter) return;
    const buf = new Uint8Array(9);
    const view = new DataView(buf.buffer);
    view.setUint8(0, 0x01);
    view.setInt32(1, dx, true);
    view.setInt32(5, dy, true);
    this.controlWriter.write(buf);
  }

  sendKeyEvent(code: string, isDown: boolean) {
    if (!this.controlWriter) return;
    const buf = new Uint8Array(6);
    const view = new DataView(buf.buffer);
    view.setUint8(0, 0x02);
    let hash = 0;
    for(let i = 0; i < code.length; i++) hash = (hash << 5) - hash + code.charCodeAt(i);
    view.setUint32(1, hash, true);
    view.setUint8(5, isDown ? 1 : 0);
    this.controlWriter.write(buf);
  }

  pollGamepads() {
    const gamepads = navigator.getGamepads();
    for (const gp of gamepads) {
      if (gp) {
        this.sendGamepadState(gp);
      }
    }
    requestAnimationFrame(() => this.pollGamepads());
  }

  sendGamepadState(gp: Gamepad) {
    if (!this.controlWriter) return;
    const buf = new Uint8Array(17);
    const view = new DataView(buf.buffer);
    view.setUint8(0, 0x03);
    
    view.setUint8(1, gp.index);
    
    let flags = 1 << 4; // connected
    if (gp.buttons[12]?.pressed) flags |= (1 << 0);
    if (gp.buttons[13]?.pressed) flags |= (1 << 1);
    if (gp.buttons[14]?.pressed) flags |= (1 << 2);
    if (gp.buttons[15]?.pressed) flags |= (1 << 3);
    view.setUint8(2, flags);

    let lo = 0;
    if (gp.buttons[0]?.pressed) lo |= (1 << 0);
    if (gp.buttons[1]?.pressed) lo |= (1 << 1);
    if (gp.buttons[2]?.pressed) lo |= (1 << 2);
    if (gp.buttons[3]?.pressed) lo |= (1 << 3);
    if (gp.buttons[4]?.pressed) lo |= (1 << 4);
    if (gp.buttons[5]?.pressed) lo |= (1 << 5);
    if (gp.buttons[8]?.pressed) lo |= (1 << 6);
    if (gp.buttons[9]?.pressed) lo |= (1 << 7);
    view.setUint8(3, lo);

    let hi = 0;
    if (gp.buttons[10]?.pressed) hi |= (1 << 0);
    if (gp.buttons[11]?.pressed) hi |= (1 << 1);
    if (gp.buttons[16]?.pressed) hi |= (1 << 2);
    view.setUint8(4, hi);

    const lt = (gp.buttons[6]?.value || 0) * 255;
    const rt = (gp.buttons[7]?.value || 0) * 255;
    view.setUint8(5, lt);
    view.setUint8(6, rt);

    const lx = (gp.axes[0] || 0) * 32767;
    const ly = (gp.axes[1] || 0) * 32767;
    const rx = (gp.axes[2] || 0) * 32767;
    const ry = (gp.axes[3] || 0) * 32767;
    view.setInt16(7, lx, true);
    view.setInt16(9, ly, true);
    view.setInt16(11, rx, true);
    view.setInt16(13, ry, true);

    view.setUint8(15, 0);
    view.setUint8(16, 0);

    this.controlWriter.write(buf).catch(console.error);
  }
}
