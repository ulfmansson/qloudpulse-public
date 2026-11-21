//! Mirrors collect-swamp's `aws-infra-collect` (daily): its region list and resource list come from the bill, so this
//! runs after the bill in the same run and derives both from the bill's rows the way the collector derives them from
//! `aws_cost.cur_daily` (`regionsInUse`, `billedResources`, `billedArn`, `billedRegion`, `apiRegion`).
//!
//! Covered: the network, tags of the billed ARNs, what the internet reaches (load balancers, Client VPN), public
//! snapshots and images, buckets, CloudFront, the describers that list everything (Aurora, log groups, OpenSearch,
//! MSK, MQ, alarms, flow logs), and those the billed ARNs drive (instances and their images, volumes, Elastic IPs,
//! RDS, DynamoDB, EKS, ElastiCache, bucket settings, application load balancers, API Gateway, ECR).
//!
//! Also Cloud Control's settings of SQS, SNS, EFS, ECS, Kinesis, KMS, network and gateway load balancers, NAT
//! gateways, VPC endpoints, transit gateways and hosted zones.
//!
//! Not yet: ECR scan findings (which images count as in use depends on the clock) and the `usageMetrics` reads. The replay answers those
//! requests "access denied", and the collector writes those rows without those settings.

use super::Run;
use super::cur::BillRow;
use crate::aws::caller::{Account, AwsError};
use crate::body::Body;
use crate::log;
use anyhow::Result;
use regex::Regex;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::OnceLock;

pub const COLLECTOR: &str = "aws-infra-collect";
pub const GLOBAL: &str = "global";
const USAGE: &[&str] = &["Usage", "DiscountedUsage", "SavingsPlanCoveredUsage"];

/// `YYYY-MM-DD` `days` before `day`.
fn days_before(day: &str, days: i64) -> String {
    let d: jiff::civil::Date = day.parse().expect("a YYYY-MM-DD day");
    d.checked_sub(jiff::Span::new().days(days))
        .expect("in range")
        .to_string()
}

/// The regions the bill shows the account using in the 35 days to `today`, sorted (`regionsInUse`).
pub fn regions_in_use(rows: &[BillRow], today: &str, account: &str) -> Vec<String> {
    let from = days_before(today, 35);
    let mut out: Vec<String> = rows
        .iter()
        .filter(|r| r.account_id == account && r.day.as_str() >= from.as_str() && r.day.as_str() <= today)
        .filter(|r| USAGE.contains(&r.line_item_type.as_str()) && !r.region.is_empty() && r.region != GLOBAL)
        .map(|r| r.region.clone())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// A resource the bill names, as `billedResources` groups it.
#[derive(Debug, Clone, PartialEq)]
pub struct Billed {
    pub account_id: String,
    pub region: String,
    pub product_code: String,
    pub resource_id: String,
}

/// The resources billed in the 7 days to `today` (`billedResources`): per resource the costliest real region
/// (global only when there is no other), the costliest product; ordered by account and resource id.
pub fn billed_resources(rows: &[BillRow], today: &str, account: &str) -> Vec<Billed> {
    let from = days_before(today, 7);
    let mut by: BTreeMap<String, Vec<&BillRow>> = BTreeMap::new();
    for r in rows {
        if r.account_id == account
            && r.day.as_str() >= from.as_str()
            && r.day.as_str() <= today
            && !r.resource_id.is_empty()
            && USAGE.contains(&r.line_item_type.as_str())
        {
            by.entry(r.resource_id.clone()).or_default().push(r);
        }
    }
    by.into_iter()
        .map(|(resource_id, rs)| {
            let region_of = |r: &BillRow| {
                if r.region.is_empty() || r.region == GLOBAL {
                    GLOBAL.to_string()
                } else {
                    r.region.clone()
                }
            };
            let mut by_region = rs.clone();
            by_region.sort_by(|a, b| {
                let ga = a.region.is_empty() || a.region == GLOBAL;
                let gb = b.region.is_empty() || b.region == GLOBAL;
                ga.cmp(&gb).then(b.cost.total_cmp(&a.cost))
            });
            let mut by_cost = rs.clone();
            by_cost.sort_by(|a, b| b.cost.total_cmp(&a.cost));
            Billed {
                account_id: account.to_string(),
                region: region_of(by_region[0]),
                product_code: by_cost[0].product_code.clone(),
                resource_id,
            }
        })
        .collect()
}

fn region_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^[a-z]{2}(-[a-z]+)+-\d+$").expect("valid"))
}

const EC2_IDS: &[(&str, &str, bool)] = &[
    (r"^i-[0-9a-f]+$", "instance", true),
    (r"^vol-[0-9a-f]+$", "volume", true),
    (r"^snap-[0-9a-f]+$", "snapshot", false),
    (r"^ami-[0-9a-f]+$", "image", false),
    (r"^nat-[0-9a-f]+$", "natgateway", true),
    (r"^eipalloc-[0-9a-f]+$", "elastic-ip", true),
    (r"^eni-[0-9a-f]+$", "network-interface", true),
    (r"^vpce-[0-9a-f]+$", "vpc-endpoint", true),
    (r"^tgw-attach-[0-9a-f]+$", "transit-gateway-attachment", true),
    (r"^tgw-[0-9a-f]+$", "transit-gateway", true),
    (r"^vpn-[0-9a-f]+$", "vpn-connection", true),
    (r"^cvpn-endpoint-[0-9a-f]+$", "client-vpn-endpoint", true),
    (r"^pcx-[0-9a-f]+$", "vpc-peering-connection", true),
    (r"^h-[0-9a-f]+$", "dedicated-host", true),
    (r"^cr-[0-9a-f]+$", "capacity-reservation", true),
];

