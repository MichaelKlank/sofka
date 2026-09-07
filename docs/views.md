# Views and thresholds

## Custom views

Define table columns for any resource. Most useful for custom resources, which
otherwise fall back to NAME/AGE. sofka keys views by apiVersion/plural
(`"cert-manager.io/v1/certificates"`, `"v1/pods"`), group/plural, bare plural, or
lowercase kind. The most specific key wins.

```toml
[views."cert-manager.io/v1/certificates"]
sort = "EXPIRES:desc"     # initial sort column, ":asc" (default) or ":desc";
                          # a sort you pick in the TUI (S/I/header click) is
                          # saved per kind by default and has priority
                          # set remember_sort = false at the top level to disable
# replace = true          # replace the curated columns instead of overlaying

[[views."cert-manager.io/v1/certificates".columns]]
name = "READY"
path = "Ready"            # the condition *type* name, found wherever it is
type = "condition"        # in the array — order isn't guaranteed by anything

[[views."cert-manager.io/v1/certificates".columns]]
name = "EXPIRES"
path = "/status/notAfter"
type = "time"             # rendered as elapsed ("3d4h") / "in 30d"

[[views."cert-manager.io/v1/certificates".columns]]
name = "ISSUER"
path = "/spec/issuerRef/name"
wide = true               # only shown in wide mode (`w`)
```

`path` is a JSON Pointer (RFC 6901) into the object as the API serves it:
`/metadata/…`, `/spec/…`, `/status/…`, and array indices like
`/spec/ports/0/port`.

`type` is `text` (default), `status`, `number`, `quantity` (`500m`, `1Gi`),
`time`, or `condition`. Typed columns sort by value, not by text.

For a `condition` column, `path` is the condition **type name** (`Ready`,
`Available`, `Reconciling`, …). sofka finds it in `status.conditions` by name -
never by array index, whose order nothing guarantees - renders its `status`
(`True`/`False`/`Unknown`), and colors the row like a `status` column.

Optional `width` (for fixed columns) and `align` (`left`/`center`/`right`) tune
the layout. By default columns overlay the curated ones: a matching header
replaces it in place, new columns go before AGE. Invalid entries are skipped with
a warning in the app - they never take down the TUI.

### Built-in and metric columns

Set exactly one source for each column: `path`, `builtin`, or `metric`.
Use `builtin` to retain an existing computed column, such as `READY` or `AGE`.
Its value comes from the selected resource's built-in view. Use `metric` for
live usage, resource totals, or percentages. `name` sets the displayed header.
`type` applies only to `path` columns. All sources support `wide`, `width`,
and `align`.

This example puts CPU usage and request utilization before memory usage and AGE:

```toml
[views."v1/pods"]
replace = true
columns = [
  { name = "NAME", builtin = "NAME" },
  { name = "READY", builtin = "READY" },
  { name = "STATUS", builtin = "STATUS" },
  { name = "RESTARTS", builtin = "RESTARTS" },
  { name = "CPU", metric = "cpu" },
  { name = "CPU/R", metric = "cpu-request" },
  { name = "%CPU/R", metric = "cpu-request-utilization" },
  { name = "%CPU/L", metric = "cpu-limit-utilization", wide = true },
  { name = "MEM", metric = "memory" },
  { name = "%MEM/R", metric = "memory-request-utilization" },
  { name = "%MEM/L", metric = "memory-limit-utilization", wide = true },
  { name = "AGE", builtin = "AGE" },
]
```

With `replace = true` and at least one valid `builtin` or `metric` column,
only the declared columns are used, in declaration order. A namespace column
is still added in all-namespaces mode. Without `replace`, columns overlay
the built-in view. Default metrics remain unless their source or header is
already declared, including a declaration with `wide = true`.
Existing configurations that use only `path` retain their default metric
columns. A custom CPU or MEM header prevents a duplicate default header.

Available metric sources:

