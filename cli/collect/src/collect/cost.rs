//! Mirrors collect-swamp's `aws-cost-collect` (hourly inventory), in its order, without the `usageMetrics` reads:
//! those depend on what qloudpulse has already stored, so a bundle cannot answer them yet.

use super::{Run, distinct, json_objects, json_strings};
use crate::aws::caller::Account;
use crate::body::Body;
use crate::window::HOUR;
use anyhow::Result;
use jiff::{SignedDuration, Timestamp};
use serde_json::{Value, json};

type Collector = fn(&Run, &Account, &str) -> Result<()>;

/// The collectors in the order qloudpulse runs them in a region.
pub const COLLECTORS: &[(&str, Collector)] = &[
    ("dynamodb", dynamodb),
    ("eks", eks),
    ("ec2", ec2),
    ("elasticache", elasticache),
    ("dynamodb-backups", dynamodb_backups),
    ("ebs", ebs),
    ("nat", nat),
    ("load-balancers", load_balancers),
    ("public-ipv4", public_ipv4),
    ("tgw-attachments", tgw_attachments),
    ("vpn", vpn),
    ("vpc-endpoints", vpc_endpoints),
    ("client-vpn", client_vpn),
    ("resolver", resolver),
    ("rds", rds),
    ("kinesis", kinesis),
    ("kms", kms),
    ("log-storage", log_storage),
    ("alarms", alarms),
    ("directories", directories),
    ("opensearch", opensearch),
    ("msk", msk),
    ("mq", mq),
];

fn empty() -> Value {
    json!({})
}

fn dynamodb_tables(run: &Run, account: &Account, region: &str) -> Result<Vec<String>> {
    let pages = run.call(account, region, "dynamodb", "ListTables", empty())?;
    Ok(json_strings(&pages, &["TableNames"]))
}

fn dynamodb(run: &Run, account: &Account, region: &str) -> Result<()> {
    for table in dynamodb_tables(run, account, region)? {
        run.call_ignoring(
            account,
            region,
            "dynamodb",
            "DescribeTable",
            json!({"TableName": table}),
            &["ResourceNotFoundException"],
        )?;
    }
    Ok(())
}

fn dynamodb_backups(run: &Run, account: &Account, region: &str) -> Result<()> {
    run.call(account, region, "dynamodb", "ListBackups", json!({"BackupType": "ALL"}))?;
    for table in dynamodb_tables(run, account, region)? {
        let pages = run.call(
            account,
            region,
            "dynamodb",
            "DescribeContinuousBackups",
            json!({"TableName": table}),
        )?;
        let status = json_strings(
            &pages,
            &[
                "ContinuousBackupsDescription",
                "PointInTimeRecoveryDescription",
                "PointInTimeRecoveryStatus",
            ],
        );
        if status.iter().any(|s| s == "ENABLED") {
            // Usually already answered by the DynamoDB collector; the caller records it once.
            run.call_ignoring(
                account,
                region,
                "dynamodb",
                "DescribeTable",
                json!({"TableName": table}),
                &["ResourceNotFoundException"],
            )?;
        }
    }
    Ok(())
}

fn eks(run: &Run, account: &Account, region: &str) -> Result<()> {
    let pages = run.call(account, region, "eks", "ListClusters", json!({"maxResults": 100}))?;
    let mut versions = Vec::new();
    for name in json_strings(&pages, &["clusters"]) {
        let described = run.call_ignoring(
            account,
            region,
            "eks",
            "DescribeCluster",
            json!({"name": name}),
            &["ResourceNotFoundException"],
        )?;
        versions.extend(json_strings(&described, &["cluster", "version"]));
    }
    let versions = distinct(versions);
    if !versions.is_empty() {
        run.call(
            account,
            region,
            "eks",
            "DescribeClusterVersions",
            json!({"clusterVersions": versions}),
        )?;
    }
    Ok(())
}

