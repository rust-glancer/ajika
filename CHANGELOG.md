# Changelog

## [0.2.0](https://github.com/rust-glancer/ajika/compare/v0.1.7...v0.2.0) (2026-11-18)


### ⚠ BREAKING CHANGES

* More perf and storage improvements ([#28](https://github.com/rust-glancer/ajika/issues/28))
* Implement deferred checkpoint mode ([#27](https://github.com/rust-glancer/ajika/issues/27))

### Features

* Implement deferred checkpoint mode ([#27](https://github.com/rust-glancer/ajika/issues/27)) ([5e701ff](https://github.com/rust-glancer/ajika/commit/5e701ff4850343db9154911bd3866e73a70c507f))
* More perf and storage improvements ([#28](https://github.com/rust-glancer/ajika/issues/28)) ([3cbb854](https://github.com/rust-glancer/ajika/commit/3cbb854c279511313df37eaa839b2ba24c53234e))
* Use crc32c instead of blake3 for checksum ([#25](https://github.com/rust-glancer/ajika/issues/25)) ([e236ed2](https://github.com/rust-glancer/ajika/commit/e236ed28c59fcef7b5408ed4d48e97bf14fba844))

## [0.1.7](https://github.com/rust-glancer/ajika/compare/v0.1.6...v0.1.7) (2026-11-12)


### Bug Fixes

* Do not store encoded values in memory for no reason ([#22](https://github.com/rust-glancer/ajika/issues/22)) ([412b72e](https://github.com/rust-glancer/ajika/commit/412b72edae0dc3fb6b80ceb78db6ee3f55c10f97))

## [0.1.6](https://github.com/rust-glancer/ajika/compare/v0.1.5...v0.1.6) (2026-11-11)


### Features

* A bunch of fixups ([#20](https://github.com/rust-glancer/ajika/issues/20)) ([8fb70fa](https://github.com/rust-glancer/ajika/commit/8fb70faa854df233a283a83b559925ee7825c9a7))

## [0.1.5](https://github.com/rust-glancer/ajika/compare/v0.1.4...v0.1.5) (2026-11-10)


### Features

* Forget all ([#19](https://github.com/rust-glancer/ajika/issues/19)) ([d1f40c5](https://github.com/rust-glancer/ajika/commit/d1f40c54faa25e2bd8315e994f5af813f1bde518))
* Input context for lazy initializers ([#16](https://github.com/rust-glancer/ajika/issues/16)) ([d97880e](https://github.com/rust-glancer/ajika/commit/d97880e04d9b48ad61ccd5a5d74d774d17a706b6))
* Make updating inputs return value ([#18](https://github.com/rust-glancer/ajika/issues/18)) ([2a7a6c9](https://github.com/rust-glancer/ajika/commit/2a7a6c9454ab9b3d70bb216926aacc5f2cc2bc45))

## [0.1.4](https://github.com/rust-glancer/ajika/compare/v0.1.3...v0.1.4) (2026-11-10)


### Features

* Allow host-provided cancellation ([#15](https://github.com/rust-glancer/ajika/issues/15)) ([2ec07e4](https://github.com/rust-glancer/ajika/commit/2ec07e444f73cc831ce34fc81e4ffccd65442f28))
* Remove with_base ([#13](https://github.com/rust-glancer/ajika/issues/13)) ([6577fb5](https://github.com/rust-glancer/ajika/commit/6577fb567f17b256cfae1a765c8d39933779bc96))
* returns(ref) API ([#11](https://github.com/rust-glancer/ajika/issues/11)) ([9246c84](https://github.com/rust-glancer/ajika/commit/9246c84c548e8baa66648d567d851e024c157176))

## [0.1.3](https://github.com/rust-glancer/ajika/compare/v0.1.2...v0.1.3) (2026-11-09)


### Features

* Add value_ref to get &T instead of Arc&lt;T&gt; ([#8](https://github.com/rust-glancer/ajika/issues/8)) ([f8fbaf7](https://github.com/rust-glancer/ajika/commit/f8fbaf7ece6350e0157e760bdf246fd88ad0dd42))

## [0.1.2](https://github.com/rust-glancer/ajika/compare/v0.1.1...v0.1.2) (2026-11-07)


### Features

* Auto recovery ([#6](https://github.com/rust-glancer/ajika/issues/6)) ([7d8d3f9](https://github.com/rust-glancer/ajika/commit/7d8d3f9cbb25e5d142aa77a955b03d889e28553d))

## [0.1.1](https://github.com/rust-glancer/ajika/compare/v0.1.0...v0.1.1) (2026-11-07)


### Features

* Better error type ([#4](https://github.com/rust-glancer/ajika/issues/4)) ([f446215](https://github.com/rust-glancer/ajika/commit/f4462155eb9702d273cdd4fbd14082652c44a6dc))

## 0.1.0

Initial release.
