# Royal Linux Boot Process

## Overview

Royal Linux will use a traditional Linux boot process with GRUB as its initial bootloader and OpenRC as its init system.

## Boot Sequence

The planned boot sequence is:

1. **Firmware** — UEFI initializes the hardware.
2. **GRUB** — Loads the Linux kernel and initramfs.
3. **Kernel** — Initializes hardware, memory, and essential kernel subsystems.
4. **Initramfs** — Prepares the root filesystem and mounts it.
5. **OpenRC** — Starts and manages system services.
6. **Login** — Presents the user with a login prompt or configured graphical login manager.

## Bootloader

Initial bootloader: GRUB.

Planned responsibilities:
- Boot menu management.
- Kernel selection.
- Recovery boot entries.
- Support for UEFI systems.

## Init System

Initial init system: OpenRC.

OpenRC will manage:
- System initialization.
- Service startup and shutdown.
- Service dependencies.
- Runlevels.

## Initramfs

Royal will provide an initramfs to prepare the system before switching to the real root filesystem.

It must support the storage and filesystem configurations required by Royal, including Btrfs where applicable.

## Recovery

Royal should eventually support:
- Booting into recovery environments.
- Selecting previous kernels.
- Recovering from failed system upgrades.
- Btrfs snapshot-based recovery.

## Design Goals

- Predictable startup behavior.
- Minimal unnecessary background services.
- Reliable recovery options.
- Support for both x86_64 and ARM64.