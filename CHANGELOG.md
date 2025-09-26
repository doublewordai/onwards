# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
