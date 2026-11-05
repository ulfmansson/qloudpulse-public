# qloudpulse-public

What you install to connect a Kubernetes cluster to qloudpulse: the cluster agent, as a Helm chart or as plain YAML.
Both install the same thing.

The agent is one pod running the upstream [OpenTelemetry Collector](https://opentelemetry.io/docs/collector/), with
no code of ours in it. Everything it does is in the config you can read in
[`templates/configmap.yaml`](charts/qloudpulse-agent/templates/configmap.yaml).

## What you need

- From qloudpulse: an **address** ending in `/api/v1/write`, and your company's **token**. The token can write metrics
  for your company and do nothing else.
- On EKS, your AWS account connected to qloudpulse first. A node's cost and its EC2 instance come from there.
- From the pod: outbound HTTPS to that address, and port 10250 on every node. Nothing connects into your cluster, so a
  private API endpoint works too.

## Install with Helm

```bash
helm upgrade --install qloudpulse-agent https://github.com/ulfmansson/qloudpulse-public/releases/download/qloudpulse-agent-0.1.0/qloudpulse-agent-0.1.0.tgz \
  --namespace qloudpulse --create-namespace \
  --set clusterName=<eks-cluster-name> \
  --set endpoint=<address-from-qloudpulse> \
  --set token=<token-from-qloudpulse>
```

To keep the token out of Helm's values and your shell history, create a Secret with the key `token` and pass
`--set existingSecret=<secret name>` instead.

## Install with plain YAML

```bash
curl -fsSLO https://raw.githubusercontent.com/ulfmansson/qloudpulse-public/qloudpulse-agent-0.1.0/manifests/qloudpulse-agent.yaml
```

Replace the three `REPLACE-WITH` values at the top of the file, then:

```bash
kubectl apply -f qloudpulse-agent.yaml
```

If you change a value later, also run `kubectl -n qloudpulse rollout restart deployment/qloudpulse-agent`. The file has
the chart's default settings. To change others, such as which labels to send, edit its ConfigMap or use the chart.

## Check that it runs

```bash
kubectl -n qloudpulse get pods
```

```bash
kubectl -n qloudpulse logs deploy/qloudpulse-agent --tail=20
```

A 401 in the log means the token is wrong. A timeout or "connection refused" means the pod cannot reach the address, or a
node's kubelet on port 10250. Tell us if your security groups or network policies block that port.

## Settings

| Setting | Default | |
|---|---|---|
| `clusterName` | required | On EKS, the EKS cluster's own name, which is how a node is matched to its EC2 instance |
| `endpoint` | required | The address from qloudpulse |
| `token` | required, unless `existingSecret` is set | Your company's token |
| `existingSecret` | none | A Secret you manage, with the token under the key `token` |
| `environment` | `default` | What reports call this cluster's environment |
| `environmentNamespaceLabel` | none | A namespace label that holds the environment, for clusters that share several |
| `labels.pod`, `labels.namespace` | none | The labels to send, for grouping costs |
| `collectionInterval` | `30s` | How often to read the cluster |
| `resources`, `goMemLimit` | see [`values.yaml`](charts/qloudpulse-agent/values.yaml) | Raise them together for clusters of more than about 100 nodes |
| `nodeSelector`, `tolerations`, `affinity`, `podAnnotations`, `priorityClassName`, `imagePullSecrets` | none | The usual pod settings |

## What it sends

Seven metrics, every 30 seconds, for each pod and node. Nothing else leaves the cluster, because a filter in the config
drops everything else.

| Metric | From | Says |
|---|---|---|
| `k8s_container_cpu_request`, `k8s_container_memory_request_bytes` | Kubernetes API | What each container requested |
| `k8s_pod_phase` | Kubernetes API | Whether each pod is running |
| `k8s_node_allocatable_cpu`, `k8s_node_allocatable_memory_bytes` | Kubernetes API | What each node has to give |
| `k8s_pod_cpu_usage`, `k8s_pod_memory_working_set_bytes` | Kubelet | What each pod used |

Each carries the names of its cluster, namespace, pod, container, node and owning workload. Pod and namespace labels are
sent only if you list them, on `k8s_pod_phase` alone. No logs, traces, environment variables, images or Secrets are sent.

## What it may read

The [ClusterRole](charts/qloudpulse-agent/templates/rbac.yaml) is read-only: `get`, `list` and `watch` on namespaces,
nodes, pods, replication controllers, resource quotas, services, DaemonSets, Deployments, ReplicaSets, StatefulSets,
CronJobs, Jobs and HorizontalPodAutoscalers, and `get` on `nodes/stats`.

It has no access to Secrets, ConfigMaps, pod logs or exec. It does not use `nodes/proxy`, because `get` on that can run
commands in pods through the kubelet. The pod runs as a non-root user with a read-only filesystem and no capabilities.

## What it uses

It asks for 25m CPU and 96 Mi of memory, with a memory limit of 512 Mi and no CPU limit. On a one-node test cluster with
six pods it used 4m CPU and 51 Mi of memory. It has not been measured on a large cluster yet, and it grows with the number
of nodes and pods.

## Remove it

```bash
helm uninstall qloudpulse-agent --namespace qloudpulse
```

Or, if you used the plain YAML:

```bash
kubectl delete -f qloudpulse-agent.yaml
```

## Changing the chart

`manifests/qloudpulse-agent.yaml` is generated from the chart, so the two cannot differ. After a change to the chart, run
`scripts/render-manifest.sh` and commit both.
