//! The resource types the client prices, and the attributes it sends for each.
//!
//! Nothing outside these lists is read from a resource: an attribute that does not set a price stays in CI. A path
//! `a.b` takes `b` from the object `a`, or from every object in the list `a`.
//!
//! Pulumi's AWS provider is built from Terraform's, so the names are Terraform's in camelCase; a Terraform plan reader
//! can use the same lists with the names in snake_case.

/// One resource type the client prices.
#[derive(Debug)]
pub struct PricedType {
    /// The Pulumi type token, as `pulumi preview --json` writes it.
    pub token: &'static str,
    /// What the comment calls it.
    pub label: &'static str,
    /// The attributes that set its price. Empty for a type priced from usage alone, such as an S3 bucket.
    pub attributes: &'static [&'static str],
}

/// Every type the client prices, the first version's list: EC2, EBS, RDS and Aurora, ElastiCache, NAT gateways,
/// public IPv4 addresses, load balancers, EKS, Lambda and S3.
pub const PRICED: &[PricedType] = &[
    PricedType {
        token: "aws:ec2/instance:Instance",
        label: "EC2 instance",
        attributes: &[
            "instanceType",
            "tenancy",
            "ebsOptimized",
            "creditSpecification.cpuCredits",
            "rootBlockDevice.volumeSize",
            "rootBlockDevice.volumeType",
            "rootBlockDevice.iops",
            "rootBlockDevice.throughput",
            "ebsBlockDevices.volumeSize",
            "ebsBlockDevices.volumeType",
            "ebsBlockDevices.iops",
            "ebsBlockDevices.throughput",
        ],
    },
    PricedType {
        token: "aws:ebs/volume:Volume",
        label: "EBS volume",
        attributes: &["size", "type", "iops", "throughput"],
    },
    PricedType {
        token: "aws:rds/instance:Instance",
        label: "RDS instance",
        attributes: &[
            "instanceClass",
            "engine",
            "engineVersion",
            "licenseModel",
            "multiAz",
            "allocatedStorage",
            "storageType",
            "iops",
            "storageThroughput",
            "backupRetentionPeriod",
            "performanceInsightsEnabled",
            "performanceInsightsRetentionPeriod",
        ],
    },
    PricedType {
        token: "aws:rds/cluster:Cluster",
        label: "Aurora cluster",
        attributes: &[
            "engine",
            "engineVersion",
            "engineMode",
            "storageType",
            "backupRetentionPeriod",
            "serverlessv2ScalingConfiguration.minCapacity",
            "serverlessv2ScalingConfiguration.maxCapacity",
        ],
    },
    PricedType {
        token: "aws:rds/clusterInstance:ClusterInstance",
        label: "Aurora instance",
        attributes: &["instanceClass", "engine", "performanceInsightsEnabled"],
    },
    PricedType {
        token: "aws:elasticache/cluster:Cluster",
        label: "ElastiCache cluster",
        attributes: &["nodeType", "numCacheNodes", "engine"],
    },
    PricedType {
        token: "aws:elasticache/replicationGroup:ReplicationGroup",
        label: "ElastiCache replication group",
        attributes: &[
            "nodeType",
            "engine",
            "numCacheClusters",
            "numNodeGroups",
            "replicasPerNodeGroup",
            "multiAzEnabled",
        ],
    },
    PricedType {
        token: "aws:ec2/natGateway:NatGateway",
        label: "NAT gateway",
        attributes: &["connectivityType"],
    },
    PricedType {
        token: "aws:ec2/eip:Eip",
        label: "Elastic IP",
        attributes: &["domain"],
    },
    PricedType {
        token: "aws:lb/loadBalancer:LoadBalancer",
        label: "load balancer",
        attributes: &["loadBalancerType"],
    },
    PricedType {
        token: "aws:alb/loadBalancer:LoadBalancer",
        label: "load balancer",
        attributes: &["loadBalancerType"],
    },
    PricedType {
        token: "aws:eks/cluster:Cluster",
        label: "EKS cluster",
        attributes: &["version"],
    },
    PricedType {
        token: "aws:eks/nodeGroup:NodeGroup",
        label: "EKS node group",
        attributes: &[
            "instanceTypes",
            "capacityType",
            "diskSize",
            "amiType",
            "scalingConfig.desiredSize",
            "scalingConfig.minSize",
            "scalingConfig.maxSize",
        ],
    },
    PricedType {
        token: "aws:lambda/function:Function",
        label: "Lambda function",
        attributes: &["memorySize", "architectures", "ephemeralStorage.size"],
    },
    PricedType {
        token: "aws:s3/bucket:Bucket",
        label: "S3 bucket",
        attributes: &[],
    },
    PricedType {
        token: "aws:s3/bucketV2:BucketV2",
        label: "S3 bucket",
        attributes: &[],
    },
];

/// The priced type with this token, if the client prices it.
pub fn priced(token: &str) -> Option<&'static PricedType> {
    PRICED.iter().find(|t| t.token == token)
}

/// Whether a token names an AWS resource, so that one the client does not price is counted as not priced.
pub fn is_aws(token: &str) -> bool {
    token.starts_with("aws:")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    const BLOCK_DEVICE: [&str; 4] = ["volumeSize", "volumeType", "iops", "throughput"];

    #[test]
    fn tokens_are_unique() {
        let mut seen = HashSet::new();
        for t in PRICED {
            assert!(seen.insert(t.token), "{} is listed twice", t.token);
        }
    }

    #[test]
    fn block_device_attributes_are_listed_for_both_kinds_of_ec2_disk() {
        let ec2 = priced("aws:ec2/instance:Instance").unwrap();
        for parent in ["rootBlockDevice", "ebsBlockDevices"] {
            for leaf in BLOCK_DEVICE {
                let path = format!("{parent}.{leaf}");
                assert!(ec2.attributes.contains(&path.as_str()), "{path} is missing");
            }
        }
    }

    #[test]
    fn paths_are_at_most_two_deep() {
        for t in PRICED {
            for a in t.attributes {
                assert!(a.split('.').count() <= 2, "{a} in {} is deeper than two", t.token);
            }
        }
    }
}
