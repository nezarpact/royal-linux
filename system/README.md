# Royal Linux System

This directory contains Royal Linux's base system configuration and integration.

## Components

- Kernel configuration
- Init system configuration
- System services
- Filesystem configuration
- User and permission management
- System defaults

## Design Goals

- Minimal base installation.
- Modular system components.
- OpenRC as the initial init system.
- Btrfs filesystem support.
- User control over installed services.
- Minimal unnecessary dependencies.

## Philosophy

The Royal base system should provide everything necessary for a functional operating system without forcing users to install components they don't need.