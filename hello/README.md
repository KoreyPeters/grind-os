<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->

# vfio-hello

Phase 0, milestone 2. A tiny native Rust program, run inside the test VM,
that opens the e1000e through VFIO and reads one register.

```sh
vm/fetch.sh        # once
hello/check.sh     # build, add to the initramfs, boot, run; exit 0 on success
```

## What it does

Each step prints `VFIO step=<step> result=pass|fail expected="..." got="..."`,
and the program stops at the first failure:

1. `group`: finds the device's IOMMU group in sysfs.
2. `open-container`, `api-version`, `type1v2`: opens `/dev/vfio/vfio` and checks
   the API version and Type1 v2 IOMMU support.
3. `open-group`, `group-viable`, `set-container`, `set-iommu`: opens the group,
   checks that it is viable, attaches it to the container and selects the
   Type1 v2 IOMMU. From this point the device's DMA is confined to the
   container's (still empty) IOMMU mappings.
4. `device-fd`, `device-info`: gets the device file descriptor and checks that
   it is a PCI device.
5. `config-region`, `config-id`: reads config dword 0 and checks for
   `0x10d38086`, the 82574L (the model QEMU's e1000e emulates).
6. `bar0-region`, `read-status`: reads the **Device Status register (STATUS),
   BAR0 offset 0x00008**, with `pread` on the BAR0 region. It is read-only and
   reading it has no side effects. The pass condition is a value other than
   `0xffffffff`, since all ones means the device did not respond. The value
   itself is not interpreted.

Observed in the test VM with QEMU 4.2.1: `STATUS = 0x00000281`.

## Build notes

- No crate dependencies: `std` plus a declaration of `ioctl(2)`. `unsafe` is
  limited to the three ioctl wrappers and adopting the returned device fd.
  This is trusted native code; drivers stay under rule 2 and never use `unsafe`.
- glibc is linked statically (`-C target-feature=+crt-static` with an explicit
  `--target`), so no musl toolchain is needed. `check.sh` uses `readelf` to
  confirm there is no interpreter and no `NEEDED` libraries.

## Sources

- VFIO ioctl numbers and struct layouts: the Linux userspace API header
  `<linux/vfio.h>` (GPL-2.0 WITH Linux-syscall-note, whose note permits use by
  userspace programs) and the kernel's VFIO documentation. The values were
  checked by compiling a C program against the host's header and printing them.
- STATUS offset and the 82574L device ID: Intel 82574 GbE Controller Family
  datasheet.
- No Linux driver code was consulted.
