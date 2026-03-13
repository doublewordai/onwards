# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.20.0](https://github.com/doublewordai/onwards/compare/v0.19.2...v0.20.0) - 2026-03-13

### Added

- [**breaking**] RequestContext-based ToolExecutor trait ([#151](https://github.com/doublewordai/onwards/pull/151))

## [0.19.2](https://github.com/doublewordai/onwards/compare/v0.19.1...v0.19.2) - 2026-03-11

### Fixed

- replace Span::enter() with .instrument() in async retry loop ([#145](https://github.com/doublewordai/onwards/pull/145))

## [0.19.1](https://github.com/doublewordai/onwards/compare/v0.19.0...v0.19.1) - 2026-03-11

### Fixed

- use reqwest-blocking-client for OTLP batch export ([#144](https://github.com/doublewordai/onwards/pull/144))

### Other

- release v0.19.0 ([#140](https://github.com/doublewordai/onwards/pull/140))

## [0.19.0](https://github.com/doublewordai/onwards/compare/v0.18.6...v0.19.0) - 2026-03-11

### Added

- otel tracing: clean commit with only relevant files ([#130](https://github.com/doublewordai/onwards/pull/130))

### Fixed

- *(errors)* correct forbidden error response body ([#138](https://github.com/doublewordai/onwards/pull/138))

### Other

- *(deps)* update docker/login-action action to v4 ([#123](https://github.com/doublewordai/onwards/pull/123))
- *(deps)* update docker/build-push-action action to v7 ([#127](https://github.com/doublewordai/onwards/pull/127))

## [0.18.6](https://github.com/doublewordai/onwards/compare/v0.18.5...v0.18.6) - 2026-03-10

### Fixed

- preserve error code in strict mode for internal errors ([#136](https://github.com/doublewordai/onwards/pull/136))

## [0.18.5](https://github.com/doublewordai/onwards/compare/v0.18.4...v0.18.5) - 2026-03-10

### Fixed

- return 503 when model has no providers ([#133](https://github.com/doublewordai/onwards/pull/133))
- use PAT in release-plz so Docker build triggers ([#134](https://github.com/doublewordai/onwards/pull/134))

## [0.18.4](https://github.com/doublewordai/onwards/compare/v0.18.3...v0.18.4) - 2026-03-09

### Fixed

- evaluate routing rules before empty pool check ([#131](https://github.com/doublewordai/onwards/pull/131))

## [0.18.3](https://github.com/doublewordai/onwards/compare/v0.18.2...v0.18.3) - 2026-03-09

### Added

- return 503 for connection errors to disambiguate scaled-down backends ([#128](https://github.com/doublewordai/onwards/pull/128))

## [0.18.2](https://github.com/doublewordai/onwards/compare/v0.18.1...v0.18.2) - 2026-03-04

### Added

- add POST /v1/completions to strict router ([#121](https://github.com/doublewordai/onwards/pull/121))

## [0.18.1](https://github.com/doublewordai/onwards/compare/v0.18.0...v0.18.1) - 2026-02-27

### Fixed

- preserve request counts across config updates ([#118](https://github.com/doublewordai/onwards/pull/118))

## [0.18.0](https://github.com/doublewordai/onwards/compare/v0.17.1...v0.18.0) - 2026-02-25

### Added

- replace weighted random with weighted least connections load balancing ([#114](https://github.com/doublewordai/onwards/pull/114))

## [0.17.1](https://github.com/doublewordai/onwards/compare/v0.17.0...v0.17.1) - 2026-02-24

### Fixed

- remove /v1 prefix from strict mode routes to prevent double /v1 path ([#115](https://github.com/doublewordai/onwards/pull/115))

### Other

- *(ci)* stop running heavy CI on main pushes ([#112](https://github.com/doublewordai/onwards/pull/112))

## [0.17.0](https://github.com/doublewordai/onwards/compare/v0.16.1...v0.17.0) - 2026-02-23

### Added

- add routing rules for per-key traffic control ([#103](https://github.com/doublewordai/onwards/pull/103))

## [0.16.1](https://github.com/doublewordai/onwards/compare/v0.16.0...v0.16.1) - 2026-02-23

### Fixed

- maybe fix concurrency bug with Dashmap ([#109](https://github.com/doublewordai/onwards/pull/109))

## [0.16.0](https://github.com/doublewordai/onwards/compare/v0.15.2...v0.16.0) - 2026-02-23

### Added

- add Open Responses adapter for protocol translation ([#79](https://github.com/doublewordai/onwards/pull/79))

### Fixed

- trusted providers ([#107](https://github.com/doublewordai/onwards/pull/107))

## [0.15.2](https://github.com/doublewordai/onwards/compare/v0.15.1...v0.15.2) - 2026-02-19

### Fixed

- HttpConnector enforcing http test and SSE handling in strict mode ([#102](https://github.com/doublewordai/onwards/pull/102))

## [0.15.1](https://github.com/doublewordai/onwards/compare/v0.15.0...v0.15.1) - 2026-02-18

### Fixed

- Set `enforce_http` to `false` to allow non-http URI schemes in `… ([#100](https://github.com/doublewordai/onwards/pull/100))

### Other

- *(deps)* update rust docker tag to v1.93.1 ([#85](https://github.com/doublewordai/onwards/pull/85))

## [0.15.0](https://github.com/doublewordai/onwards/compare/v0.14.0...v0.15.0) - 2026-02-17

### Added

- *(strict-mode)* add trusted pools to bypass sanitization ([#88](https://github.com/doublewordai/onwards/pull/88))

## [0.14.0](https://github.com/doublewordai/onwards/compare/v0.13.0...v0.14.0) - 2026-02-16

### Added

- improved connection pooling, reuse, holds idles, cleanups, timeouts ([#65](https://github.com/doublewordai/onwards/pull/65))

## [0.13.0](https://github.com/doublewordai/onwards/compare/v0.12.0...v0.13.0) - 2026-02-16

### Added

- add with-replacement sampling for weighted random failover ([#93](https://github.com/doublewordai/onwards/pull/93))

## [0.12.0](https://github.com/doublewordai/onwards/compare/v0.11.2...v0.12.0) - 2026-02-13

### Added

- add strict mode router with schema validation ([#78](https://github.com/doublewordai/onwards/pull/78))
- openAI-style error sanitization ([#84](https://github.com/doublewordai/onwards/pull/84))

### Other

- *(deps)* update rust crate anyhow to v1.0.101 ([#81](https://github.com/doublewordai/onwards/pull/81))
- *(deps)* update rust docker tag to v1.93.0 ([#63](https://github.com/doublewordai/onwards/pull/63))
- *(deps)* update actions/checkout action to v6 ([#76](https://github.com/doublewordai/onwards/pull/76))
- *(deps)* update rust crate hyper-util to v0.1.20 ([#70](https://github.com/doublewordai/onwards/pull/70))
- *(deps)* update rust crate clap to v4.5.57 ([#66](https://github.com/doublewordai/onwards/pull/66))
- *(deps)* update actions/upload-pages-artifact action to v4 ([#77](https://github.com/doublewordai/onwards/pull/77))
- *(deps)* update rust crate bytes to v1.11.1 ([#72](https://github.com/doublewordai/onwards/pull/72))
- replace monolithic README with mdBook documentation site ([#74](https://github.com/doublewordai/onwards/pull/74))

## [0.11.2](https://github.com/doublewordai/onwards/compare/v0.11.1...v0.11.2) - 2026-02-02

### Added

- furnish more useful info on to traces ([#67](https://github.com/doublewordai/onwards/pull/67))

### Other

- enforce conventional commits in pr titles ([#69](https://github.com/doublewordai/onwards/pull/69))

## [0.11.1](https://github.com/doublewordai/onwards/compare/v0.11.0...v0.11.1) - 2026-01-26

### Fixed

- *(deps)* update rust crate axum-prometheus to 0.10.0 ([#54](https://github.com/doublewordai/onwards/pull/54))

### Other

- Buffer SSE chunks to handle incomplete JSON from providers ([#64](https://github.com/doublewordai/onwards/pull/64))
- *(deps)* update rust crate axum-test to v18 ([#57](https://github.com/doublewordai/onwards/pull/57))
- *(deps)* update ubuntu docker tag to v24 ([#58](https://github.com/doublewordai/onwards/pull/58))
- *(deps)* update docker/build-push-action action to v6 ([#56](https://github.com/doublewordai/onwards/pull/56))
- *(deps)* update actions/checkout action to v6 ([#55](https://github.com/doublewordai/onwards/pull/55))
- *(deps)* update rust docker tag to v1.92.0 ([#53](https://github.com/doublewordai/onwards/pull/53))

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
