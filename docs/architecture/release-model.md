# Royal Linux Release Model

## Overview

Royal Linux will follow a rolling-release model.

The system will receive continuous package updates rather than relying on fixed major-version releases.

## Release Strategy

Royal will use staged package promotion to balance flexibility and reliability.

### Repository Stages

**Testing**
- Receives newly built packages.
- Runs automated package tests.
- Identifies dependency and compatibility issues.
- Allows early testing before official release.

**Official**
- Contains packages approved for general use.
- Receives packages that pass the required testing process.
- Prioritizes reliability and system compatibility.

## Package Promotion

The planned promotion process:

1. Package is submitted or updated.
2. Build infrastructure creates the package.
3. Automated tests are performed.
4. Maintainer review is conducted when required.
5. Approved packages are promoted to Official.

## Update Safety

Royal aims to reduce the risk of failed system upgrades.

Planned safeguards:

- Automatic Btrfs snapshots before upgrades.
- Package integrity and signature verification.
- Dependency conflict detection.
- Upgrade testing.
- Recovery support for failed upgrades.

## Architecture Support

Initial supported architectures:

- x86_64
- ARM64

Packages must be built and tested for their intended architecture.

## Design Goals

- Continuous updates.
- Reliable package promotion.
- Minimal unnecessary delays.
- Clear separation between testing and official software.
- Consistent behavior across supported architectures.