# Royal Linux Package Ecosystem

## Overview

Royal Linux will maintain its own package ecosystem, including a package manager, package format, repositories, and build definitions.

The ecosystem will initially use upstream software where practical while progressively developing Royal-controlled infrastructure.

## Package Manager

Name: Pact

Language: Rust

Pact will be developed independently in its own Git repository.

### Planned capabilities

- Install, remove, and update packages.
- Resolve dependencies.
- Display required changes before installation.
- Verify package signatures.
- Support package feature flags.
- Create Btrfs snapshots before system upgrades.
- Support binary packages and optional source builds.

## Package Format

Royal will develop its own package format.

The format should support:

- Package metadata.
- Version information.
- Dependencies.
- Installation files.
- Integrity verification.
- Package signatures.
- Compression using Zstandard.

## Package Sources

Royal packages will primarily be distributed as prebuilt binaries.

Source builds will be available as an alternative through configurable feature flags.

## Feature Flags

Pact will support:

- Global feature defaults.
- Individual package overrides.
- Configurable build options.

## Repositories

Royal will maintain separate repositories for different stages of package stability.

Initial repository structure:

- Testing: Newly built and updated packages.
- Official: Packages approved for general use.

Packages must pass automated testing before promotion. Important packages and significant changes will also receive maintainer review.

## Security

- Official packages must be signed.
- Package integrity must be verified.
- Unsigned community and locally built packages require explicit user permission.
- Source builds should run in isolated environments.

## Long-Term Goals

- Reduce dependence on upstream package infrastructure.
- Maintain Royal-controlled package definitions.
- Develop independent binary build infrastructure.
- Make Pact fast, flexible, and reliable.