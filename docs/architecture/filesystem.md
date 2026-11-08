# Royal Linux Filesystem Hierarchy

## Overview

Royal Linux will follow the Linux Filesystem Hierarchy Standard (FHS) where practical while maintaining control over its own system organization.

The filesystem should remain predictable, modular, and easy to maintain.

## Root Directories

| Directory | Purpose |
|---|---|
| `/boot` | Kernel and bootloader files |
| `/dev` | Device interfaces |
| `/etc` | System configuration |
| `/home` | User directories |
| `/lib` | Essential shared libraries and system components |
| `/opt` | Optional software |
| `/proc` | Kernel and process information |
| `/root` | Root user's home directory |
| `/run` | Runtime system data |
| `/sys` | Kernel and hardware interfaces |
| `/tmp` | Temporary files |
| `/usr` | Applications, libraries, and shared resources |
| `/var` | Logs, caches, and variable system data |

## Royal Configuration

Royal-specific configuration will primarily live under:

- `/etc/royal/`

This directory will contain distribution-specific configuration and system defaults.

## Filesystem Support

Btrfs is the initial preferred filesystem.

Planned capabilities:

- System snapshots.
- Snapshot integration with Pact.
- Recovery support following failed upgrades.
- Optional user-managed snapshots.

## Design Goals

- Follow established Linux conventions.
- Avoid unnecessary filesystem modifications.
- Keep distribution-specific configuration organized.
- Support reliable upgrades and recovery.
- Allow users to customize their systems.