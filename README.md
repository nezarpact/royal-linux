# Royal Linux

**An independently developed Linux distribution.**

Royal Linux is a Linux distribution built around flexibility, customization, and user control.

The goal is to create a complete distribution with its own build infrastructure, package management system, repositories, installation tools, and system configuration.

## Philosophy

Royal Linux aims to provide:

- Flexibility without unnecessary complexity.
- A lightweight and customizable base system.
- A rolling-release development model.
- User choice over desktop environments and system components.
- Reliable package management and system updates.

## Architecture

Royal Linux is built using upstream Linux software, assembled and maintained through Royal's own distribution infrastructure.

| Component | Technology |
|---|---|
| Foundation | Linux From Scratch (LFS) |
| Init system | OpenRC |
| Package manager | Pact |
| Package manager language | Rust |
| C library | glibc |
| Compiler | GCC |
| Filesystem | Btrfs |
| Bootloader | GRUB |

## Development

Royal Linux is currently in its early development stage.

The initial focus is establishing the build infrastructure, constructing the base system, and developing the Royal package ecosystem.

## Project Goals

- [ ] Establish the Royal build system.
- [ ] Build the Royal base system.
- [ ] Develop Pact.
- [ ] Establish official package repositories.
- [ ] Develop the Royal installer.
- [ ] Create a bootable Royal Linux ISO.

## License

License to be determined.
