# Offline collection: `qloudpulse-collect`

Status: accepted design, nothing built yet (2026-11-21).

## Why

Some customers will not grant qloudpulse cross-account roles, or want to see and control every byte that leaves
their accounts. `qloudpulse-collect` is a client they run themselves. It reads their cloud accounts with their own
credentials, lets them inspect, filter and redact what it found, and writes a `.tgz` bundle. They deliver the bundle;
qloudpulse ingests it through the same collectors that read live accounts today.

The client never talks to qloudpulse. It needs nothing from our servers and holds no qloudpulse token.

## Decisions

### The binary

- **Separate binary, `qloudpulse-collect`**, in a Cargo workspace next to `cli/`. No qloudpulse host and no
  qloudpulse HTTP client is compiled in, so "never phones home" can be checked with `grep`, `cargo tree`, or an
  egress policy that allows only the cloud APIs. Shared crates (redaction, bundle format) are fine.
- **Rust.** To keep the client in step with the TypeScript collectors, both build against a generated
  **call-plan manifest** (operations and IAM actions per collector) and a **bundle JSON Schema** (see Ownership).
- **Unattended is the primary mode**: no prompts, structured logs, non-zero exit on partial failure. `analyze` and
  `ui` are the human-facing commands.
- **Config is TOML**, path from `--config`. `init` writes a commented starter file. No secrets in it: AWS credentials
  come from the default credential chain.

### Commands and data flow

```
collect  -> raw snapshot (local only, never delivered)
analyze  -> report on a raw snapshot or a finished bundle
pack     -> apply config, scan, write .tgz (+ optional --encrypt)
run --every 1h [--ui]   -> collect + pack + deliver, clock-aligned, raw deleted after pack (unless --keep-raw)
ui       -> local web UI
init, rotate-key
```

- Hourly runs fire on the hour, not "60 minutes after start". One-shot is the primitive; `run --every` is a thin
  loop so the binary works as a container. Examples ship for cron, systemd timers and a Kubernetes CronJob.
- Output retention (last N bundles / N days) is enforced by `run`.

### What is collected (v1)

AWS only, mirroring these collectors:

| Collector | Client does | Bundle part |
|---|---|---|
| cost (hourly) | Live inventory `Describe*`/`List*` calls in every configured account and region | hourly |
| CUR (daily) | Runs the same aggregation SQL as the collector, embedded in the binary, in the customer's Athena over CUR 2.0. Ships **query results only**, never raw CUR files | daily |
| infra (daily) | Network, tags, config for the resources the **same run's** CUR results named in the last 7 days | daily |
| forecast (daily, **opt-in**) | Cost Explorer `GetCostAndUsage`/`GetCostForecast`. AWS bills $0.01 per request, so off by default | daily |
| carbon (daily) | Sustainability API, monthly data. First bundle back to 2022-01, later ones only changed months | daily |

