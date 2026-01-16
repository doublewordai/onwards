# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.11.0](https://github.com/doublewordai/onwards/compare/v0.10.1...v0.11.0) - 2026-01-16

### Added

- [**breaking**] chat completion sanitisation ([#59](https://github.com/doublewordai/onwards/pull/59))

## [0.10.1](https://github.com/doublewordai/onwards/compare/v0.10.0...v0.10.1) - 2026-01-15

### Added

- return 502 Bad Gateway for empty provider pools

## [0.10.0](https://github.com/doublewordai/onwards/compare/v0.9.2...v0.10.0) - 2026-01-13

### Added

- Add load balancing for multiple downstream providers ([#47](https://github.com/doublewordai/onwards/pull/47))

### Other

- *(deps)* update rust crate hyper to v1.8.1 ([#46](https://github.com/doublewordai/onwards/pull/46))
- add fallback and pool-level configuration documentation ([#48](https://github.com/doublewordai/onwards/pull/48))
- *(deps)* update rust crate anyhow to v1.0.100 ([#34](https://github.com/doublewordai/onwards/pull/34))
- *(deps)* update rust crate async-trait to v0.1.89 ([#35](https://github.com/doublewordai/onwards/pull/35))
- Add renovate.json ([#32](https://github.com/doublewordai/onwards/pull/32))

## [0.9.2](https://github.com/doublewordai/onwards/compare/v0.9.1...v0.9.2) - 2025-11-14

### Added

- global and per-key concurrency limits

### Other

- Merge branch 'main' of https://github.com/doublewordai/onwards

## [0.9.1](https://github.com/doublewordai/onwards/compare/v0.9.0...v0.9.1) - 2025-11-06

### Added

- add response headers ([#29](https://github.com/doublewordai/onwards/pull/29))

## [0.9.0](https://github.com/doublewordai/onwards/compare/v0.8.4...v0.9.0) - 2025-10-30

### Added

- configurable auth header names

## [0.8.4](https://github.com/doublewordai/onwards/compare/v0.8.3...v0.8.4) - 2025-10-27

### Added

- strip matching prefixes

## [0.8.3](https://github.com/doublewordai/onwards/compare/v0.8.2...v0.8.3) - 2025-10-27

### Added

- normalize all urls

## [0.8.2](https://github.com/doublewordai/onwards/compare/v0.8.1...v0.8.2) - 2025-10-17

### Added

- deal with incoming headers properly

## [0.8.1](https://github.com/doublewordai/onwards/compare/v0.8.0...v0.8.1) - 2025-09-26

### Other

- fix missing import in doctest

## [0.8.0](https://github.com/doublewordai/onwards/compare/v0.7.1...v0.8.0) - 2025-09-26

### Added

- add body transformation with path parameter ([#21](https://github.com/doublewordai/onwards/pull/21))

### Other

- update documentation

## [0.7.1](https://github.com/doublewordai/onwards/compare/v0.7.0...v0.7.1) - 2025-09-25

### Fixed

- add builder for Auth

## [0.7.0](https://github.com/doublewordai/onwards/compare/v0.6.1...v0.7.0) - 2025-09-25

### Added

- add per key limits option ([#19](https://github.com/doublewordai/onwards/pull/19))

## [0.6.1](https://github.com/doublewordai/onwards/compare/v0.6.0...v0.6.1) - 2025-09-17

### Added

- add /models endpoint alongside /v1/models so that we can consistently proxy to endpoints (like gemini) that dont use the /v1 prefix

### Other

- Merge branch 'main' of https://github.com/doublewordai/onwards

## [0.6.0](https://github.com/doublewordai/onwards/compare/v0.5.0...v0.6.0) - 2025-09-08

### Added

- rate limits + structured errors ([#16](https://github.com/doublewordai/onwards/pull/16))

### Other

- Rename autolabel to autolabel.yaml
- Create autolabel

## [0.5.0](https://github.com/doublewordai/onwards/compare/v0.4.0...v0.5.0) - 2025-08-18

### Fixed

- filter /v1/models endpoint by bearer token permissions ([#15](https://github.com/doublewordai/onwards/pull/15))

### Other

- add release process documentation
