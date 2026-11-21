# qloudpulse-collect

`qloudpulse-collect` reads your AWS accounts with your own credentials, records what qloudpulse's collectors would
read, applies your redaction rules, and writes a `.tgz` bundle. You can open the bundle and check it before
qloudpulse ingests it. Nothing is read from qloudpulse, and you decide whether and how bundles reach it.

The design and its reasons are in [`docs/offline-collection.md`](../docs/offline-collection.md). This first version
covers qloudpulse's hourly AWS cost collector.

## Get started

```bash
qloudpulse-collect init
```

`init` writes `qloudpulse-collect.toml` and a pseudonym key, `qloudpulse-collect.key`. Back the key up: losing it
changes every pseudonym, which breaks cost history at that point. Then set the role to read through:

```bash
qloudpulse-collect policy > policy.json
```

That command prints the read-only IAM policy the client needs. Every action in it comes from the compiled-in call
plan (`qloudpulse-collect call-plan`, source [`call-plan.json`](call-plan.json)). The client refuses any operation
that is not in the call plan, before it builds the request.

## Look before anything leaves

```bash
qloudpulse-collect collect
```

```bash
qloudpulse-collect analyze
```

```bash
qloudpulse-collect analyze --draft-rules
```

- `collect` writes a **raw snapshot** to `work_dir`. It is local only and never delivered.
- `analyze` lists every tag key with counts, sample values, what your rules do to it, and what each rule costs in
  qloudpulse's reports.
- `--draft-rules` prints a `[tags]` section to paste into the config.

```bash
qloudpulse-collect pack
```

```bash
qloudpulse-collect analyze bundles/qloudpulse-main-20261121T0600Z.tgz
```

```bash
qloudpulse-collect verify bundles/qloudpulse-main-20261121T0600Z.tgz
```

- `pack` applies the rules and writes the bundle.
- `analyze <bundle>` shows exactly what leaves.
- `verify` checks every file in the bundle against its manifest.

A bundle is plain text, so `tar -xzf` and `jq` work on it:
- `manifest.json` holds the window, the accounts and regions read, a SHA-256 for every file, the hash of your
  config, a summary of your rules, what each rule changed, and your key's fingerprint.
- `calls/*.ndjson` holds one recorded AWS answer per line, after redaction.

## The bill

With `[cur]` set, the run at `daily_hour` also reads the bill: the same Athena SQL qloudpulse runs, over your CUR 2.0
table in the payer account, for the previous and the current month (`periods_back`). Only the query results are
recorded, never the CUR files: about one row per day, account, product, resource and usage type.

The role needs, besides what `policy` prints:
- Athena on the workgroup (`qloudpulse-cost` unless you name another),
- Glue `GetDatabase` and `GetTable` on the database and table (`policy` prints these),
- read on the CUR export bucket, and write on the workgroup's results location.

`scope_tag_keys` must be the scope tag keys you set in qloudpulse: they are columns in the query, and qloudpulse's
replay matches the query text. The bill's tag columns (`Name`, the EKS cluster tags and your scope keys) follow your
tag rules like tags anywhere else; a dropped value is left empty. Use `collect --daily` to read the bill outside the
daily hour.

## Forecast and carbon

- **Forecast** (`forecast = true` under `[cur]`, off by default): Cost Explorer in the payer account, this month's
  and last month's spend per linked account, then AWS's forecast to the end of the month for each. AWS bills $0.01
  per request, which is why it is opt-in. `prediction_interval` must be the one set in qloudpulse (default 80).
- **Carbon** (`carbon = true`, the default): AWS's monthly carbon estimate of each account since 2022, by region and
  service, once a day. The role needs `sustainability:GetEstimatedCarbonEmissions`; an account without it is
  recorded as refused.

## Rules

Under `[tags]`, a tag no rule names is dropped (`default = "deny"`) or kept (`"allow"`). Each tag key can have one
action:
- `keep`
- `drop`
- `pseudonymize`: the same value always gives the same `p_…`, and only your key can reverse it
- `replace`: a regex with `$1`, or a fixed value

`[identifiers]` pseudonymizes account IDs and resource IDs while keeping their format: an account ID becomes another
12-digit number, `i-0abc…` becomes `i-` and as many other hex digits. The same ID gets the same pseudonym everywhere,
inside ARNs and requests too, so the bill and the inventory still join. Both are kept by default.

`[[fields]]` rules apply the same actions to any field, matched by the end of its path. The client refuses rules
for fields qloudpulse prices from, such as instance type, size or engine. When a rule changes a value, for example a
table name, the change carries into the later requests that name it. Nothing leaks through a request, and
qloudpulse's replay still matches.

## Every hour

```bash
qloudpulse-collect run
```

`run` collects at 5 past each hour, packs, deletes the raw snapshot, delivers, and keeps the newest
`keep_bundles` bundles. It never deletes a bundle a delivery target is still missing; a failed delivery is retried
at the next run. Use `run --once` for cron, a systemd timer or a Kubernetes CronJob.

## Delivery

Each delivery target is opt-in under `[deliver]`:

| Target | What it does |
|---|---|
| `[deliver.s3]` | `PutObject` into **your own** bucket, with optional SSE-KMS. This is the only write the client makes to AWS. `policy` adds a statement for that one bucket. |
| `[deliver.qloudpulse]`, or `qloudpulse-collect upload <bundle>` | HTTPS `POST` of the bundle with a bundle-upload token. The token is limited to one company and to uploads, and is read from `QLOUDPULSE_UPLOAD_TOKEN` or a file. qloudpulse deletes the file once it has ingested the data. |
| `[deliver.command]` | Runs your command with `{bundle}` replaced by the bundle's path. |

## A build that cannot contact qloudpulse

```bash
cargo build --release --no-default-features
```

This build leaves out the `upload` feature, so there is no qloudpulse code and no qloudpulse host in the binary.
CI checks that with `strings`.
