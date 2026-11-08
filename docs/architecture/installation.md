# Royal Linux Installation

## Overview

Royal Linux will provide a minimal live environment and a custom command-line installer.

The installation process should give users control over their system without requiring unnecessary complexity.

## Live Environment

The Royal installation ISO will boot into a live CLI environment.

The live environment should provide:

- Essential system utilities.
- Storage and partitioning tools.
- Network configuration.
- Filesystem management.
- Access to the Royal installer.

## Installer

Royal will develop its own command-line installer.

The installer will eventually handle:

- Disk selection and partitioning.
- Filesystem configuration.
- Bootloader installation.
- Base system installation.
- User account creation.
- Network configuration.
- Desktop environment selection.
- Initial system configuration.

## Installation Profiles

The default installation will be minimal.

Users will be able to select optional components, including:

- Desktop environments.
- Display managers.
- Networking tools.
- Additional system utilities.

No desktop environment will be forced into the default installation.

## Filesystem Support

Btrfs will be the initial preferred filesystem.

The installer should support appropriate filesystem and partition configurations while allowing future expansion.

## Boot Support

Initial bootloader: GRUB.

Initial target architectures:

- x86_64
- ARM64

## Design Goals

- Minimal default installation.
- User-controlled component selection.
- Clear installation steps.
- Reliable boot configuration.
- Support for different hardware configurations.
- Consistent installation behavior across supported architectures.