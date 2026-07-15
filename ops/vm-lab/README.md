# VM Lab

Tracked VM automation lives in `ops/vm-lab/`.

Generated VM state stays under `.local/vm-lab/`, which remains ignored by git.

The earlier QEMU-based lab workflow has been removed.

The tracked local VM workflow now targets VirtualBox using Ubuntu's published Jammy cloud-image OVA.

## VirtualBox Jammy Workflow

Prepare the VirtualBox appliance, generate a cloud-init seed ISO, configure NAT for guest internet plus SSH bootstrap, and attach a host-only adapter for native QUIC media:

```powershell
.\ops\vm-lab\virtualbox-jammy-cloud.ps1 -Action prepare
```

Start the VM with a GUI window:

```powershell
.\ops\vm-lab\virtualbox-jammy-cloud.ps1 -Action start
```

Check status:

```powershell
.\ops\vm-lab\virtualbox-jammy-cloud.ps1 -Action status
```

Stop or destroy the VM:

```powershell
.\ops\vm-lab\virtualbox-jammy-cloud.ps1 -Action stop
.\ops\vm-lab\virtualbox-jammy-cloud.ps1 -Action destroy
```

Install Qubox inside the guest and run the end-to-end handoff test:

```powershell
.\ops\vm-lab\virtualbox-jammy-cloud.ps1 -Action install
.\ops\vm-lab\virtualbox-jammy-cloud.ps1 -Action test
```

To validate Linux host capture inside the guest before a full handoff, run a smoke test that auto-selects PipeWire or X11 and writes a real `.h264` artifact:

```bash
cd /home/ubuntu/src/qubox
DISPLAY=:0 XAUTHORITY=/home/ubuntu/.Xauthority ./target/debug/host-agent --smoke-test --linux-capture auto --x11-display :0.0 --h264-encoder libx264 --max-media-frames 60 --smoke-test-output /home/ubuntu/linux-host-smoke.h264
```

## Notes

- The workflow downloads `jammy-server-cloudimg-amd64.ova` from Ubuntu and imports it into VirtualBox under `.local/vm-lab/virtualbox-jammy-cloud/`.
- A cloud-init seed ISO is built through the existing `Ubuntu-22.04` WSL distro using `cloud-localds`.
- The guest is configured with `ubuntu` / `ubuntu` credentials and a generated SSH key under `.local/vm-lab/virtualbox-jammy-cloud/state/`.
- Adapter 1 stays on VirtualBox NAT so the guest can install packages and accept SSH bootstrap through `127.0.0.1:2222 -> guest:22`.
- Adapter 2 uses the local `VirtualBox Host-Only Ethernet Adapter`, and the guest is pinned to `192.168.56.20/24`; the host-agent advertises that real guest IP for native QUIC instead of `127.0.0.1`.
- The VM is configured with a 1600x900 display hint, and the guest automation also attempts to apply that mode through `xrandr` after boot.
- The first boot installs a lightweight XFCE desktop through cloud-init, so reaching the graphical login or autologin session can take several minutes.
- If the first-boot desktop install is still running, the VM may show a text console first and `cloud-init status` will report `running` until package installation completes.
- The workflow forces `lightdm` instead of the cloud image's default `gdm3`, and it autostarts an `xterm` window so the guest is visibly testable even if the desktop background itself is blank.
- Guest automation uses password-based SSH through the existing `Ubuntu-22.04` WSL distro via `sshpass`, so it does not pause for interactive prompts.
- If the VM was already running before these network or resolution settings changed, run `-Action stop` and then `-Action start` once so VirtualBox reapplies the NIC and display configuration.