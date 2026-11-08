# Royal Linux Architecture

## Overview

Royal Linux is an independently developed Linux distribution built around flexibility, customization, performance, and user control.

Royal will use upstream open-source software where appropriate while maintaining its own build infrastructure, package ecosystem, system configuration, and installation tools.

## Core Design

| Component | Initial Direction |
|---|---|
| Kernel | Linux |
| C library | glibc |
| Compiler | GCC |
| Init system | OpenRC |
| Package manager | Pact |
| Package manager language | Rust |
| Filesystem | Btrfs |
| Bootloader | GRUB |
| Default shell | Bash |
| Build foundation | LFS / BLFS |

## Distribution Model

- Independent distribution development.
- Rolling-release updates.
- Testing repository before official promotion.
- Binary packages with optional source builds.
- Configurable package features.
- Minimal default installation.
- Optional desktop environments.

## Supported Architectures

Initial targets:
- x86_64
- ARM64

## Package Ecosystem

Royal will maintain its own package format, package definitions, repositories, and package management infrastructure.

Pact will be developed independently in its own repository.

## Development Philosophy

Royal prioritizes:

- Flexibility over unnecessary restrictions.
- Modularity over monolithic design.
- Performance without sacrificing functionality.
- Transparency in system configuration.
- Reliability through testing and controlled updates.

## Development Status

This document describes the initial architectural direction. Components and implementation details may evolve during development.