| Sources                                                 | Resources   | Value                                          |
| ------------------------------------------------------- | ----------- | ---------------------------------------------- |
| `cpu`, `memory`                                         | Pods, nodes | Live usage                                     |
| `cpu-request`, `memory-request`                         | Pods        | Request totals                                 |
| `cpu-limit`, `memory-limit`                             | Pods        | Limit totals                                   |
| `cpu-request-utilization`, `memory-request-utilization` | Pods        | Usage as a percentage of the request           |
| `cpu-limit-utilization`, `memory-limit-utilization`     | Pods        | Usage as a percentage of the limit             |
| `node-pods`                                             | Nodes       | Pod count                                      |
| `node-cpu-utilization`, `node-memory-utilization`       | Nodes       | Usage as a percentage of allocatable resources |

Pod totals sum application containers and native sidecars
(`initContainers` with `restartPolicy = "Always"`). A declaration in
`spec.resources` takes priority for that resource and request or limit.
Regular init containers and pod overhead are excluded. These are workload
totals after startup, not the effective values used for scheduling.
Pod percentages can hide a container that is near its own limit. Open the
container picker to check individual containers.

For requests, unset container values contribute zero. If all values are
unset, the total is `-`. For limits, the total is `-` if any included
container has no limit, unless a pod-level limit is set. A percentage is
`-` when its request or limit is missing or zero, or usage is unavailable.
A measured zero usage is `0m`, `0Mi`, or `0%`. Request and limit totals
remain available without Metrics Server.

Sorting uses numeric values. Missing values sort first in ascending order,
as in the default CPU and MEM columns. Structured filters use the displayed
header, without regard to letter case: `cpu/r>4`, `%cpu/r>=75`,
`%mem/l>=90%`, or `mem/r>=1Gi`. CPU quantities without a suffix are cores.
Memory quantities without a suffix are bytes. Percentage values are points
from zero, with an optional `%` suffix. Metric filters and sorts update
when a new sample arrives. Use structured filters to search metric values.
The existing `cpu`, `mem`, and `memory` usage filters remain available when
those default columns are hidden.

CPU and memory values use the existing resource threshold bands.
Percentages use `[thresholds].utilization`, including per-resource overrides.
Duplicate headers, invalid source combinations, and sources that are not
available for a resource produce configuration warnings.

### Namespace-specific views

Add `@<namespace>` to a view key to select a layout for one namespace:

```toml
[[views."v1/pods".columns]]
name = "OWNERKIND"
path = "/metadata/ownerReferences/0/kind"

[[views."v1/pods@matlab".columns]]
name = "TENANT"
path = "/metadata/annotations/ops.example.com~1tenant"

[[views."v1/pods@matlab".columns]]
name = "MODEL"
path = "/metadata/annotations/ops.example.com~1model"
```

When a namespaced resource view is set to one namespace, sofka tries these
keys in order:

1. `apiVersion/plural@namespace`
2. `group/plural@namespace`
3. `plural@namespace`
4. `kind@namespace`
5. The same resource keys without a namespace, in the same order.

For example, `pods@matlab` has priority over `v1/pods`.
All-namespaces mode and cluster-scoped resources use only unqualified keys.
A row filter does not change the namespace used for this lookup.

The selected layout does not inherit columns, `replace`, or `sort` from
another resource key. If it has no `sort`, `[views."*"].sort` supplies the
global default. The `"*"` key supports only the default sort; it does not
supply columns or navigation settings. A table without the specified column
ignores the global sort until that column is available. The default sort is
checked again when CRD printer columns arrive or wide mode changes. A saved
sort can replace a configured default when its column becomes available.
An active user or bookmark sort keeps its priority. Columns still overlay the built-in layout unless
`replace = true`. In this example, the `matlab` view adds TENANT and MODEL,
but does not add OWNERKIND. Wide mode works as usual. An active sort stays
on its column if that column is still present. A saved user sort has priority
over the configured initial sort when `remember_sort` is enabled (the default).
Set `remember_sort = false` at the top level of the config to stop saving
and restoring user sort choices. Sort changes still work in the active view.
On a new view start, the configured sort applies again. Existing saved
choices stay on disk while the option is disabled.

The sort picker's no-sort entry clears sorting in the active view. Column
updates, wide mode, config reload, and watch refresh keep that choice.
Opening a resource view again applies its saved or configured sort as usual.