fn arn_region(arn: &str) -> Option<&str> {
    arn.split(':').nth(3).filter(|r| !r.is_empty())
}

/// The resource's ARN, or None when the bill names it some other way (`billedArn`).
pub fn billed_arn(b: &Billed) -> Option<String> {
    static RESOLVER: OnceLock<Regex> = OnceLock::new();
    let resolver = RESOLVER
        .get_or_init(|| Regex::new(r"^arn:[^:]+:r53resolver:([a-z0-9-]+):(\d{12}):(eni-[0-9a-f]+)$").expect("valid"));
    if let Some(c) = resolver.captures(&b.resource_id) {
        return region_re()
            .is_match(&c[1])
            .then(|| format!("arn:aws:ec2:{}:{}:network-interface/{}", &c[1], &c[2], &c[3]));
    }
    if b.resource_id.starts_with("arn:") {
        return match arn_region(&b.resource_id) {
            Some(r) if !region_re().is_match(r) => None,
            _ => Some(b.resource_id.clone()),
        };
    }
    for (re, kind, owned) in EC2_IDS {
        if Regex::new(re).expect("valid").is_match(&b.resource_id) {
            return Some(if *owned {
                format!("arn:aws:ec2:{}:{}:{kind}/{}", b.region, b.account_id, b.resource_id)
            } else {
                format!("arn:aws:ec2:{}::{kind}/{}", b.region, b.resource_id)
            });
        }
    }
    let fs = Regex::new(r"^fs-[0-9a-f]+$").expect("valid");
    match b.product_code.as_str() {
        "AmazonEFS" if fs.is_match(&b.resource_id) => {
            return Some(format!(
                "arn:aws:elasticfilesystem:{}:{}:file-system/{}",
                b.region, b.account_id, b.resource_id
            ));
        }
        "AmazonFSx" if fs.is_match(&b.resource_id) => {
            return Some(format!(
                "arn:aws:fsx:{}:{}:file-system/{}",
                b.region, b.account_id, b.resource_id
            ));
        }
        _ => {}
    }
    if b.product_code == "AmazonS3"
        && Regex::new(r"^[a-z0-9][a-z0-9.-]+$")
            .expect("valid")
            .is_match(&b.resource_id)
    {
        return Some(format!("arn:aws:s3:::{}", b.resource_id));
    }
    None
}

/// The region a billed row is written in (`billedRegion`).
pub fn billed_region(b: &Billed) -> String {
    let Some(arn) = billed_arn(b) else {
        return b.region.clone();
    };
    match arn_region(&arn) {
        Some(r) => r.to_string(),
        None if arn.starts_with("arn:aws:s3:::") => b.region.clone(),
        None => GLOBAL.to_string(),
    }
}

/// The region whose API answers for an ARN written in `region` (`apiRegion`).
pub fn api_region(region: &str, arn: &str) -> String {
    if region != GLOBAL {
        return region.to_string();
    }
    if arn.split(':').nth(2) == Some("globalaccelerator") {
        "us-west-2".into()
    } else {
        "us-east-1".into()
    }
}

fn call(
    run: &Run,
    account: &Account,
    region: &str,
    service: &str,
    operation: &str,
    params: Value,
) -> Result<Vec<Body>> {
    run.caller.call(COLLECTOR, account, region, service, operation, params)
}

fn xml_texts(pages: &[Body], path: &[&str], field: &str) -> Vec<String> {
    pages
        .iter()
        .filter_map(Body::xml)
        .flat_map(|r| r.find_all(path))
        .filter_map(|e| e.text_of(field).map(str::to_string))
        .collect()
}

/// The 8 network listings (`readNetwork`). True when all answered.
fn network(run: &Run, account: &Account, region: &str) -> bool {
    let mut ok = true;
    for op in [
        "DescribeVpcs",
        "DescribeSubnets",
        "DescribeSecurityGroups",
        "DescribeNetworkAcls",
        "DescribeInternetGateways",
        "DescribeVpcPeeringConnections",
        "DescribeNetworkInterfaces",
    ] {
        ok &= call(run, account, region, "ec2", op, json!({"MaxResults": 1000})).is_ok();
    }
    ok &= call(
        run,
        account,
        region,
        "ec2",
        "DescribeRouteTables",
        json!({"MaxResults": 100}),
    )
    .is_ok();
    ok
}

/// Tags of `arns`, 100 a call; a batch the API refuses as invalid is halved until the bad ARN is alone.
fn tags(run: &Run, account: &Account, home: &str, arns: &[String]) {
    fn read(run: &Run, account: &Account, home: &str, batch: &[String]) {
        match call(
            run,
            account,
            home,
            "tagging",
            "GetResources",
            json!({"ResourceARNList": batch}),
        ) {
            Err(e)
                if batch.len() > 1
                    && e.downcast_ref::<AwsError>()
                        .is_some_and(|a| a.code == "InvalidParameterException" || a.code == "ValidationException") =>
            {
                let half = batch.len().div_ceil(2);
                read(run, account, home, &batch[..half]);
                read(run, account, home, &batch[half..]);
            }
            _ => {}
        }
    }
    for batch in arns.chunks(100) {
        read(run, account, home, batch);
    }
}

