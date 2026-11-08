# Royal Linux Bootstrap

This directory contains the tools, scripts, and configuration required to bootstrap the initial Royal Linux system.

## Goals

- Establish a controlled build environment.
- Build the initial toolchain.
- Construct the initial root filesystem.
- Minimize dependencies on the host distribution.
- Provide a repeatable bootstrap process.

## Foundation

The initial bootstrap process will use Linux From Scratch (LFS) as a technical reference and starting point.

LFS is a bootstrap method, not Royal Linux's permanent package management system.