# Changelog

Changes are grouped by user-visible behavior. Repository releases use annotated
`vMAJOR.MINOR.PATCH` tags with `-alpha.N`, `-beta.N` or `-rc.N` prerelease suffixes.
Internal crate versions are implementation versions and may differ from the
repository release version.

## Unreleased

- Build guide, release process, note template and automatic note categories.

## v0.1.0-alpha.1 — 2026-10-16

Source-only development prerelease at `e9f0e4246c5944b154ff15529e2636bc2db036b3`.

### Added

- Imported historical image and literal tool/result cards with bounded image
  decoding, source receipts and stale-navigation rejection.
- Eight native-host build lanes covering backend and desktop UI on Linux x64,
  Windows x64, macOS Apple silicon and macOS Intel.
- Platform-specific inference choices and visible handling of incompatible
  saved selections.

### Fixed

- Removed unsupported WSL2 provisioning, winget and Windows startup controls.
- Removed the oversized global toolbar; kept compact chat actions and draft recovery.
- Corrected the Windows backend compilation boundary for Unix-only transcription.

### Validation and limitations

All eight [build jobs passed](https://github.com/lazarevtill/Smith/actions/runs/38894937123).
Build success does not qualify desktop startup, installers or GPU execution.
Windows process/storage adapters and Linux/Windows dialogs/clipboard remain
incomplete. No installable binaries or model weights accompany this release.
See [platform readiness](docs/PLATFORM-READINESS.md) and [license status](LICENSE-STATUS.md).