/// The describers that list everything, in the API region of a part.
fn listing_describers(run: &Run, account: &Account, region: &str) {
    // Aurora (lib/aurora.ts): clusters, then global clusters and instances when an Aurora one is left.
    if let Ok(pages) = call(
        run,
        account,
        region,
        "rds",
        "DescribeDBClusters",
        json!({"MaxRecords": 100}),
    ) {
        let clusters: Vec<_> = pages
            .iter()
            .filter_map(Body::xml)
            .flat_map(|r| r.find_all(&["DescribeDBClustersResult", "DBClusters", "DBCluster"]))
            .filter(|c| {
                !matches!(c.text_of("Engine"), Some("docdb" | "neptune")) && c.text_of("Status") != Some("deleting")
            })
            .collect();
        if !clusters.is_empty() {
            let _ = call(
                run,
                account,
                region,
                "rds",
                "DescribeGlobalClusters",
                json!({"MaxRecords": 100}),
            );
            let _ = call(
                run,
                account,
                region,
                "rds",
                "DescribeDBInstances",
                json!({"MaxRecords": 100}),
            );
        }
    }
    // Log groups, their metric filters, and one subscription filter per standard group.
    if let Ok(groups) = call(run, account, region, "logs", "DescribeLogGroups", json!({"limit": 50})) {
        let _ = call(
            run,
            account,
            region,
            "logs",
            "DescribeMetricFilters",
            json!({"limit": 50}),
        );
        for g in super::json_objects(&groups, &["logGroups"]) {
            if g["logGroupClass"].as_str().unwrap_or("STANDARD") == "STANDARD"
                && let Some(name) = g["logGroupName"].as_str()
            {
                let _ = call(
                    run,
                    account,
                    region,
                    "logs",
                    "DescribeSubscriptionFilters",
                    json!({"logGroupName": name, "limit": 1}),
                );
            }
        }
    }
    // OpenSearch, MSK, MQ and alarms: the same requests as the hourly collector's.
    for (name, f) in [
        (
            "opensearch",
            super::cost::opensearch as fn(&Run, &Account, &str) -> Result<()>,
        ),
        ("msk", super::cost::msk),
        ("mq", super::cost::mq),
        ("alarms", super::cost::alarms),
    ] {
        if let Err(e) = f(run, account, region) {
            log::warn(
                "infra describer stopped",
                &[("describer", name.into()), ("error", e.to_string())],
            );
        }
    }
    let _ = call(
        run,
        account,
        region,
        "ec2",
        "DescribeFlowLogs",
        json!({"MaxResults": 1000}),
    );
}

/// What the internet reaches through the network (`readFronts`).
fn fronts(run: &Run, account: &Account, region: &str) {
    if let Ok(lbs) = call(
        run,
        account,
        region,
        "elbv2",
        "DescribeLoadBalancers",
        json!({"PageSize": 400}),
    ) {
        let facing = lbs
            .iter()
            .filter_map(Body::xml)
            .flat_map(|r| r.find_all(&["DescribeLoadBalancersResult", "LoadBalancers", "member"]))
            .filter(|lb| lb.text_of("Scheme") == Some("internet-facing"))
            .filter_map(|lb| lb.text_of("LoadBalancerArn").map(str::to_string))
            .collect::<Vec<_>>();
        for arn in facing {
            let _ = call(
                run,
                account,
                region,
                "elbv2",
                "DescribeListeners",
                json!({"LoadBalancerArn": arn, "PageSize": 400}),
            );
        }
    }
    let _ = call(
        run,
        account,
        region,
        "elb",
        "DescribeLoadBalancers",
        json!({"PageSize": 400}),
    );
    let _ = call(
        run,
        account,
        region,
        "ec2",
        "DescribeClientVpnEndpoints",
        json!({"MaxResults": 1000}),
    );
}

/// Public snapshots and images (`readPublicImages`).
fn public_images(run: &Run, account: &Account, region: &str) {
    let snapshots = json!({"Owner": ["self"], "RestorableBy": ["all"], "MaxResults": 1000});
    let _ = call(run, account, region, "ec2", "DescribeSnapshots", snapshots);
    let images = json!({"Owner": ["self"], "Filter": [{"Name": "is-public", "Value": ["true"]}], "MaxResults": 1000});
    let _ = call(run, account, region, "ec2", "DescribeImages", images);
    let manual = json!({"SnapshotType": "manual", "MaxRecords": 100});
    if let Ok(pages) = call(run, account, region, "rds", "DescribeDBSnapshots", manual.clone()) {
        for id in xml_texts(
            &pages,
            &["DescribeDBSnapshotsResult", "DBSnapshots", "DBSnapshot"],
            "DBSnapshotIdentifier",
        ) {
            let _ = call(
                run,
                account,
                region,
                "rds",
                "DescribeDBSnapshotAttributes",
                json!({"DBSnapshotIdentifier": id}),
            );
        }
    }
    if let Ok(pages) = call(run, account, region, "rds", "DescribeDBClusterSnapshots", manual) {
        let path = [
            "DescribeDBClusterSnapshotsResult",
            "DBClusterSnapshots",
            "DBClusterSnapshot",
        ];
        for id in xml_texts(&pages, &path, "DBClusterSnapshotIdentifier") {
            let params = json!({"DBClusterSnapshotIdentifier": id});
            let _ = call(
                run,
                account,
                region,
                "rds",
                "DescribeDBClusterSnapshotAttributes",
                params,
            );
        }
    }
}

/// Buckets' public access, once per account (`readBucketAccess`).
fn buckets(run: &Run, account: &Account) {
    let _ = call(
        run,
        account,
        "eu-west-1",
        "s3control",
        "GetPublicAccessBlock",
        json!({"AccountId": account.id}),
    );
    let Ok(pages) = call(
        run,
        account,
        "us-east-1",
        "s3",
        "ListBuckets",
        json!({"max-buckets": 1000}),
    ) else {
        return;
    };
    let mut by_region: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for b in pages
        .iter()
        .filter_map(Body::xml)
        .flat_map(|r| r.find_all(&["Buckets", "Bucket"]))
    {
        if let Some(name) = b.text_of("Name") {
            let region = b.text_of("BucketRegion").unwrap_or("us-east-1");
            by_region.entry(region.to_string()).or_default().push(name.to_string());
        }
    }
    const REFUSED: &[&str] = &["AccessDenied", "AccessDeniedException", "AllAccessDisabled"];
    for (region, names) in by_region {
        for name in names {
            for op in ["GetBucketPolicyStatus", "GetBucketAcl", "GetPublicAccessBlock"] {
                match call(run, account, &region, "s3", op, json!({"Bucket": name})) {
                    Err(e)
                        if e.downcast_ref::<AwsError>()
                            .is_some_and(|a| REFUSED.contains(&a.code.as_str())) =>
                    {
                        break;
                    }
                    _ => {}
                }
            }
        }
    }
}

