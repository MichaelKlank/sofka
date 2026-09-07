//! Adjacency: the objects directly connected to one object — what owns it,
//! what it owns, what its spec names, and what names it. Pure: the rule
//! tables and the pointer walking live here, unit-tested; `app/adjacent.rs`
//! fetches the objects and drives the view.

use std::collections::HashMap;

use kube::discovery::ApiResource;
use serde_json::Value;

use crate::views::{View, lookup_keys};

/// How the reverse direction of a [`RefRule`] is listed: which objects of the
/// rule's source kind name the selected object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Reverse {
    /// List the source kind in the selected row's namespace — the namespace
    /// the table is showing, for a cluster-scoped row.
    #[default]
    Namespace,
    /// List the source kind across the cluster.
    Cluster,
    /// Don't look for usages through this rule.
    None,
}

impl Reverse {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "namespace" => Some(Self::Namespace),
            "cluster" => Some(Self::Cluster),
            "none" => Some(Self::None),
            _ => None,
        }
    }
}

/// One reference from objects of a kind to objects of another kind: "a Pod's
/// `spec.nodeName` names a Node".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefRule {
    /// The source kind, keyed like `[views."…"]`: `apiVersion/plural`,
    /// `group/plural`, or a bare plural.
    pub from: String,
    /// JSON Pointer into the source object; a `*` segment fans out over every
    /// element of an array.
    pub path: String,
    /// Where the target's namespace lives when it isn't the source's own — a
    /// PersistentVolume's `claimRef.namespace`. Paired with `path` by position.
    pub namespace_path: Option<String>,
    /// Target kind (alias, plural, or kind), resolved against the cluster.
    pub kind: String,
    /// Relation label shown on the row: "mounts", "runs on".
    pub relation: String,
    pub reverse: Reverse,
}

struct BuiltinRef {
    from: &'static str,
    path: &'static str,
    namespace_path: Option<&'static str>,
    kind: &'static str,
    relation: &'static str,
    reverse: Reverse,
}

impl BuiltinRef {
    fn rule(&self) -> RefRule {
        RefRule {
            from: self.from.to_string(),
            path: self.path.to_string(),
            namespace_path: self.namespace_path.map(str::to_string),
            kind: self.kind.to_string(),
            relation: self.relation.to_string(),
            reverse: self.reverse,
        }
    }
}

const fn r(
    from: &'static str,
    path: &'static str,
    kind: &'static str,
    relation: &'static str,
    reverse: Reverse,
) -> BuiltinRef {
    BuiltinRef {
        from,
        path,
        namespace_path: None,
        kind,
        relation,
        reverse,
    }
}

