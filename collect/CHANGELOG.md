# Changelog

What changed in each version of `qloudpulse-collect`.

## [0.1.0] - Unreleased

- First version: `collect`, `analyze`, `pack`, `verify`, `deliver`, `upload`, `run`, `policy`, `call-plan`.
- Reads what qloudpulse's hourly AWS cost collector reads (without the `usageMetrics` reads), from a hand-made seed
  call plan.
- Tag rules (keep, drop, replace, pseudonymize; unlisted tags denied) and field rules; changed values carried into the
  requests that name them.
- Delivery to your own S3 bucket, to qloudpulse (feature `upload`), or to a command; retries and retention.