/// The last part of an ARN's resource after `marker` (`table/orders` → `orders` for `table/`).
fn after<'a>(arn: &'a str, marker: &str) -> Option<&'a str> {
    arn.split_once(marker).map(|(_, rest)| rest)
}

fn is_code(e: &anyhow::Error, codes: &[&str]) -> bool {
    e.downcast_ref::<AwsError>()
        .is_some_and(|a| codes.contains(&a.code.as_str()))
}

/// The describers the bill's ARNs drive (`describeBilled`, the types it names), in a part's API region. Each
/// describer that fails is logged and the rest go on, as the collector writes those rows without their settings.
fn billed_describers(run: &Run, account: &Account, region: &str, arns: &[String]) {
    let of = |service: &str, marker: &str| -> Vec<String> {
        arns.iter()
            .filter(|a| a.split(':').nth(2) == Some(service))
            .filter_map(|a| after(a, marker).map(str::to_string))
            .collect()
    };
    // EC2: instances, volumes and Elastic IPs by id, 100 a call; then the images of the instances found.
    let mut images = Vec::new();
    for batch in of("ec2", ":instance/").chunks(100) {
        let params = json!({"Filter": [{"Name": "instance-id", "Value": batch}], "MaxResults": 1000});
        if let Ok(pages) = call(run, account, region, "ec2", "DescribeInstances", params) {
            images.extend(xml_texts(
                &pages,
                &["reservationSet", "item", "instancesSet", "item"],
                "imageId",
            ));
        }
    }
    for batch in of("ec2", ":volume/").chunks(100) {
        let params = json!({"Filter": [{"Name": "volume-id", "Value": batch}], "MaxResults": 500});
        let _ = call(run, account, region, "ec2", "DescribeVolumes", params);
    }
    for batch in of("ec2", ":elastic-ip/").chunks(100) {
        let _ = call(
            run,
            account,
            region,
            "ec2",
            "DescribeAddresses",
            json!({"Filter": [{"Name": "allocation-id", "Value": batch}]}),
        );
    }
    // RDS instances and ElastiCache clusters: listed whole when the bill names any.
    if !of("rds", ":db:").is_empty() {
        let _ = call(
            run,
            account,
            region,
            "rds",
            "DescribeDBInstances",
            json!({"MaxRecords": 100}),
        );
    }
    // DynamoDB tables: each described, then its backups.
    for table in of("dynamodb", ":table/") {
        let _ = call(
            run,
            account,
            region,
            "dynamodb",
            "DescribeTable",
            json!({"TableName": table}),
        );
        let _ = call(
            run,
            account,
            region,
            "dynamodb",
            "DescribeContinuousBackups",
            json!({"TableName": table}),
        );
    }
    // EKS clusters, then the versions they run.
    let mut versions = Vec::new();
    for name in of("eks", ":cluster/") {
        if let Ok(pages) = call(run, account, region, "eks", "DescribeCluster", json!({"name": name})) {
            versions.extend(super::json_strings(&pages, &["cluster", "version"]));
        }
    }
    let versions = super::distinct(versions);
    if !versions.is_empty() {
        let _ = call(
            run,
            account,
            region,
            "eks",
            "DescribeClusterVersions",
            json!({"clusterVersions": versions}),
        );
    }
    if !of("elasticache", ":cluster:").is_empty() {
        let _ = call(
            run,
            account,
            region,
            "elasticache",
            "DescribeCacheClusters",
            json!({"MaxRecords": 100}),
        );
    }
    // S3 buckets: public policy, lifecycle, then Intelligent-Tiering; a bucket gone since the bill is passed over.
    for bucket in arns.iter().filter_map(|a| a.strip_prefix("arn:aws:s3:::")) {
        for op in [
            "GetBucketPolicyStatus",
            "GetBucketLifecycleConfiguration",
            "ListBucketIntelligentTieringConfigurations",
        ] {
            match call(run, account, region, "s3", op, json!({"Bucket": bucket})) {
                Err(e) if is_code(&e, &["NoSuchBucket"]) => break,
                _ => {}
            }
        }
    }
    application_load_balancers(run, account, region, arns);
    api_gateways(run, account, region, arns);
    repositories(run, account, region, &of("ecr", ":repository/"));
    // The images the instances found run.
    let mut images = super::distinct(images);
    images.sort();
    for batch in images.chunks(100) {
        let params = json!({"Filter": [{"Name": "image-id", "Value": batch}], "MaxResults": 1000});
        let _ = call(run, account, region, "ec2", "DescribeImages", params);
    }
    cloud_control(run, account, region, arns);
}

/// An ARN's resource type, as `arnType` names it: `sqs:queue`, `ecs:service`, `ec2:natgateway`.
pub fn arn_type(arn: &str) -> String {
    let parts: Vec<&str> = arn.splitn(6, ':').collect();
    let (service, resource) = (parts.get(2).copied().unwrap_or(""), parts.get(5).copied().unwrap_or(""));
    if let Some(i) = resource.find([':', '/'])
        && i > 0
    {
        return format!("{service}:{}", &resource[..i]);
    }
    let own = match service {
        "s3" => "bucket",
        "sns" => "topic",
        "sqs" => "queue",
        _ => "resource",
    };
    format!("{service}:{own}")
}