Out of v1: Kubernetes (needs the client to read the kubelet and API itself), flow logs, health (tied to an SQS queue
in qloudpulse's account), Datadog, DigitalOcean, vSphere. Price lists stay server-side; they are public and the client
never needs them.

Without our database the client plans its own calls:

- **Regions** come from config, default every enabled region (`ec2:DescribeRegions`). The hourly run uses the regions
  in the last daily CUR result, cached locally.
- **Accounts** are listed in config, or discovered with `organizations:ListAccounts`. In each, the client assumes a
  role name the customer chose. There is no `ExternalId`, because there is no third party.
- The binary **refuses any operation not on its compiled-in read-only allow-list**. The published IAM policy is
  generated from the same list.

### Bundle format

- A `.tgz` of **recorded calls**: one record per `(account, region, service, operation, input) -> output`, the output
  in the AWS SDK v3 output shape, as NDJSON, one file per data kind. Athena results are recorded the same way.
- **Errors are recorded too.** `AccessDenied`, or a throttle that never cleared, is replayed as the same error, so
  collectors degrade exactly as they do live.
- `manifest.json`: tool version, `schema_version` (semver), `call_plan_version`, `source_id`, collection window, the
  accounts covered, a SHA-256 for every file, a hash and summary of the applied config, the HMAC key fingerprint, and
  scan results.
- Plain text throughout so a reviewer can `tar -xzf` and `jq` it. The JSON Schema is published in this repo.
- `pack --encrypt` (opt-in) wraps the finished `.tgz` as a JWE to a qloudpulse public key that is **compiled in and
  pinned**. The customer inspects the plaintext first; its SHA-256 is recorded alongside.
- Daily parts ride in the first hourly bundle after a configurable hour.

### Filtering and redaction

- Rules target **any field by path**; tags are the commonest target. Four actions:
  - `drop`
  - `keep` (allowlist)
  - `replace`: a static value or a regex rewrite
  - `pseudonymize`: HMAC-SHA256 with a key only the customer holds. The same input always gives the same output, so
    grouping survives across bundles and we cannot reverse it. An optional local mapping file lets the customer
    translate pseudonyms back when reading reports.
- **Unlisted tags are denied by default.** A setting can flip this to allow-all. `init` pre-fills `keep` for the keys
  qloudpulse features use: `Name`, `Environment`, `Team`, `Company`, `eks:cluster-name`, `aws:eks:cluster-name`,
  `karpenter.sh/nodepool`, `eks:nodegroup-name`, `aws:autoscaling:groupName`, plus the company's scope tag keys.
- **Required fields** (instance type, region, usage amounts, cost, …): the client refuses a config that drops them
  and says why.
- **Feature impact.** `analyze` and the UI say what a rule costs, for example "dropping `Team` disables cost-by-team
  and tag rules", or "redacting resource names disables environment detection".
- **Identifiers inside ARNs, IDs and names** are pseudonymized **format-preserving**, chosen per identifier kind:
  - account ID -> another 12-digit number
  - resource ID -> same prefix and length (`i-0` plus pseudonymous hex)
  - name -> `n_<hmac>`
  - ARN -> parsed, each part rewritten with the same functions

  These are applied identically to CUR results and recorded calls, so the bill and inventory still join. The default
  is to **keep** IDs; names and tag values are pseudonymized only when a rule says so.
- **Derive before redact.** The client can compute fields from raw data before redaction and attach them as synthetic
  tags. v1 does `qloudpulse:environment` from the customer's own name/tag rules, so names can be pseudonymized
  without losing environment grouping. The backend prefers this tag over its own name-based detection.
- **Secret and personal-data scan** (emails, keys, tokens, IPs, high-entropy strings) runs on what would be packed.
  `analyze` reports hits; `pack` **refuses** until each hit is redacted or allow-listed in config
  (`--allow-findings` to override).
- **HMAC key.** `init` creates it and tells the user to back it up. The client warns loudly if it changes. The manifest
  carries its fingerprint (an HMAC of a fixed string, which does not reveal the key); the backend shows a
  "pseudonym discontinuity on date X" marker instead of treating the new values as new resources. `rotate-key` can
  write an old-to-new pseudonym mapping the customer may upload to relink history; it reveals no real values.

### Local UI

- `qloudpulse-collect ui`, or `run --every 1h --ui`. Binds `127.0.0.1` by default. A random token is printed at
  startup, the `Host` header is checked against DNS rebinding, a strict CSP is set, and every asset is embedded, so the
  page talks to nothing but localhost. Another address needs an explicit `--ui-bind`, and the token is still required.
- Server-rendered HTML (axum + askama) with a little plain JS, embedded with `include_str!`. No npm build.
- Four views:
  1. **Analyze**: tag keys, distinct values, which resources carry them, feature impact, scan findings.
  2. **Rules editor** with a raw/redacted side-by-side preview of the same record. Writes the TOML with `toml_edit`,
     so comments survive, through the same validation as the CLI.
  3. **Bundle browser**: open any produced `.tgz`, check the manifest sums, browse every record as it would leave.
  4. **Run history**: last runs, warnings, delivery hook result.
- Raw data for the dashboard comes from a **"collect sample now"** action held **in memory only** for the session.
  Raw data never hits disk from the UI.
- Under `run`, config edits take effect at the next cycle.

### Delivery

The binary writes files. How they reach qloudpulse is the customer's choice:

- **Manual upload** in the qloudpulse web UI.
- **Their own tooling**, e.g. cron plus `curl`.
- **`--deliver <command>`**, run once per bundle. The reference command is a `curl` with a **bundle-upload token**: one
  company, upload only, created in the qloudpulse UI. Only the configured command sees the token; the binary never
  reads it.

### What qloudpulse does with a bundle

- It checks the bundle **at upload**: manifest sums, schema, that every recorded operation is on the published
  allow-list, and that the accounts belong to this source. An unknown major `schema_version` is rejected there with a
  clear message.
- It feeds the recorded calls through the same collectors that read live accounts, so a bundle-fed account gets the
  same reports. Pricing happens on our side; the client never needs prices.
- An operation the collectors need that the bundle lacks (an older client) is treated as access denied, and the ingest
  report says which client version adds it.
- Each bundle is processed as of its own window, once per window, so a late bundle fills in a missed hour.
- An AWS account is either connected live or fed by bundles, never both. A company may run several clients, each with
  its own `source_id`, but an account belongs to exactly one source, and CUR comes only from the payer's source.
- The current and the previous major schema are accepted for at least 6 months.

### Contract and distribution

- The call-plan manifest and the bundle JSON Schema are generated from qloudpulse's collectors and published in this
  repo. The client builds against that copy, so what it may call is visible here.
- Releases use the CLI's pipeline: static musl/macOS/Windows binaries, `SHA256SUMS`, GitHub attestations, a
  version-pinned `install.sh`, plus a distroless container image on ghcr, also attested. Tags are
  `qloudpulse-collect-X.Y.Z`.

## Order of work

1. **Vertical slice: hourly cost.** `collect`/`analyze`/`pack` for the cost collector's calls, against the published
   call plan and schema; one bundle uploaded and ingested end to end.
2. CUR (Athena SQL embedded) and infra, including format-preserving identifier pseudonymization across both.
3. Forecast (opt-in) and carbon.
4. Derive-before-redact environment, secret scan, `--encrypt`, `rotate-key`.
5. Local UI.
6. Release pipeline and container image (the pipeline can start alongside step 1).

Later: Kubernetes, flow logs, other providers, signed bundles.
