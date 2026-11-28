# Offline collection: `qloudpulse-collect`

Status: accepted design, nothing built yet (2026-11-21).

## Why

Some customers will not grant qloudpulse cross-account roles, or want to see and control every byte that leaves
their accounts. `qloudpulse-collect` is a client they run themselves. It reads their cloud accounts with their own
credentials, lets them inspect, filter and redact what it found, and writes a `.tgz` bundle. They deliver the bundle;
qloudpulse ingests it through the same collectors that read live accounts today.

The client needs nothing from our servers. It contacts qloudpulse only when the customer tells it to upload a
finished bundle (`upload`, or `deliver.qloudpulse` in config), and then it sends that bundle and nothing else.

Besides AWS and that upload, the only host the default build contacts is `api.github.com` (and GitHub's release
downloads for `update`), to learn of a newer release: from the commands a person runs at a terminal and from `ui`, at
most once a day, never from `collect`, `pack`, `deliver`, `upload` or `run`, and never with
`QLOUDPULSE_NO_UPDATE_CHECK=1`. The build without `upload` has no update check and no `update`.

## Decisions

### The binary

- **Separate binary, `qloudpulse-collect`**, next to `cli/`. The only qloudpulse code in it is the bundle upload, and
  that sits behind the Cargo feature `upload` (on by default). A build with `--no-default-features` has no qloudpulse
  host and no qloudpulse HTTP client compiled in, so "never phones home" can be checked with `grep`, `cargo tree`, or
  an egress policy that allows only the cloud APIs. Releases ship the default build; the offline one is a
  `cargo install --no-default-features` of the release's tag (`cli/collect/README.md`, Verify what you run).
- **Rust.** To keep the client in step with the TypeScript collectors, both build against a generated
  **call-plan manifest** (operations and IAM actions per collector) and a **bundle JSON Schema** (see Ownership).
- **Unattended is the primary mode**: no prompts, structured logs, non-zero exit on partial failure. `analyze` and
  `ui` are the human-facing commands.
- **Config is TOML**, path from `--config` (or `QLOUDPULSE_COLLECT_CONFIG`); unnamed, `./qloudpulse-collect.toml` if
  it exists, else `qloudpulse-collect.toml` in `$XDG_CONFIG_HOME/qloudpulse` or `~/.config/qloudpulse`
  (`%APPDATA%\qloudpulse` on Windows). Raw snapshots and bundles go to the state directory,
  `~/.local/state/qloudpulse/collect/<source_id>/` (`$XDG_STATE_HOME`, `%LOCALAPPDATA%`), unless the config names
  `work_dir` or `output_dir`; a bundle every target has is deleted. `--help` lists the paths. `init` writes a
  commented starter file. No secrets in it: AWS credentials
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

- A `.tgz` of **recorded calls**, as NDJSON, one file per data kind. Each record is one HTTP exchange with an AWS
  API: account, region, service, operation, the request parameters, the page number, and the **raw response** (status,
  the headers that matter, and the XML or JSON body exactly as AWS sent it, after redaction). Athena results are
  recorded the same way.
- Raw responses, not parsed objects, because qloudpulse's collectors replay them through their own AWS SDK: the SDK
  parses a recorded response exactly as it parses a live one, so nothing is lost or reshaped in between. A call
  matches its recording on account, region, service, operation, its parameters other than paging ones (`NextToken`,
  `Marker`, `MaxResults`, …) and the page number.
- **Errors are recorded too.** `AccessDenied`, or a throttle that never cleared, is replayed as the same error, so
  collectors degrade exactly as they do live.
- Redaction works on the parsed body and writes it back in the same format. Checksum headers such as DynamoDB's
  `x-amz-crc32` are not recorded, since a redacted body would fail them.
- `manifest.json`: tool version, `schema_version` (semver), `call_plan_version`, `source_id`, collection window, the
  accounts covered, a SHA-256 for every file, a hash and summary of the applied config, the HMAC key fingerprint, and
  scan results.
- Plain text throughout so a reviewer can `tar -xzf` and `jq` it. The JSON Schema is published in this repo.
- `pack --encrypt` (opt-in) wraps the finished `.tgz` as a JWE to a qloudpulse public key that is **compiled in and
  pinned**. The customer inspects the plaintext first; its SHA-256 is recorded alongside.
- Daily parts ride in the first hourly bundle after a configurable hour.

### Filtering and redaction