/// An ARN's resource without its type, as `arnResource` gives it: `service/c/s` → `c/s`.
pub fn arn_resource(arn: &str) -> String {
    let resource = arn.splitn(6, ':').nth(5).unwrap_or(arn);
    match resource.find([':', '/']) {
        Some(i) if i > 0 && i + 1 < resource.len() => resource[i + 1..].to_string(),
        _ => resource.to_string(),
    }
}

/// The types Cloud Control reads (`CLOUD_CONTROL_TYPES`), in order: (row type, TypeName, the identifier keys of a
/// listed model that says nothing more, when the type is listed where the bill names many).
const CLOUD_CONTROL: &[(&str, &str, Option<&[&str]>)] = &[
    ("sqs:queue", "AWS::SQS::Queue", None),
    ("sns:topic", "AWS::SNS::Topic", None),
    (
        "elasticfilesystem:file-system",
        "AWS::EFS::FileSystem",
        Some(&["FileSystemId"]),
    ),
    ("ecs:cluster", "AWS::ECS::Cluster", None),
    ("ecs:service", "AWS::ECS::Service", None),
    ("kinesis:stream", "AWS::Kinesis::Stream", None),
    ("kms:key", "AWS::KMS::Key", None),
    (
        "elasticloadbalancing:loadbalancer",
        "AWS::ElasticLoadBalancingV2::LoadBalancer",
        None,
    ),
    ("ec2:natgateway", "AWS::EC2::NatGateway", Some(&["NatGatewayId"])),
    ("ec2:vpc-endpoint", "AWS::EC2::VPCEndpoint", Some(&["Id"])),
    ("ec2:transit-gateway", "AWS::EC2::TransitGateway", Some(&["Id"])),
    ("route53:hostedzone", "AWS::Route53::HostedZone", None),
];

/// The read actions Cloud Control calls on your behalf for those types (`actions` of each type), for `policy`.
pub const CLOUD_CONTROL_ACTIONS: &[&str] = &[
    "sqs:GetQueueAttributes",
    "sqs:ListQueueTags",
    "sns:GetTopicAttributes",
    "sns:ListTagsForResource",
    "sns:ListSubscriptionsByTopic",
    "sns:GetDataProtectionPolicy",
    "elasticfilesystem:DescribeBackupPolicy",
    "elasticfilesystem:DescribeFileSystemPolicy",
    "elasticfilesystem:DescribeFileSystems",
    "elasticfilesystem:DescribeLifecycleConfiguration",
    "elasticfilesystem:DescribeReplicationConfigurations",
    "ecs:DescribeClusters",
    "ecs:DescribeServices",
    "kms:DescribeKey",
    "kms:GetKeyPolicy",
    "kms:GetKeyRotationStatus",
    "kms:ListResourceTags",
    "kinesis:DescribeStreamSummary",
    "kinesis:ListTagsForStream",
    "kinesis:ListTagsForResource",
    "elasticloadbalancing:DescribeLoadBalancers",
    "elasticloadbalancing:DescribeLoadBalancerAttributes",
    "elasticloadbalancing:DescribeCapacityReservation",
    "elasticloadbalancing:DescribeTags",
    "ec2:DescribeNatGateways",
    "ec2:DescribeVpcEndpoints",
    "ec2:DescribeSubnets",
    "ec2:DescribeSecurityGroups",
    "ec2:DescribeVpcs",
    "vpc-lattice:DescribeServiceNetworkVpcEndpointAssociation",
    "ec2:DescribeTransitGateways",
    "ec2:DescribeTags",
    "route53:GetHostedZone",
    "route53:ListTagsForResource",
    "route53:ListQueryLoggingConfigs",
];

/// Listed instead of read one by one from this many of a type in a part (`LIST_FROM`).
const LIST_FROM: usize = 10;

/// A billed resource's Cloud Control identifier, per type; None where Cloud Control cannot read it.
fn cloud_control_identifier(kind: &str, arn: &str) -> Option<String> {
    match kind {
        "sqs:queue" => {
            let p: Vec<&str> = arn.splitn(6, ':').collect();
            match (p.get(3), p.get(4), p.get(5)) {
                (Some(r), Some(a), Some(n)) if !r.is_empty() && !a.is_empty() && !n.is_empty() => {
                    Some(format!("https://sqs.{r}.amazonaws.com/{a}/{n}"))
                }
                _ => None,
            }
        }
        "sns:topic" => Some(arn.to_string()),
        "ecs:service" => {
            let re = Regex::new(r":service/([^/]+)/[^/]+$").expect("valid");
            re.captures(arn).map(|c| format!("{arn}|{}", &c[1]))
        }
        "elasticloadbalancing:loadbalancer" => Regex::new(r":loadbalancer/(net|gwy)/")
            .expect("valid")
            .is_match(arn)
            .then(|| arn.to_string()),
        _ => Some(arn_resource(arn)).filter(|id| !id.is_empty()),
    }
}