/// The instance types and platforms qloudpulse asks spot prices for, in first-seen order.
pub fn spot_query(pages: &[Body]) -> (Vec<String>, Vec<String>) {
    const PRICED: &[&str] = &["Linux/UNIX", "SUSE Linux", "Red Hat Enterprise Linux", "Windows"];
    let instances: Vec<_> = pages
        .iter()
        .filter_map(Body::xml)
        .flat_map(|r| r.find_all(&["reservationSet", "item", "instancesSet", "item"]))
        .collect();
    let types = distinct(
        instances
            .iter()
            .filter_map(|i| i.text_of("instanceType").map(str::to_string)),
    );
    let platforms = distinct(instances.iter().filter_map(|i| {
        let platform = i
            .text_of("platformDetails")
            .filter(|p| !p.is_empty())
            .unwrap_or("Linux/UNIX");
        let spot = i.text_of("instanceLifecycle") == Some("spot");
        (spot || PRICED.contains(&platform)).then(|| platform.to_string())
    }));
    (types, platforms)
}

fn timestamp(t: Timestamp) -> String {
    t.strftime("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn spot_prices(
    run: &Run,
    account: &Account,
    region: &str,
    types: &[String],
    platforms: &[String],
    start: Timestamp,
    end: Timestamp,
) -> Result<()> {
    if types.is_empty() || platforms.is_empty() {
        return Ok(());
    }
    let params = json!({
        "InstanceType": types,
        "ProductDescription": platforms,
        "StartTime": timestamp(start),
        "EndTime": timestamp(end),
        "MaxResults": 1000,
    });
    run.call(account, region, "ec2", "DescribeSpotPriceHistory", params)?;
    Ok(())
}

fn ec2(run: &Run, account: &Account, region: &str) -> Result<()> {
    let params = json!({
        "Filter": [{"Name": "instance-state-name", "Value": ["running"]}],
        "MaxResults": 1000,
    });
    let pages = run.call(account, region, "ec2", "DescribeInstances", params)?;
    let (types, platforms) = spot_query(&pages);
    let hour = run.window.start;
    spot_prices(run, account, region, &types, &platforms, hour, hour + HOUR)?;
    if run.spot_history {
        spot_prices(
            run,
            account,
            region,
            &types,
            &platforms,
            hour - SignedDuration::from_hours(24 * 90),
            hour,
        )?;
    }
    Ok(())
}

fn elasticache(run: &Run, account: &Account, region: &str) -> Result<()> {
    run.call(
        account,
        region,
        "elasticache",
        "DescribeCacheClusters",
        json!({"MaxRecords": 100}),
    )?;
    Ok(())
}

fn ebs(run: &Run, account: &Account, region: &str) -> Result<()> {
    let params = json!({"Filter": [{"Name": "status", "Value": ["in-use", "available"]}], "MaxResults": 500});
    run.call(account, region, "ec2", "DescribeVolumes", params)?;
    Ok(())
}

fn nat(run: &Run, account: &Account, region: &str) -> Result<()> {
    let params = json!({"Filter": [{"Name": "state", "Value": ["available"]}], "MaxResults": 1000});
    run.call(account, region, "ec2", "DescribeNatGateways", params)?;
    Ok(())
}

fn load_balancers(run: &Run, account: &Account, region: &str) -> Result<()> {
    run.call(
        account,
        region,
        "elbv2",
        "DescribeLoadBalancers",
        json!({"PageSize": 400}),
    )?;
    run.call(
        account,
        region,
        "elb",
        "DescribeLoadBalancers",
        json!({"PageSize": 400}),
    )?;
    Ok(())
}

fn public_ipv4(run: &Run, account: &Account, region: &str) -> Result<()> {
    run.call(
        account,
        region,
        "ec2",
        "DescribeNetworkInterfaces",
        json!({"MaxResults": 1000}),
    )?;
    run.call(account, region, "ec2", "DescribeAddresses", empty())?;
    run.call(account, region, "ec2", "DescribeVpnConnections", empty())?;
    Ok(())
}

fn tgw_attachments(run: &Run, account: &Account, region: &str) -> Result<()> {
    run.call(
        account,
        region,
        "ec2",
        "DescribeTransitGatewayAttachments",
        json!({"MaxResults": 1000}),
    )?;
    Ok(())
}

fn vpn(run: &Run, account: &Account, region: &str) -> Result<()> {
    run.call(account, region, "ec2", "DescribeVpnConnections", empty())?;
    Ok(())
}

fn vpc_endpoints(run: &Run, account: &Account, region: &str) -> Result<()> {
    run.call(
        account,
        region,
        "ec2",
        "DescribeVpcEndpoints",
        json!({"MaxResults": 1000}),
    )?;
    Ok(())
}

fn client_vpn(run: &Run, account: &Account, region: &str) -> Result<()> {
    let pages = run.call(
        account,
        region,
        "ec2",
        "DescribeClientVpnEndpoints",
        json!({"MaxResults": 1000}),
    )?;
    let available: Vec<String> = pages
        .iter()
        .filter_map(Body::xml)
        .flat_map(|r| r.find_all(&["clientVpnEndpoint", "item"]))
        .filter(|e| e.child("status").and_then(|s| s.text_of("code")) == Some("available"))
        .filter_map(|e| e.text_of("clientVpnEndpointId").map(str::to_string))
        .collect();
    for id in available {
        let params = json!({"ClientVpnEndpointId": id, "MaxResults": 1000});
        run.call(account, region, "ec2", "DescribeClientVpnTargetNetworks", params)?;
    }
    Ok(())
}

fn resolver(run: &Run, account: &Account, region: &str) -> Result<()> {
    run.call(
        account,
        region,
        "route53resolver",
        "ListResolverEndpoints",
        json!({"MaxResults": 100}),
    )?;
    Ok(())
}

fn rds(run: &Run, account: &Account, region: &str) -> Result<()> {
    run.call(
        account,
        region,
        "rds",
        "DescribeDBInstances",
        json!({"MaxRecords": 100}),
    )?;
    Ok(())
}

fn kinesis(run: &Run, account: &Account, region: &str) -> Result<()> {
    let pages = run.call(account, region, "kinesis", "ListStreams", empty())?;
    let summaries = json_objects(&pages, &["StreamSummaries"]);
    let arns: Vec<String> = if summaries.is_empty() {
        json_strings(&pages, &["StreamNames"])
            .into_iter()
            .map(|name| format!("arn:aws:kinesis:{region}:{}:stream/{name}", account.id))
            .collect()
    } else {
        summaries
            .iter()
            .filter_map(|s| match s.get("StreamARN").and_then(Value::as_str) {
                Some(arn) => Some(arn.to_string()),
                None => s
                    .get("StreamName")
                    .and_then(Value::as_str)
                    .map(|name| format!("arn:aws:kinesis:{region}:{}:stream/{name}", account.id)),
            })
            .collect()
    };
    for arn in arns {
        run.call(
            account,
            region,
            "kinesis",
            "DescribeStreamSummary",
            json!({"StreamARN": arn}),
        )?;
    }
    Ok(())
}

fn kms(run: &Run, account: &Account, region: &str) -> Result<()> {
    let pages = run.call(account, region, "kms", "ListKeys", json!({"Limit": 1000}))?;
    for key in json_strings(&pages, &["Keys", "KeyId"]) {
        run.call_ignoring(
            account,
            region,
            "kms",
            "DescribeKey",
            json!({"KeyId": key}),
            &["NotFoundException"],
        )?;
    }
    Ok(())
}

fn log_storage(run: &Run, account: &Account, region: &str) -> Result<()> {
    run.call(account, region, "logs", "DescribeLogGroups", json!({"limit": 50}))?;
    Ok(())
}

pub(crate) fn alarms(run: &Run, account: &Account, region: &str) -> Result<()> {
    let params = json!({"AlarmTypes": ["MetricAlarm", "CompositeAlarm"], "MaxRecords": 100});
    run.call(account, region, "cloudwatch", "DescribeAlarms", params)?;
    Ok(())
}

fn directories(run: &Run, account: &Account, region: &str) -> Result<()> {
    run.call(account, region, "ds", "DescribeDirectories", empty())?;
    Ok(())
}

pub(crate) fn opensearch(run: &Run, account: &Account, region: &str) -> Result<()> {
    let pages = run.call(account, region, "opensearch", "ListDomainNames", empty())?;
    let names = json_strings(&pages, &["DomainNames", "DomainName"]);
    for batch in names.chunks(5) {
        run.call(
            account,
            region,
            "opensearch",
            "DescribeDomains",
            json!({"DomainNames": batch}),
        )?;
    }
    Ok(())
}

pub(crate) fn msk(run: &Run, account: &Account, region: &str) -> Result<()> {
    let pages = run.call(account, region, "kafka", "ListClustersV2", json!({"maxResults": 100}))?;
    for arn in json_strings(&pages, &["clusterInfoList", "clusterArn"]) {
        run.call(
            account,
            region,
            "kafka",
            "DescribeClusterV2",
            json!({"ClusterArn": arn}),
        )?;
    }
    Ok(())
}

pub(crate) fn mq(run: &Run, account: &Account, region: &str) -> Result<()> {
    let pages = run.call(account, region, "mq", "ListBrokers", json!({"maxResults": 100}))?;
    for id in json_strings(&pages, &["brokerSummaries", "brokerId"]) {
        run.call(account, region, "mq", "DescribeBroker", json!({"BrokerId": id}))?;
    }
    Ok(())
}

/// ECS clusters, their instances and services, capacity providers and the Auto Scaling groups behind them.
pub fn ecs(run: &Run, account: &Account, region: &str) -> Result<()> {
    let pages = run.call(account, region, "ecs", "ListClusters", json!({"maxResults": 100}))?;
    let arns = json_strings(&pages, &["clusterArns"]);
    if arns.is_empty() {
        return Ok(());
    }
    let mut clusters = Vec::new();
    for batch in arns.chunks(100) {
        clusters.extend(run.call(account, region, "ecs", "DescribeClusters", json!({"clusters": batch}))?);
    }
    let mut providers = json_strings(&clusters, &["clusters", "capacityProviders"]);
    for name in json_strings(&clusters, &["clusters", "clusterName"]) {
        let listed = run.call(
            account,
            region,
            "ecs",
            "ListContainerInstances",
            json!({"cluster": name, "maxResults": 100}),
        )?;
        for batch in json_strings(&listed, &["containerInstanceArns"]).chunks(100) {
            let described = run.call(
                account,
                region,
                "ecs",
                "DescribeContainerInstances",
                json!({"cluster": name, "containerInstances": batch}),
            )?;
            providers.extend(json_strings(
                &described,
                &["containerInstances", "capacityProviderName"],
            ));
        }
        let services = run.call(
            account,
            region,
            "ecs",
            "ListServices",
            json!({"cluster": name, "maxResults": 100}),
        )?;
        for batch in json_strings(&services, &["serviceArns"]).chunks(10) {
            run.call(
                account,
                region,
                "ecs",
                "DescribeServices",
                json!({"cluster": name, "services": batch}),
            )?;
        }
    }
    let mut providers: Vec<String> = providers
        .into_iter()
        .filter(|p| p != "FARGATE" && p != "FARGATE_SPOT")
        .collect();
    providers.sort();
    providers.dedup();
    let mut groups = Vec::new();
    for batch in providers.chunks(10) {
        let described = run.call(
            account,
            region,
            "ecs",
            "DescribeCapacityProviders",
            json!({"capacityProviders": batch}),
        )?;
        groups.extend(
            json_strings(
                &described,
                &["capacityProviders", "autoScalingGroupProvider", "autoScalingGroupArn"],
            )
            .into_iter()
            .filter_map(|arn| {
                arn.split_once("autoScalingGroupName/")
                    .map(|(_, name)| name.to_string())
            }),
        );
    }
    for batch in groups.chunks(50) {
        run.call(
            account,
            region,
            "autoscaling",
            "DescribeAutoScalingGroups",
            json!({"AutoScalingGroupNames": batch}),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spot_query_takes_running_types_and_priced_platforms_in_order() {
        let xml = "<DescribeInstancesResponse><reservationSet><item><instancesSet>\
            <item><instanceType>m5.large</instanceType><platformDetails>Linux/UNIX</platformDetails></item>\
            <item><instanceType>c6g.xlarge</instanceType><platformDetails>Windows with SQL Server Standard</platformDetails></item>\
            <item><instanceType>m5.large</instanceType><platformDetails>Windows with SQL Server Standard</platformDetails>\
                  <instanceLifecycle>spot</instanceLifecycle></item>\
            <item><instanceType>t3.micro</instanceType></item>\
            </instancesSet></item></reservationSet></DescribeInstancesResponse>";
        let (types, platforms) = spot_query(&[Body::parse(xml.as_bytes())]);
        assert_eq!(types, ["m5.large", "c6g.xlarge", "t3.micro"]);
        assert_eq!(platforms, ["Linux/UNIX", "Windows with SQL Server Standard"]);
    }
}
