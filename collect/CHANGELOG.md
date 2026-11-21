# Changelog

What changed in each version of `qloudpulse-collect`.

## [0.1.0] - Unreleased

- First version: `collect`, `analyze`, `pack`, `verify`, `deliver`, `upload`, `run`, `policy`, `call-plan`.
- Reads what qloudpulse's hourly AWS cost collector reads (without the `usageMetrics` reads), from a hand-made seed
  call plan.
- Tag rules (keep, drop, replace, pseudonymize; unlisted tags denied) and field rules; changed values carried into the
  requests that name them.
- The bill (`[cur]`): qloudpulse's CUR 2.0 queries in your Athena, once a day, results only; its tag columns follow
  the tag rules.
- Infra, first part (with the bill, once a day): regions and billed resources derived from the bill as qloudpulse
  does; network, tags of billed ARNs, internet-facing load balancers, public snapshots and images, bucket access,
  CloudFront, and the describers that list everything.
- `[identifiers]`: account and resource IDs pseudonymized with their format kept, consistently everywhere.
- One TLS stack (rustls on ring); no C crypto build.
- Delivery to your own S3 bucket, to qloudpulse (feature `upload`), or to a command; retries and retention.