/// The settings of the billed resources Cloud Control reads (`readSettings`): for a listable type the bill names
/// at least ten of, one listing, then a GetResource for each listed model that holds only its identifier; else a
/// GetResource for each. The collector stops starting calls at its 30-minute settings deadline; the client reads
/// them all, and the replay answers whichever the collector asks for.
fn cloud_control(run: &Run, account: &Account, region: &str, arns: &[String]) {
    for (kind, type_name, list) in CLOUD_CONTROL {
        let rows: Vec<String> = arns
            .iter()
            .filter(|a| arn_type(a) == *kind)
            .filter_map(|a| cloud_control_identifier(kind, a))
            .collect();
        if rows.is_empty() {
            continue;
        }
        let mut get = rows.clone();
        if let Some(id_keys) = list
            && rows.len() >= LIST_FROM
        {
            get.clear();
            if let Ok(pages) = call(
                run,
                account,
                region,
                "cloudcontrol",
                "ListResources",
                json!({"TypeName": type_name, "MaxResults": 100}),
            ) {
                let listed: BTreeMap<String, String> = super::json_objects(&pages, &["ResourceDescriptions"])
                    .into_iter()
                    .filter_map(|d| {
                        let id = d["Identifier"].as_str()?.to_string();
                        Some((id, d["Properties"].as_str().unwrap_or("{}").to_string()))
                    })
                    .collect();
                for id in &rows {
                    let Some(props) = listed.get(id) else { continue };
                    let bare = serde_json::from_str::<Value>(props)
                        .ok()
                        .and_then(|v| v.as_object().map(|m| m.keys().all(|k| id_keys.contains(&k.as_str()))))
                        .unwrap_or(true);
                    if bare {
                        get.push(id.clone());
                    }
                }
            }
        }
        for id in get {
            let _ = call(
                run,
                account,
                region,
                "cloudcontrol",
                "GetResource",
                json!({"TypeName": type_name, "Identifier": id}),
            );
        }
    }
}

/// Application load balancers' settings (`lib/alb_config.ts`): 20 a call, one at a time once one is gone.
fn application_load_balancers(run: &Run, account: &Account, region: &str, arns: &[String]) {
    let albs: Vec<&String> = arns.iter().filter(|a| a.contains(":loadbalancer/app/")).collect();
    let mut live = Vec::new();
    for batch in albs.chunks(20) {
        let read = |batch: &[&String]| {
            call(
                run,
                account,
                region,
                "elbv2",
                "DescribeLoadBalancers",
                json!({"LoadBalancerArns": batch}),
            )
        };
        let path = ["DescribeLoadBalancersResult", "LoadBalancers", "member"];
        // Application load balancers only: network and gateway ones are read through Cloud Control.
        let application = |pages: &[Body]| -> Vec<String> {
            pages
                .iter()
                .filter_map(Body::xml)
                .flat_map(|r| r.find_all(&path))
                .filter(|lb| lb.text_of("Type").is_none_or(|t| t == "application"))
                .filter_map(|lb| lb.text_of("LoadBalancerArn").map(str::to_string))
                .collect()
        };
        match read(batch) {
            Ok(pages) => live.extend(application(&pages)),
            Err(e) if is_code(&e, &["LoadBalancerNotFound"]) => {
                for one in batch {
                    if let Ok(pages) = read(std::slice::from_ref(one)) {
                        live.extend(application(&pages));
                    }
                }
            }
            Err(_) => {}
        }
    }
    for lb in live {
        if let Ok(pages) = call(
            run,
            account,
            region,
            "elbv2",
            "DescribeListeners",
            json!({"LoadBalancerArn": lb}),
        ) {
            let listeners: Vec<_> = pages
                .iter()
                .filter_map(Body::xml)
                .flat_map(|r| r.find_all(&["DescribeListenersResult", "Listeners", "member"]))
                .filter_map(|l| {
                    l.text_of("ListenerArn")
                        .map(|a| (a.to_string(), l.text_of("Protocol") == Some("HTTPS")))
                })
                .collect();
            for (listener, https) in listeners {
                let _ = call(
                    run,
                    account,
                    region,
                    "elbv2",
                    "DescribeRules",
                    json!({"ListenerArn": listener}),
                );
                if https {
                    let _ = call(
                        run,
                        account,
                        region,
                        "elbv2",
                        "DescribeListenerCertificates",
                        json!({"ListenerArn": listener}),
                    );
                }
            }
        }
        let _ = call(
            run,
            account,
            region,
            "elbv2",
            "DescribeLoadBalancerAttributes",
            json!({"LoadBalancerArn": lb}),
        );
        let _ = call(
            run,
            account,
            region,
            "elbv2",
            "DescribeTargetGroups",
            json!({"LoadBalancerArn": lb}),
        );
        let _ = call(
            run,
            account,
            region,
            "wafv2",
            "GetWebACLForResource",
            json!({"ResourceArn": lb}),
        );
    }
}

/// API Gateway stages (`lib/apigw_config.ts`): the APIs listed, then the stages and resources of the billed ones.
fn api_gateways(run: &Run, account: &Account, region: &str, arns: &[String]) {
    let wanted = |kind: &str| -> Vec<String> {
        let mut ids: Vec<String> = arns
            .iter()
            .filter(|a| a.starts_with("arn:aws:apigateway:"))
            .filter_map(|a| after(a, kind).and_then(|r| r.split('/').next()).map(str::to_string))
            .collect();
        ids.dedup();
        ids
    };
    let rest = wanted("::/restapis/");
    if !rest.is_empty()
        && let Ok(pages) = call(run, account, region, "apigateway", "GetRestApis", json!({"limit": 500}))
    {
        for id in super::json_strings(&pages, &["items", "id"])
            .into_iter()
            .filter(|id| rest.contains(id))
        {
            let _ = call(
                run,
                account,
                region,
                "apigateway",
                "GetStages",
                json!({"restApiId": id}),
            );
            let params = json!({"restApiId": id, "embed": ["methods"], "limit": 500});
            let _ = call(run, account, region, "apigateway", "GetResources", params);
        }
    }
    let http = wanted("::/apis/");
    if !http.is_empty()
        && let Ok(pages) = call(
            run,
            account,
            region,
            "apigatewayv2",
            "GetApis",
            json!({"maxResults": "500"}),
        )
    {
        for id in super::json_strings(&pages, &["items", "apiId"])
            .into_iter()
            .filter(|id| http.contains(id))
        {
            let _ = call(
                run,
                account,
                region,
                "apigatewayv2",
                "GetStages",
                json!({"ApiId": id, "maxResults": "500"}),
            );
        }
    }
}