/// References between the kinds common enough to ship. Data, not control
/// flow: `[[views."…".refs]]` adds rows for anything else, and the reverse
/// direction of every row is what finds usages ("pods mounting this PVC").
const BUILTIN_REFS: &[BuiltinRef] = &[
    r(
        "v1/pods",
        "/spec/nodeName",
        "nodes",
        "runs on",
        Reverse::Namespace,
    ),
    r(
        "v1/pods",
        "/spec/serviceAccountName",
        "serviceaccounts",
        "runs as",
        Reverse::Namespace,
    ),
    r(
        "v1/pods",
        "/spec/priorityClassName",
        "priorityclasses",
        "prioritized by",
        Reverse::None,
    ),
    r(
        "v1/pods",
        "/spec/volumes/*/persistentVolumeClaim/claimName",
        "persistentvolumeclaims",
        "mounts",
        Reverse::Namespace,
    ),
    r(
        "v1/pods",
        "/spec/volumes/*/configMap/name",
        "configmaps",
        "mounts",
        Reverse::Namespace,
    ),
    r(
        "v1/pods",
        "/spec/volumes/*/secret/secretName",
        "secrets",
        "mounts",
        Reverse::Namespace,
    ),
    r(
        "v1/pods",
        "/spec/volumes/*/projected/sources/*/configMap/name",
        "configmaps",
        "mounts",
        Reverse::Namespace,
    ),
    r(
        "v1/pods",
        "/spec/volumes/*/projected/sources/*/secret/name",
        "secrets",
        "mounts",
        Reverse::Namespace,
    ),
    r(
        "v1/pods",
        "/spec/containers/*/envFrom/*/configMapRef/name",
        "configmaps",
        "reads",
        Reverse::Namespace,
    ),
    r(
        "v1/pods",
        "/spec/containers/*/envFrom/*/secretRef/name",
        "secrets",
        "reads",
        Reverse::Namespace,
    ),
    r(
        "v1/pods",
        "/spec/containers/*/env/*/valueFrom/configMapKeyRef/name",
        "configmaps",
        "reads",
        Reverse::Namespace,
    ),
    r(
        "v1/pods",
        "/spec/containers/*/env/*/valueFrom/secretKeyRef/name",
        "secrets",
        "reads",
        Reverse::Namespace,
    ),
    r(
        "v1/pods",
        "/spec/imagePullSecrets/*/name",
        "secrets",
        "pulls with",
        Reverse::Namespace,
    ),
    r(
        "v1/persistentvolumeclaims",
        "/spec/storageClassName",
        "storageclasses",
        "provisioned by",
        Reverse::Namespace,
    ),
    r(
        "v1/persistentvolumeclaims",
        "/spec/volumeAttributesClassName",
        "volumeattributesclasses",
        "attributes from",
        Reverse::Namespace,
    ),
    r(
        "v1/persistentvolumeclaims",
        "/spec/volumeName",
        "persistentvolumes",
        "bound to",
        Reverse::None,
    ),
    BuiltinRef {
        from: "v1/persistentvolumes",
        path: "/spec/claimRef/name",
        namespace_path: Some("/spec/claimRef/namespace"),
        kind: "persistentvolumeclaims",
        relation: "bound to",
        reverse: Reverse::None,
    },
    r(
        "v1/persistentvolumes",
        "/spec/storageClassName",
        "storageclasses",
        "provisioned by",
        Reverse::Cluster,
    ),
    r(
        "v1/serviceaccounts",
        "/secrets/*/name",
        "secrets",
        "tokens in",
        Reverse::None,
    ),
    r(
        "networking.k8s.io/ingresses",
        "/spec/rules/*/http/paths/*/backend/service/name",
        "services",
        "routes to",
        Reverse::Namespace,
    ),
    r(
        "networking.k8s.io/ingresses",
        "/spec/defaultBackend/service/name",
        "services",
        "routes to",
        Reverse::Namespace,
    ),
    r(
        "networking.k8s.io/ingresses",
        "/spec/tls/*/secretName",
        "secrets",
        "tls from",
        Reverse::Namespace,
    ),
    r(
        "networking.k8s.io/ingresses",
        "/spec/ingressClassName",
        "ingressclasses",
        "class",
        Reverse::None,
    ),
    r(
        "karpenter.sh/nodeclaims",
        "/status/nodeName",
        "nodes",
        "became",
        Reverse::Cluster,
    ),
];

/// Kinds owned through `ownerReferences` by objects of a kind — where a
/// row's children are looked for. `[views."…"].children` adds to it.
const BUILTIN_CHILDREN: &[(&str, &[&str])] = &[
    ("apps/deployments", &["replicasets"]),
    ("apps/replicasets", &["pods"]),
    ("apps/statefulsets", &["pods", "controllerrevisions"]),
    ("apps/daemonsets", &["pods", "controllerrevisions"]),
    ("batch/jobs", &["pods"]),
    ("batch/cronjobs", &["jobs"]),
    ("v1/services", &["endpointslices"]),
    ("karpenter.sh/nodepools", &["nodeclaims"]),
    ("karpenter.sh/nodeclaims", &["nodes"]),
];