A user or bookmark sort waits while its column is hidden or unavailable.
The table uses its natural order during that time. When the column returns,
the selected sort and direction return, even with `remember_sort = false`.
Clearing the sort or selecting another column cancels the waiting choice.

The `node` and `drill` settings use the same key order, but each setting
falls back separately. A view that sets only columns does not hide a
`node` or `drill` setting on a less specific key. Built-in navigation rules
still apply.

Namespace selection works with `sofka pods -n matlab`, the namespace picker,
resource commands, bookmarks, and view history. The namespace suffix must
be a valid namespace name: 1 to 63 lowercase letters, digits, or hyphens,
with a letter or digit at each end. Invalid suffixes produce a configuration
warning and the view is ignored.

### Pods and nodes

Custom columns also overlay sofka's curated core-resource views. Pods already
show `IP` and `NODE` after toggling wide mode with `w`, and nodes always show
`VERSION`. Press `w` in the nodes view to show `LABELS` as comma-separated
`key=value` pairs in key order. Nodes without labels show `<none>`.
Use `/` to find text in the visible labels. To filter by an exact label, press
`/`, enter `-l karpenter.sh/nodepool=default`, then press Enter. Label selectors
also work with wide mode off. Use the label key and value for your cluster.

Custom columns can show individual labels or annotations. Extra node topology
and provisioning details can come from labels:

```toml
# Karpenter example; adjust provider-specific NODEPOOL and TYPE label names.
[views."v1/nodes"]

[[views."v1/nodes".columns]]
name = "NODEPOOL"
path = "/metadata/labels/karpenter.sh~1nodepool"

[[views."v1/nodes".columns]]
name = "ZONE"
path = "/metadata/labels/topology.kubernetes.io~1zone"

[[views."v1/nodes".columns]]
name = "INSTANCE"
path = "/metadata/labels/node.kubernetes.io~1instance-type"

[[views."v1/nodes".columns]]
name = "TYPE"
path = "/metadata/labels/karpenter.sh~1capacity-type"
```

In a JSON Pointer, `/` inside a label name must be escaped as `~1`. For example,
EKS commonly uses `eks.amazonaws.com~1nodegroup` for `NODEPOOL` and
`eks.amazonaws.com~1capacityType` for `TYPE`. Add `wide = true` to any custom
column that should appear only after pressing `w`.

### Navigating between kinds

sofka's drill-downs are relationships between kinds, expressed as a watch
selector: `enter` on a Deployment lists pods under its `matchLabels`, `enter`
on a Node lists pods with `spec.nodeName` equal to its name, `o` on a pod opens
the node named in `spec.nodeName`. Those relationships are built in for core
kinds. Custom resources relate to each other the same way - an operator's
parent object owns children it labels, a claim names the node it became - but
sofka can't know a CRD's field layout in advance. A view can declare the
relationship, and `enter`/`o` then work exactly as they do for core kinds:
push the current view, open the target scoped to the row, `esc` to come back.

Two shapes cover most cases.

**A row names one node** - `node` is a JSON Pointer to the field holding the
node's name. `o` jumps to that node, and so does `enter` for a kind with no
drill-down of its own. Pods (`/spec/nodeName`) and Karpenter NodeClaims
(`/status/nodeName`) are built in; `node` adds a kind or overrides a built-in.
This is what the NodeClaim row amounts to:

```toml
[views."karpenter.sh/v1/nodeclaims"]
node = "/status/nodeName"
```