/// ECR repositories (`lib/ecr.ts`): 100 names a call, one at a time once one is gone; the registry's scanning, then
/// each repository's images and lifecycle policy. Scan findings, which depend on which images count as in use at
/// collection time, are not read yet.
fn repositories(run: &Run, account: &Account, region: &str, names: &[String]) {
    let mut found = Vec::new();
    for batch in names.chunks(100) {
        let read = |batch: &[String]| {
            call(
                run,
                account,
                region,
                "ecr",
                "DescribeRepositories",
                json!({"repositoryNames": batch}),
            )
        };
        match read(batch) {
            Ok(pages) => found.extend(super::json_strings(&pages, &["repositories", "repositoryName"])),
            Err(e) if is_code(&e, &["RepositoryNotFoundException"]) => {
                for one in batch {
                    if let Ok(pages) = read(std::slice::from_ref(one)) {
                        found.extend(super::json_strings(&pages, &["repositories", "repositoryName"]));
                    }
                }
            }
            Err(_) => {}
        }
    }
    if found.is_empty() {
        return;
    }
    let _ = call(
        run,
        account,
        region,
        "ecr",
        "GetRegistryScanningConfiguration",
        json!({}),
    );
    for name in found {
        let _ = call(
            run,
            account,
            region,
            "ecr",
            "DescribeImages",
            json!({"repositoryName": name, "maxResults": 1000}),
        );
        let _ = call(
            run,
            account,
            region,
            "ecr",
            "GetLifecyclePolicy",
            json!({"repositoryName": name}),
        );
    }
}