/// Every reference whose source is `ar`: built-in rows keyed like the view
/// keys, then the `refs` of every configured view for the kind. Additive
/// across keys — a specific view's rows don't hide a broader key's.
pub fn rules_for(views: &HashMap<String, View>, ar: &ApiResource) -> Vec<RefRule> {
    let keys = lookup_keys(ar);
    let mut rules: Vec<RefRule> = BUILTIN_REFS
        .iter()
        .filter(|b| keys.iter().any(|k| k == b.from))
        .map(BuiltinRef::rule)
        .collect();
    for key in &keys {
        if let Some(view) = views.get(key) {
            rules.extend(view.refs.iter().cloned());
        }
    }
    rules
}

/// The plurals to scan for a row's children, built-in and configured.
pub fn children_for(views: &HashMap<String, View>, ar: &ApiResource) -> Vec<String> {
    let keys = lookup_keys(ar);
    let mut plurals: Vec<String> = BUILTIN_CHILDREN
        .iter()
        .filter(|(from, _)| keys.iter().any(|k| k == from))
        .flat_map(|(_, kinds)| kinds.iter().map(|k| k.to_string()))
        .collect();
    for key in &keys {
        if let Some(view) = views.get(key) {
            plurals.extend(view.children.iter().cloned());
        }
    }
    plurals.dedup();
    plurals
}

/// Every reference known, built-in and configured. The reverse lookup scans
/// these for rules whose target is the selected kind.
pub fn all_rules(views: &HashMap<String, View>) -> Vec<RefRule> {
    let mut rules: Vec<RefRule> = BUILTIN_REFS.iter().map(BuiltinRef::rule).collect();
    for view in views.values() {
        rules.extend(view.refs.iter().cloned());
    }
    rules
}

/// The strings at `path` in `obj`. A `*` segment fans out over every element
/// of an array, so one pointer can name every claim a pod mounts. Empty
/// strings and non-strings are dropped; order follows the document.
pub fn pointer_values(obj: &Value, path: &str) -> Vec<String> {
    let Some(rest) = path.strip_prefix('/') else {
        return Vec::new();
    };
    let segments: Vec<String> = rest
        .split('/')
        .map(|s| s.replace("~1", "/").replace("~0", "~"))
        .collect();
    let mut out = Vec::new();
    collect(obj, &segments, &mut out);
    out
}

