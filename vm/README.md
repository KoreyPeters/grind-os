<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->

# Test VM

Phase 0, milestone 1. A small Linux guest under QEMU/KVM with a q35 machine,
an emulated Intel IOMMU (with interrupt remapping) and an emulated e1000e.
Inside the guest, the e1000e is unbound from Linux's `e1000e` driver and
bound to `vfio-pci`.

```sh
vm/fetch.sh            # download and verify pinned packages into vm/.cache/
vm/build-initramfs.sh  # build vm/build/vmlinuz and vm/build/initramfs.gz
vm/run.sh --check      # boot headless; exit 0 only if every check passes
vm/run.sh              # boot with a shell on this terminal (Ctrl-A x quits QEMU)
```

A check boot takes about two seconds.

To run a program in the guest against the e1000e after the checks pass:

```sh
vm/build-initramfs.sh --add path/to/static-binary=/bin/prog
vm/run.sh --check --run /bin/prog    # init runs "/bin/prog <pci-address>"
```

The program must be statically linked, because the guest has no shared
libraries. Its exit status is reported as step `run`, and any
`<PREFIX> step=... result=fail ...` lines it prints are shown in
`run.sh`'s failure summary. `hello/check.sh` uses this mechanism.

## Prerequisites

- `qemu-system-x86_64`, `curl`, `dpkg-deb`, `depmod`/`modprobe` (kmod), `cpio`, `gzip`.
- Read/write access to `/dev/kvm`. WSL often creates it as `root:root 0600`,
  which shuts out the `kvm` group. Fix it with:

  ```sh
  sudo chown root:kvm /dev/kvm && sudo chmod 660 /dev/kvm
  ```

  WSL does not reliably run udev rules, so to make this survive a WSL
  restart, put the command in `/etc/wsl.conf`:

  ```ini
  [boot]
  command = chown root:kvm /dev/kvm && chmod 660 /dev/kvm
  ```

## How it works

| File | Role |
| --- | --- |
| `SOURCES` | Pinned inputs: name, sha256, URL |
| `fetch.sh` | Downloads, verifies and unpacks `SOURCES` into `.cache/` |
| `build-initramfs.sh` | Packs busybox, the needed kernel modules and `init` into a reproducible initramfs |
| `init` | Guest PID 1: runs the checks, then powers off (`--check`) or opens a shell |
| `run.sh` | Boots QEMU and, in check mode, turns the guest's output into an exit code |

The guest is a Debian 12 kernel and a static busybox, booted directly with
`-kernel`/`-initrd`. There is no disk image and no state carried between boots.

**Inputs** come from `snapshot.debian.org`'s content-addressed `/file/<sha1>`
URLs, so they stay available after Debian updates the live archive. A hash
mismatch stops the fetch. To move to a newer kernel, update `SOURCES` with
the new package's snapshot URL and the sha256 from Debian's `Packages` index.

**QEMU command line** (see `run.sh`): `-machine q35,accel=kvm,kernel-irqchip=split`,
`-device intel-iommu,intremap=on` (listed before the devices it translates
for), `-device e1000e` on a user-mode network backend, kernel command line
`console=ttyS0 intel_iommu=on`.

## Checks

`init` prints one line per step and stops at the first failure:

```
VMCHECK step=<step> result=pass|fail expected="..." got="..."
VMCHECK result=pass
VMCHECK result=fail failed_step=<step>
```

| Step | Passes when |
| --- | --- |
| `iommu` | `/sys/class/iommu` has a DMAR unit |
| `irq-remap` | dmesg reports interrupt remapping enabled |
| `modules` | `e1000e`, `vfio-pci` and `vfio_iommu_type1` load |
| `device` | exactly one `8086:10d3` (82574L) is on the PCI bus |
| `iommu-group` | that device is alone in its IOMMU group |
| `bound-e1000e` | Linux's `e1000e` driver claimed it |
| `unbind` | it is unbound from `e1000e` |
| `bind-vfio` | `driver_override` plus `drivers_probe` binds it to `vfio-pci` |
| `vfio-dev` | `/dev/vfio/vfio` and `/dev/vfio/<group>` exist |
| `run` | only with `--run`: the program exits 0 |

Host-side problems (missing KVM access, missing build files, timeout, no
result line) are reported by `run.sh` as `VMRUN step=... expected=... got=...`.
The full serial log goes to `vm/build/serial.log`.

## Notes from building it

- Alpine's `linux-virt` kernel was rejected because it is built without VFIO.
- Debian's 6.1 kernel enables Intel DMA remapping by default, so the guest
  passes even without `intel_iommu=on`. The flag is kept so the VM doesn't
  depend on a distro default. Booting with `intel_iommu=off` fails at the
  `iommu` step, as it should.
- Verified with QEMU 4.2.1 (Ubuntu 20.04). This version is old, and its
  e1000e and vIOMMU emulation lack years of upstream fixes. Upgrade it
  before drivers are developed against it (milestone 4).