/// One account's infra reads, given the bill's rows. Nothing when the bill shows no region in use.
pub fn collect(run: &Run, account: &Account, rows: &[BillRow], today: &str) -> Result<()> {
    let regions = regions_in_use(rows, today, &account.id);
    if regions.is_empty() {
        return Ok(());
    }
    let billed = billed_resources(rows, today, &account.id);
    let distributions = call(
        run,
        account,
        "us-east-1",
        "cloudfront",
        "ListDistributions",
        json!({"MaxItems": 100}),
    )
    .map(|pages| xml_texts(&pages, &["Items", "DistributionSummary"], "Id").len())
    .unwrap_or(0);
    let mut by_region: BTreeMap<String, Vec<&Billed>> = BTreeMap::new();
    for b in &billed {
        by_region.entry(billed_region(b)).or_default().push(b);
    }
    let mut parts: Vec<String> = regions.iter().cloned().chain(by_region.keys().cloned()).collect();
    if distributions > 0 {
        parts.push(GLOBAL.into());
    }
    parts.sort();
    parts.dedup();
    for part in &parts {
        let regional = regions.contains(part);
        let network_ok = regional && network(run, account, part);
        let mut arns: Vec<String> = Vec::new();
        for b in by_region.get(part).into_iter().flatten() {
            if let Some(a) = billed_arn(b)
                && !arns.contains(&a)
            {
                arns.push(a);
            }
        }
        let mut homes: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for a in arns {
            homes.entry(api_region(part, &a)).or_default().push(a);
        }
        for (home, in_home) in &homes {
            tags(run, account, home, in_home);
        }
        let in_part: Vec<String> = homes.values().flatten().cloned().collect();
        billed_describers(run, account, &api_region(part, ""), &in_part);
        listing_describers(run, account, &api_region(part, ""));
        if network_ok {
            fronts(run, account, part);
        }
        if regional {
            public_images(run, account, part);
        }
    }
    buckets(run, account);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(day: &str, resource: &str, region: &str, product: &str, cost: f64) -> BillRow {
        BillRow {
            day: day.into(),
            account_id: "111122223333".into(),
            line_item_type: "Usage".into(),
            product_code: product.into(),
            resource_id: resource.into(),
            region: region.into(),
            cost,
        }
    }

    use crate::aws::{Caller, HttpRequest, HttpResponse, Transport};
    use crate::callplan::CallPlan;
    use crate::window::Window;
    use aws_credential_types::Credentials;
    use std::sync::Mutex;

    /// Answers every call with an empty success in its protocol, and remembers `region service:Operation`.
    struct Quiet(Mutex<Vec<String>>);

    impl Transport for Quiet {
        fn send(&self, request: &HttpRequest) -> Result<HttpResponse> {
            let host = request
                .url
                .trim_start_matches("https://")
                .split('/')
                .next()
                .unwrap()
                .to_string();
            let target = request
                .headers
                .iter()
                .find(|(k, _)| k == "x-amz-target")
                .map(|(_, v)| v.clone());
            let body = String::from_utf8_lossy(&request.body).to_string();
            let action = url::form_urlencoded::parse(body.as_bytes())
                .find(|(k, _)| k == "Action")
                .map(|(_, v)| v.to_string());
            let what = target.or(action).unwrap_or_else(|| request.url.clone());
            self.0.lock().unwrap().push(format!("{host} {what} {body}"));
            let answer = if request
                .headers
                .iter()
                .any(|(k, v)| k == "content-type" && v.contains("json"))
            {
                "{}"
            } else {
                "<R/>"
            };
            Ok(HttpResponse {
                status: 200,
                headers: vec![],
                body: answer.into(),
            })
        }
    }

    #[test]
    fn a_part_per_region_and_global_with_tags_for_billed_arns() {
        let quiet = Quiet(Mutex::new(vec![]));
        let caller = Caller::new(CallPlan::embedded(), &quiet, vec![]);
        let run = Run {
            caller: &caller,
            window: Window::last_full_hour("2026-11-21T00:05:00Z".parse().unwrap()),
            spot_history: false,
        };
        let account = Account {
            id: "111122223333".into(),
            credentials: Credentials::new("A", "s", None, None, "t"),
        };
        let rows = vec![
            row("2026-11-20", "i-0abc", "eu-west-1", "AmazonEC2", 3.0),
            row("2026-11-20", "my-bucket", "", "AmazonS3", 1.0),
            row(
                "2026-11-20",
                "arn:aws:dynamodb:eu-west-1:111122223333:table/orders",
                "eu-west-1",
                "AmazonDynamoDB",
                2.0,
            ),
            row(
                "2026-11-20",
                "arn:aws:elasticloadbalancing:eu-west-1:111122223333:loadbalancer/app/web/50dc6c495c0c9188",
                "eu-west-1",
                "AWSELB",
                2.0,
            ),
        ];
        collect(&run, &account, &rows, "2026-11-21").unwrap();
        let seen = quiet.0.lock().unwrap().clone();
        let has = |host: &str, what: &str| seen.iter().any(|s| s.starts_with(host) && s.contains(what));
        assert!(has("cloudfront.amazonaws.com", "distribution"));
        assert!(has("ec2.eu-west-1", "DescribeVpcs"), "network in a region in use");
        assert!(!has("ec2.us-east-1", "DescribeVpcs"), "no network for the global part");
        assert!(
            seen.iter()
                .any(|s| s.starts_with("tagging.eu-west-1") && s.contains("instance/i-0abc"))
        );
        assert!(
            seen.iter()
                .any(|s| s.starts_with("tagging.us-east-1") && s.contains("arn:aws:s3:::my-bucket")),
            "global ARNs read in us-east-1"
        );
        assert!(
            has("rds.us-east-1", "DescribeDBClusters"),
            "listing describers run for the global part too"
        );
        assert!(has("111122223333.s3-control.eu-west-1", "publicAccessBlock"));
        assert!(has("s3.us-east-1", "ListBuckets"));
        assert!(
            seen.iter()
                .any(|s| s.contains("DescribeInstances") && s.contains("instance-id") && s.contains("i-0abc"))
        );
        assert!(
            seen.iter()
                .any(|s| s.starts_with("dynamodb.eu-west-1") && s.contains("DescribeTable") && s.contains("orders"))
        );
        assert!(
            seen.iter()
                .any(|s| s.contains("DescribeLoadBalancers") && s.contains("LoadBalancerArns.member.1"))
        );
        assert!(
            has("my-bucket.s3.us-east-1", "lifecycle"),
            "a billed bucket's settings, in its part's API region"
        );
    }

    #[test]
    fn cloud_control_types_and_identifiers_follow_the_collector() {
        assert_eq!(arn_type("arn:aws:sqs:eu-west-1:111122223333:jobs"), "sqs:queue");
        assert_eq!(
            arn_type("arn:aws:ecs:eu-west-1:111122223333:service/prod/api"),
            "ecs:service"
        );
        assert_eq!(
            arn_type("arn:aws:ec2:eu-west-1:111122223333:natgateway/nat-0abc"),
            "ec2:natgateway"
        );
        assert_eq!(
            arn_resource("arn:aws:kms:eu-west-1:111122223333:key/1234-abcd"),
            "1234-abcd"
        );
        assert_eq!(
            cloud_control_identifier("sqs:queue", "arn:aws:sqs:eu-west-1:111122223333:jobs").unwrap(),
            "https://sqs.eu-west-1.amazonaws.com/111122223333/jobs"
        );
        assert_eq!(
            cloud_control_identifier("ecs:service", "arn:aws:ecs:eu-west-1:111122223333:service/prod/api").unwrap(),
            "arn:aws:ecs:eu-west-1:111122223333:service/prod/api|prod"
        );
        assert!(
            cloud_control_identifier(
                "elasticloadbalancing:loadbalancer",
                "arn:aws:elasticloadbalancing:eu-west-1:1:loadbalancer/app/web/1"
            )
            .is_none()
        );
        assert_eq!(
            cloud_control_identifier("ec2:natgateway", "arn:aws:ec2:eu-west-1:1:natgateway/nat-0abc").unwrap(),
            "nat-0abc"
        );
    }

    #[test]
    fn regions_and_billed_resources_follow_the_collectors_queries() {
        let rows = vec![
            row("2026-11-20", "i-0abc", "eu-west-1", "AmazonEC2", 3.0),
            row("2026-11-19", "i-0abc", "", "AmazonEC2", 9.0),
            row("2026-10-01", "vol-0old", "us-east-1", "AmazonEC2", 1.0),
            row("2026-11-20", "my-bucket", "", "AmazonS3", 1.0),
            row(
                "2026-11-20",
                "arn:aws:rds:eu-west-1:111122223333:db:orders",
                "eu-west-1",
                "AmazonRDS",
                5.0,
            ),
        ];
        assert_eq!(regions_in_use(&rows, "2026-11-21", "111122223333"), ["eu-west-1"]);
        let billed = billed_resources(&rows, "2026-11-21", "111122223333");
        let ids: Vec<&str> = billed.iter().map(|b| b.resource_id.as_str()).collect();
        assert_eq!(
            ids,
            ["arn:aws:rds:eu-west-1:111122223333:db:orders", "i-0abc", "my-bucket"],
            "the old volume is out of the window"
        );
        let i = &billed[1];
        assert_eq!(i.region, "eu-west-1", "a real region beats global, whatever the cost");
        assert_eq!(
            billed_arn(i).unwrap(),
            "arn:aws:ec2:eu-west-1:111122223333:instance/i-0abc"
        );
        assert_eq!(billed_region(&billed[2]), "global", "a bucket billed without a region");
        assert_eq!(billed_arn(&billed[2]).unwrap(), "arn:aws:s3:::my-bucket");
        assert_eq!(api_region("global", "arn:aws:s3:::my-bucket"), "us-east-1");
    }
}