fn collect(node: &Value, segments: &[String], out: &mut Vec<String>) {
    let Some((head, tail)) = segments.split_first() else {
        if let Value::String(s) = node
            && !s.is_empty()
        {
            out.push(s.clone());
        }
        return;
    };
    match (head.as_str(), node) {
        ("*", Value::Array(items)) => {
            for item in items {
                collect(item, tail, out);
            }
        }
        (key, Value::Object(map)) => {
            if let Some(child) = map.get(key) {
                collect(child, tail, out);
            }
        }
        (index, Value::Array(items)) => {
            if let Some(child) = index.parse::<usize>().ok().and_then(|i| items.get(i)) {
                collect(child, tail, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ar(group: &str, kind: &str, plural: &str) -> ApiResource {
        ApiResource {
            group: group.into(),
            version: "v1".into(),
            api_version: if group.is_empty() {
                "v1".into()
            } else {
                format!("{group}/v1")
            },
            kind: kind.into(),
            plural: plural.into(),
        }
    }

    #[test]
    fn pointer_values_fan_out_over_arrays() {
        let pod = json!({"spec": {
            "nodeName": "ip-10-0-1-2",
            "volumes": [
                {"name": "data", "persistentVolumeClaim": {"claimName": "data-db-0"}},
                {"name": "cfg", "configMap": {"name": "db-config"}},
                {"name": "more", "persistentVolumeClaim": {"claimName": "wal-db-0"}},
            ],
            "containers": [{"envFrom": [{"secretRef": {"name": "db-creds"}}, {"configMapRef": {"name": "db-env"}}]}],
        }});
        assert_eq!(pointer_values(&pod, "/spec/nodeName"), ["ip-10-0-1-2"]);
        assert_eq!(
            pointer_values(&pod, "/spec/volumes/*/persistentVolumeClaim/claimName"),
            ["data-db-0", "wal-db-0"]
        );
        assert_eq!(
            pointer_values(&pod, "/spec/containers/*/envFrom/*/secretRef/name"),
            ["db-creds"]
        );
        assert_eq!(
            pointer_values(&pod, "/spec/volumes/1/configMap/name"),
            ["db-config"]
        );
        // Missing, non-string, and empty values contribute nothing.
        assert!(pointer_values(&pod, "/spec/serviceAccountName").is_empty());
        assert!(pointer_values(&pod, "/spec/volumes").is_empty());
        assert!(pointer_values(&json!({"a": ""}), "/a").is_empty());
        assert!(pointer_values(&pod, "spec/nodeName").is_empty());
        // Escaped keys, as in `path` for columns.
        let labelled = json!({"metadata": {"labels": {"karpenter.sh/nodepool": "default"}}});
        assert_eq!(
            pointer_values(&labelled, "/metadata/labels/karpenter.sh~1nodepool"),
            ["default"]
        );
    }

    #[test]
    fn builtin_rules_are_keyed_like_views() {
        let views = HashMap::new();
        let pods = rules_for(&views, &ar("", "Pod", "pods"));
        assert!(
            pods.iter()
                .any(|r| r.kind == "nodes" && r.relation == "runs on")
        );
        assert!(pods.iter().any(|r| r.kind == "persistentvolumeclaims"));
        // A same-named plural in another group gets nothing: PodMetrics
        // shares `pods` but has no spec to point into.
        assert!(rules_for(&views, &ar("metrics.k8s.io", "PodMetrics", "pods")).is_empty());
        let pv = rules_for(&views, &ar("", "PersistentVolume", "persistentvolumes"));
        let claim = pv
            .iter()
            .find(|r| r.kind == "persistentvolumeclaims")
            .unwrap();
        assert_eq!(
            claim.namespace_path.as_deref(),
            Some("/spec/claimRef/namespace")
        );
        assert_eq!(
            children_for(&views, &ar("apps", "Deployment", "deployments")),
            ["replicasets"]
        );
        assert!(children_for(&views, &ar("", "Secret", "secrets")).is_empty());
    }

    #[test]
    fn configured_refs_and_children_add_to_the_builtin_rows() {
        let (views, warnings) = crate::views::compile(
            &toml::from_str::<crate::config::Config>(
                r#"
                [views."karpenter.sh/v1/nodeclaims"]
                children = ["nodes"]

                [[views."karpenter.sh/v1/nodeclaims".refs]]
                path = "/spec/nodeClassRef/name"
                kind = "ec2nodeclasses"
                relation = "shaped by"
                reverse = "cluster"

                [[views.pods.refs]]
                path = "/metadata/annotations/example.com~1owner"
                kind = "teams"
                "#,
            )
            .unwrap()
            .views,
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        let claims = ar("karpenter.sh", "NodeClaim", "nodeclaims");
        let rules = rules_for(&views, &claims);
        // The built-in node row and the configured class row both apply.
        assert!(rules.iter().any(|r| r.kind == "nodes"));
        let class = rules.iter().find(|r| r.kind == "ec2nodeclasses").unwrap();
        assert_eq!(
            (class.relation.as_str(), class.reverse),
            ("shaped by", Reverse::Cluster)
        );
        assert_eq!(children_for(&views, &claims), ["nodes"]);

        let pods = rules_for(&views, &ar("", "Pod", "pods"));
        let team = pods.iter().find(|r| r.kind == "teams").unwrap();
        assert_eq!(
            (team.relation.as_str(), team.reverse),
            ("references", Reverse::Namespace)
        );
        assert!(all_rules(&views).iter().any(|r| r.kind == "teams"));
    }
}