The nodes list is scoped by `metadata.name`, the only field selector the
apiserver indexes for nodes, so the pointer has to land on a name. A row whose
pointer is empty (the node isn't assigned yet) warns instead of opening an empty
list; a pointer that lands on something other than a string warns that the
pointer is wrong.

**A row selects other objects** - `drill` names the kind `enter` should open and
how to scope it: a label selector (`labels`), a field selector (`fields`), or
both, with `{name}` and `{namespace}` filled in from the row:

```toml
# children carry the parent's name in a label
[views."karpenter.sh/v1/nodepools"]
drill = { kind = "nodeclaims", labels = "karpenter.sh/nodepool={name}" }

# the target is a single object with a known name and nothing labelling it back
[views.externalsecrets]
drill = { kind = "secrets", fields = "metadata.name={name}" }
```

Prefer `labels` when the target carries a label pointing back at the row -
that's how most operators mark what they own. Use `fields` when it doesn't:
`metadata.name` and `metadata.namespace` are selectable on every kind, other
fields only where the apiserver indexes them. `kind` is anything `:` accepts
(alias, plural, or kind) and is resolved when you press `enter`, so an unknown
kind warns and stays put. A namespaced target opens in the row's namespace; a
cluster-scoped one ignores it.

Kinds with a built-in drill-down keep it - a `drill` on pods won't replace the
container picker, and `:config` warns that the stanza is ignored. When a view
sets both `drill` and `node`, `enter` drills and `o` still jumps to the node.

**Everything connected to a row** - `u` opens the adjacent view: the row's
owners, the objects it owns, the objects its spec names, and the objects whose
specs name it, each one `⏎` away. Core kinds are built in (a pod's node, claims,
ConfigMaps, Secrets and service account; a claim's storage class, attributes
class and volume; an ingress's services and TLS secrets). A CRD declares its
own references and owned kinds:

```toml
[views."karpenter.sh/v1/nodeclaims"]
children = ["nodes"]                 # kinds to scan for ownerReferences to the row

[[views."karpenter.sh/v1/nodeclaims".refs]]
path = "/spec/nodeClassRef/name"     # JSON Pointer; `*` fans out over an array
kind = "ec2nodeclasses"
relation = "shaped by"               # row label; default "references"
reverse = "cluster"                  # usages listed: namespace (default) | cluster | none
```

`reverse` decides how far the lookup goes when the _target_ is selected: with
`namespace`, the referencing kind is listed in the row's namespace (the
namespace the table shows, for a cluster-scoped row); with `cluster`, across the
cluster; `none` skips it. A `namespace_path` names where the target's namespace
lives when it isn't the row's own, as a PersistentVolume's `claimRef.namespace`.

Both settings are resolved key by key across the view keys for a kind
(`apiVersion/plural`, `group/plural`, plural, kind), not off the single most
specific view the way `columns` and `sort` are. A specific view that only sets
columns therefore doesn't hide a `node` or `drill` set under a broader key.

### CRD printer columns

A custom resource with no explicit view picks up its CRD
`additionalPrinterColumns` automatically (columns with `priority > 0` become
wide-only). Built-in columns match both the API group and resource name.
For example, core Services keep their network columns, while
`services.serving.knative.dev` uses the Knative CRD columns. Printer columns
and cached resource rows stay separate for each API group and version.
An explicit user view with columns still takes precedence. A condition lookup
(`.status.conditions[?(@.type=="Ready")].status` - how most CRDs express their
READY column) becomes a `condition` column, found by type name. The same filter
selecting another field (`.reason`, `.message`, `.lastTransitionTime`, …) keeps
the column and reads that field from the named condition. Other JSONPath filter
or wildcard expressions aren't representable and those columns are skipped. So
most custom resources get useful columns with zero configuration.

## Thresholds

The warning and critical values behind RESTARTS/CPU/MEM cell color (and the
request/limit utilization in the container picker) are configurable. Anything you
don't set keeps the sofka default, so an empty config colors exactly as before.

Global `[thresholds]` apply everywhere. `[thresholds.resources.<key>]` overrides
per resource (keyed like `[views]`). Like every section, a per-cluster or
per-context override file can retune them for one context. Thresholds re-apply
live on `:reload`.

```toml
[thresholds]
restarts    = { warn = 3, critical = 10 }       # count
cpu         = { warn = "200m", critical = "1" } # absolute usage
memory      = { warn = "256Mi", critical = "1Gi" }
utilization = { warn = 75, critical = 90 }      # percent of request/limit

[thresholds.resources.pods]                     # per-kind override
restarts = { warn = 5, critical = 20 }
```

Omit either bound of a band to disable that level. `warn` is peach, `critical`
is red.