- Rules target **any field by path**; tags are the commonest target. Five actions:
  - `drop`
  - `keep` (allowlist)
  - `replace`: a static value or a regex rewrite
  - `pseudonymize`: HMAC-SHA256 with a key only the customer holds. The same input always gives the same output, so
    grouping survives across bundles and we cannot reverse it. An optional local mapping file lets the customer
    translate pseudonyms back when reading reports.
  - `encrypt`: deterministic AES-256-GCM (the nonce is an HMAC of the value) under keys derived from a **view
    token**, `qp_…`, that the client creates and only the customer holds. Grouping survives as with a pseudonym, and
    whoever the customer gives the token reads the real value in qloudpulse's web UI: the page decrypts it in the
    browser with WebCrypto, so the token and the real value are never sent to qloudpulse or stored by it. Values
    leave as `qpe1_<key id>_<data>`; the manifest's `view_key_id` says which token a bundle needs. The browser
    code is published as `cli/collect/web/qloudpulse-values.js`, with test vectors the client's tests keep current.
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

The binary writes bundles to its output directory. Each delivery target below is opt-in, and any combination can be
on; `run` delivers every new bundle to each, and `deliver <bundle>…` does it by hand. Only finished bundles are ever
delivered, never raw snapshots.

- **S3** (`[deliver.s3]`: bucket, prefix, region, optional KMS key): a `PutObject` to the customer's **own** bucket
  with their own credentials, the only write the binary makes to AWS. This is kept apart from the read-only
  collection allow-list, and the IAM policy for it is a separate statement on that one bucket.
- **qloudpulse** (`qloudpulse-collect upload [<bundle>…]` or `[deliver.qloudpulse]`; `upload` with no bundle named
  packs the newest raw snapshot if it has no bundle yet and sends that, or the newest bundle): an HTTPS `POST` of the bundle to
  `https://api.qloudpulse.com/v1/collection-bundles` (`--endpoint` to change it). It signs in exactly as the
  qloudpulse CLI does, with the CLI's own code: `QLOUDPULSE_TOKEN` when set (a token from Your tokens, for unattended
  runs); else the token `qloudpulse login` or `qloudpulse-collect login` kept on this machine (in the system keychain,
  or a credentials file only its owner can read); else, at a terminal, a sign-in with the browser. The answer is only
  a status and a reference; the client reads nothing else from qloudpulse.
- **A command** (`[deliver.command]`), run once per bundle with its path, for anything else.
- **Manual upload** in the qloudpulse web UI.

A delivery that fails is retried at the next `run` cycle; the bundle stays in the output directory until every
configured target has it, and retention never deletes an undelivered bundle.

qloudpulse **keeps at most one bundle per source: the newest it has ingested**. Each bundle is deleted once a newer one
from the same source has been ingested; the newest stays so it can be replayed again after a late, older bundle, which
keeps the latest figures (such as the month projection) current. One that fails ingest is kept at most 7 days for a
retry and then deleted. What stays is the data in the reports, not the files.

### What qloudpulse does with a bundle

- It checks the bundle **at upload**: manifest sums, schema, that every recorded operation is on the published
  allow-list, and that the accounts belong to this source. An unknown major `schema_version` is rejected there with a
  clear message.
- It feeds the recorded calls through the same collectors that read live accounts, so a bundle-fed account gets the
  same reports. Pricing happens on our side; the client never needs prices.
- An operation the collectors need that the bundle lacks (an older client) is treated as access denied, and the ingest
  report says which client version adds it.
- Each bundle is processed as of its own window, once per window, so a late bundle fills in a missed hour. A bundle
  more than 13 days old is refused at upload: qloudpulse keeps hourly data for 14 days.
- An AWS account is either connected live or fed by bundles, never both. A company may run several clients, each with
  its own `source_id`, but an account belongs to exactly one source, and CUR comes only from the payer's source.
- The current and the previous major schema are accepted for at least 6 months.

### Contract and distribution

- The call-plan manifest and the bundle JSON Schema are generated from qloudpulse's collectors and published in this
  repo. The client builds against that copy, so what it may call is visible here. Until the generator exists, the
  call plan in `cli/collect/call-plan.json` is a hand-made seed, marked as such.
- The client signs its own requests (SigV4) and speaks each API's wire protocol from the call plan, so it needs no
  per-service AWS SDK crates. Credentials come from the standard AWS chain (environment, profiles, SSO, instance and
  container roles, web identity).
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
