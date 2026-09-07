//! Adjacency: the objects directly connected to one object — what owns it,
//! what it owns, what its spec names, and what names it. Pure: the rule
//! tables, the pointer walking, and the gather plan live here, unit-tested;
//! `app/adjacent.rs` runs the plan against the cluster and drives the view.

use std::collections::{HashMap, HashSet};

use kube::core::DynamicObject;
use kube::discovery::ApiResource;
use serde_json::Value;

use crate::store::AdjacentItem;
use crate::views::{View, key_namespace, key_plural, lookup_keys};

/// Which way a connection runs, as the row shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    /// The selection's owner (`↑ owned by`).
    Owner,
    /// An object the selection owns (`↓ owns`).
    Child,
    /// An object the selection's spec names (`→ mounts`).
    Names,
    /// An object whose spec names the selection (`← mounts`).
    NamedBy,
}

impl Direction {
    pub fn arrow(self) -> &'static str {
        match self {
            Self::Owner => "↑",
            Self::Child => "↓",
            Self::Names => "→",
            Self::NamedBy => "←",
        }
    }
}

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
    /// PersistentVolume's `claimRef.namespace`. Its `*` segments take the
    /// same array elements `path` did, so each name pairs with its own
    /// namespace.
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

/// Every reference whose source is `ar` in `namespace`: built-in rows keyed
/// like the view keys, then the `refs` of every configured view for the kind,
/// `@namespace`-qualified views included. Additive across keys — a specific
/// view's rows don't hide a broader key's.
pub fn rules_for(
    views: &HashMap<String, View>,
    ar: &ApiResource,
    namespace: Option<&str>,
) -> Vec<RefRule> {
    let keys = lookup_keys(ar, namespace);
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

/// The plurals to scan for a row's children, built-in and configured, each
/// once.
pub fn children_for(
    views: &HashMap<String, View>,
    ar: &ApiResource,
    namespace: Option<&str>,
) -> Vec<String> {
    let keys = lookup_keys(ar, namespace);
    let builtin = BUILTIN_CHILDREN
        .iter()
        .filter(|(from, _)| keys.iter().any(|k| k == from))
        .flat_map(|(_, kinds)| kinds.iter().map(|k| k.to_string()));
    let configured = keys
        .iter()
        .filter_map(|k| views.get(k))
        .flat_map(|v| v.children.iter().cloned());
    let mut seen = HashSet::new();
    builtin
        .chain(configured)
        .filter(|p| seen.insert(p.clone()))
        .collect()
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

// ----- pointers ---------------------------------------------------------

fn segments(path: &str) -> Option<Vec<String>> {
    let rest = path.strip_prefix('/')?;
    Some(
        rest.split('/')
            .map(|s| s.replace("~1", "/").replace("~0", "~"))
            .collect(),
    )
}

/// Walk `segments` from `node`, fanning out at `*`, and record every
/// non-empty string reached together with the array indices the `*`s took.
fn expand(
    node: &Value,
    segments: &[String],
    taken: &mut Vec<usize>,
    out: &mut Vec<(Vec<usize>, String)>,
) {
    let Some((head, tail)) = segments.split_first() else {
        if let Value::String(s) = node
            && !s.is_empty()
        {
            out.push((taken.clone(), s.clone()));
        }
        return;
    };
    match (head.as_str(), node) {
        ("*", Value::Array(items)) => {
            for (i, item) in items.iter().enumerate() {
                taken.push(i);
                expand(item, tail, taken, out);
                taken.pop();
            }
        }
        (key, Value::Object(map)) => {
            if let Some(child) = map.get(key) {
                expand(child, tail, taken, out);
            }
        }
        (index, Value::Array(items)) => {
            if let Some(child) = index.parse::<usize>().ok().and_then(|i| items.get(i)) {
                expand(child, tail, taken, out);
            }
        }
        _ => {}
    }
}

/// The strings at `path` in `obj`. A `*` segment fans out over every element
/// of an array, so one pointer can name every claim a pod mounts. Empty
/// strings and non-strings are dropped; order follows the document.
pub fn pointer_values(obj: &Value, path: &str) -> Vec<String> {
    pointer_pairs(obj, path, None)
        .into_iter()
        .map(|(name, _)| name)
        .collect()
}

/// [`pointer_values`], each paired with the string at `namespace_path` for
/// the same array elements — the `*`s there take the indices `path` took, so
/// an element without a namespace yields `None` rather than shifting the
/// namespaces of the elements after it.
pub fn pointer_pairs(
    obj: &Value,
    path: &str,
    namespace_path: Option<&str>,
) -> Vec<(String, Option<String>)> {
    let Some(segs) = segments(path) else {
        return Vec::new();
    };
    let mut hits = Vec::new();
    expand(obj, &segs, &mut Vec::new(), &mut hits);
    let ns_segs = namespace_path.and_then(segments);
    hits.into_iter()
        .map(|(taken, name)| {
            let ns = ns_segs
                .as_deref()
                .and_then(|segs| value_at(obj, segs, &taken));
            (name, ns)
        })
        .collect()
}

/// The string at a pointer whose `*`s are filled from `indices`, in order.
fn value_at(obj: &Value, segs: &[String], indices: &[usize]) -> Option<String> {
    let mut node = obj;
    let mut idx = indices.iter();
    for seg in segs {
        node = match (seg.as_str(), node) {
            ("*", Value::Array(items)) => items.get(*idx.next()?)?,
            (key, Value::Object(map)) => map.get(key)?,
            (index, Value::Array(items)) => items.get(index.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    match node {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

// ----- the gather plan --------------------------------------------------

/// A kind resolved against the cluster, as the gather needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KindRef {
    pub ar: ApiResource,
    pub namespaced: bool,
    pub plural: String,
}

/// How a plan looks kinds up. The app answers from the cluster's registry;
/// tests answer from a table.
pub trait Kinds {
    /// By alias, plural, or kind — what `:` accepts.
    fn by_name(&self, name: &str) -> Option<KindRef>;
    /// By kind within an API group, as an `ownerReference` names it — so a
    /// kind shared between groups (`Cluster`) resolves to the right one.
    fn by_kind_in_group(&self, kind: &str, group: &str) -> Option<KindRef>;
}

/// Resolve a `[views."…"]`-style key (`v1/pods`, `apps/deployments`, `pods`)
/// to a kind, holding it to the group or apiVersion the key names.
pub fn resolve_view_key(kinds: &impl Kinds, key: &str) -> Option<KindRef> {
    let resolved = kinds.by_name(key_plural(key))?;
    let Some((prefix, _)) = key.rsplit_once('/') else {
        return Some(resolved);
    };
    let prefix = prefix.to_lowercase();
    let ar = &resolved.ar;
    (prefix == ar.api_version.to_lowercase() || prefix == ar.group.to_lowercase())
        .then_some(resolved)
}

/// A rule read forwards from the selection: the objects it names, as
/// (name, namespace) pairs to read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Forward {
    pub rule: RefRule,
    pub target: KindRef,
    pub refs: Vec<(String, String)>,
}

/// A rule read backwards: list `from` in `scope` ("" = all namespaces) and
/// keep the objects that name the selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backward {
    pub rule: RefRule,
    pub from: KindRef,
    pub scope: String,
}

/// Everything the gather will read for one selection, decided up front on
/// the UI thread where the kind registry lives.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Plan {
    /// Owners to GET, from `ownerReferences`.
    pub owners: Vec<(KindRef, String)>,
    /// Kinds to LIST and match by owner UID.
    pub children: Vec<KindRef>,
    pub forward: Vec<Forward>,
    pub backward: Vec<Backward>,
    /// A reference the plan couldn't follow — an owner kind that isn't served.
    pub warn: Option<String>,
}

/// Build the plan for `obj`, an object of `source`. `table_ns` is the
/// namespace the table shows, used as the usage scope for a cluster-scoped
/// row that has none of its own.
pub fn plan(
    views: &HashMap<String, View>,
    kinds: &impl Kinds,
    source: &KindRef,
    obj: &DynamicObject,
    table_ns: &str,
) -> Plan {
    let ns = obj.metadata.namespace.clone().unwrap_or_default();
    let scope_ns = if ns.is_empty() { table_ns } else { ns.as_str() };
    let row_ns = (!ns.is_empty()).then_some(ns.as_str());
    let mut plan = Plan::default();

    for o in obj.metadata.owner_references.iter().flatten() {
        let group = o.api_version.split_once('/').map_or("", |(g, _)| g);
        match kinds.by_kind_in_group(&o.kind, group) {
            Some(kind) => plan.owners.push((kind, o.name.clone())),
            None => {
                plan.warn.get_or_insert(format!(
                    "owner kind {} ({}) is not served here",
                    o.kind, o.api_version
                ));
            }
        }
    }
    plan.children = children_for(views, &source.ar, row_ns)
        .iter()
        .filter_map(|p| kinds.by_name(p))
        .collect();

    let value = serde_json::to_value(obj).unwrap_or(Value::Null);
    // A rule whose target kind isn't served here — a class from a CSI driver
    // that isn't installed — simply contributes nothing.
    for rule in rules_for(views, &source.ar, row_ns) {
        let Some(target) = kinds.by_name(&rule.kind) else {
            continue;
        };
        let refs: Vec<(String, String)> =
            pointer_pairs(&value, &rule.path, rule.namespace_path.as_deref())
                .into_iter()
                .map(|(name, target_ns)| (name, target_ns.unwrap_or_else(|| ns.clone())))
                .collect();
        if !refs.is_empty() {
            plan.forward.push(Forward { rule, target, refs });
        }
    }
    for rule in all_rules(views) {
        if rule.reverse == Reverse::None {
            continue;
        }
        let Some(target) = kinds.by_name(&rule.kind) else {
            continue;
        };
        if target.ar.plural != source.ar.plural || target.ar.group != source.ar.group {
            continue;
        }
        let Some(from) = resolve_view_key(kinds, &rule.from) else {
            continue;
        };
        // A rule declared under `kind@namespace` describes that namespace's
        // objects only, so that's where its usages are looked for.
        let scope = match (key_namespace(&rule.from), rule.reverse) {
            (Some(pinned), _) if from.namespaced => pinned.to_string(),
            (None, Reverse::Namespace) if from.namespaced => scope_ns.to_string(),
            _ => String::new(),
        };
        plan.backward.push(Backward { rule, from, scope });
    }
    plan
}

// ----- matching the gathered objects ------------------------------------

/// Whether `o` lists `uid` among its owners.
pub fn owned_by(o: &DynamicObject, uid: Option<&str>) -> bool {
    let Some(uid) = uid else {
        return false;
    };
    o.metadata
        .owner_references
        .iter()
        .flatten()
        .any(|own| own.uid == uid)
}

/// Whether `o`, an object of `rule`'s source kind, names the selection. A
/// namespaced selection is named within a namespace: the one the rule's
/// `namespace_path` gives for that element, else `o`'s own.
pub fn names_source(
    o: &DynamicObject,
    rule: &RefRule,
    source_name: &str,
    source_ns: Option<&str>,
) -> bool {
    let value = serde_json::to_value(o).unwrap_or(Value::Null);
    pointer_pairs(&value, &rule.path, rule.namespace_path.as_deref())
        .into_iter()
        .any(|(name, named_ns)| {
            if name != source_name {
                return false;
            }
            let Some(source_ns) = source_ns else {
                return true;
            };
            let named_ns = match &rule.namespace_path {
                Some(_) => named_ns,
                None => o.metadata.namespace.clone(),
            };
            named_ns.as_deref() == Some(source_ns)
        })
}

/// Drop repeats — the same object reached the same way twice, as a ConfigMap
/// mounted as a volume and again through a projected one — keeping the first.
pub fn dedup(items: &mut Vec<AdjacentItem>) {
    let mut seen = HashSet::new();
    items.retain(|it| {
        seen.insert((
            it.direction,
            it.relation.clone(),
            it.plural.clone(),
            it.namespace.clone(),
            it.name.clone(),
        ))
    });
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

    fn kind(group: &str, kind: &str, plural: &str, namespaced: bool) -> KindRef {
        KindRef {
            ar: ar(group, kind, plural),
            namespaced,
            plural: plural.into(),
        }
    }

    /// A registry answering from a table, the way `Cluster::fake` would.
    struct Table(Vec<KindRef>);

    impl Kinds for Table {
        fn by_name(&self, name: &str) -> Option<KindRef> {
            self.0
                .iter()
                .find(|k| k.plural == name || k.ar.kind.eq_ignore_ascii_case(name))
                .cloned()
        }
        fn by_kind_in_group(&self, kind: &str, group: &str) -> Option<KindRef> {
            self.0
                .iter()
                .find(|k| {
                    k.ar.kind.eq_ignore_ascii_case(kind) && k.ar.group.eq_ignore_ascii_case(group)
                })
                .cloned()
        }
    }

    fn cluster() -> Table {
        Table(vec![
            kind("", "Pod", "pods", true),
            kind("", "Node", "nodes", false),
            kind("", "PersistentVolumeClaim", "persistentvolumeclaims", true),
            kind("", "PersistentVolume", "persistentvolumes", false),
            kind("", "ConfigMap", "configmaps", true),
            kind("", "Secret", "secrets", true),
            kind("apps", "StatefulSet", "statefulsets", true),
            kind("apps", "ReplicaSet", "replicasets", true),
            kind("postgresql.cnpg.io", "Cluster", "clusters", true),
            kind("cluster.x-k8s.io", "Cluster", "clusters", true),
            kind(
                "storage.k8s.io",
                "VolumeAttributesClass",
                "volumeattributesclasses",
                false,
            ),
        ])
    }

    fn obj(v: Value) -> DynamicObject {
        serde_json::from_value(v).unwrap()
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
    fn namespace_pairs_follow_the_same_array_element() {
        let pv = json!({"spec": {"claimRef": {"name": "data", "namespace": "db"}}});
        assert_eq!(
            pointer_pairs(&pv, "/spec/claimRef/name", Some("/spec/claimRef/namespace")),
            [("data".to_string(), Some("db".to_string()))]
        );
        // The second element has no namespace: it gets None, and the third
        // keeps its own — nothing shifts.
        let multi = json!({"spec": {"refs": [
            {"name": "a", "namespace": "one"},
            {"name": "b"},
            {"name": "c", "namespace": "three"},
        ]}});
        assert_eq!(
            pointer_pairs(&multi, "/spec/refs/*/name", Some("/spec/refs/*/namespace")),
            [
                ("a".to_string(), Some("one".to_string())),
                ("b".to_string(), None),
                ("c".to_string(), Some("three".to_string())),
            ]
        );
        // A namespace path with fewer `*`s than the name path reads a fixed
        // field; with more, it can't be paired and yields None.
        assert_eq!(
            pointer_pairs(&multi, "/spec/refs/*/name", Some("/spec/refs/0/namespace")),
            [
                ("a".to_string(), Some("one".to_string())),
                ("b".to_string(), Some("one".to_string())),
                ("c".to_string(), Some("one".to_string())),
            ]
        );
        assert_eq!(
            pointer_pairs(&pv, "/spec/claimRef/name", Some("/spec/other/*/namespace")),
            [("data".to_string(), None)]
        );
    }

    #[test]
    fn builtin_rules_are_keyed_like_views() {
        let views = HashMap::new();
        let pods = rules_for(&views, &ar("", "Pod", "pods"), None);
        assert!(
            pods.iter()
                .any(|r| r.kind == "nodes" && r.relation == "runs on")
        );
        assert!(pods.iter().any(|r| r.kind == "persistentvolumeclaims"));
        // A same-named plural in another group gets nothing: PodMetrics
        // shares `pods` but has no spec to point into.
        assert!(rules_for(&views, &ar("metrics.k8s.io", "PodMetrics", "pods"), None).is_empty());
        let pv = rules_for(
            &views,
            &ar("", "PersistentVolume", "persistentvolumes"),
            None,
        );
        let claim = pv
            .iter()
            .find(|r| r.kind == "persistentvolumeclaims")
            .unwrap();
        assert_eq!(
            claim.namespace_path.as_deref(),
            Some("/spec/claimRef/namespace")
        );
        assert_eq!(
            children_for(&views, &ar("apps", "Deployment", "deployments"), None),
            ["replicasets"]
        );
        assert!(children_for(&views, &ar("", "Secret", "secrets"), None).is_empty());
    }

    #[test]
    fn configured_refs_and_children_add_to_the_builtin_rows() {
        let (views, warnings) = crate::views::compile(
            &toml::from_str::<crate::config::Config>(
                r#"
                [views."karpenter.sh/v1/nodeclaims"]
                children = ["nodes", "ec2nodeclasses"]

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
        let rules = rules_for(&views, &claims, None);
        // The built-in node row and the configured class row both apply.
        assert!(rules.iter().any(|r| r.kind == "nodes"));
        let class = rules.iter().find(|r| r.kind == "ec2nodeclasses").unwrap();
        assert_eq!(
            (class.relation.as_str(), class.reverse),
            ("shaped by", Reverse::Cluster)
        );
        // Built-in `nodes` and the configured copy of it collapse to one.
        assert_eq!(
            children_for(&views, &claims, None),
            ["nodes", "ec2nodeclasses"]
        );

        let pods = rules_for(&views, &ar("", "Pod", "pods"), None);
        let team = pods.iter().find(|r| r.kind == "teams").unwrap();
        assert_eq!(
            (team.relation.as_str(), team.reverse),
            ("references", Reverse::Namespace)
        );
        assert!(all_rules(&views).iter().any(|r| r.kind == "teams"));
    }

    #[test]
    fn namespaced_view_keys_apply_to_that_namespace_only() {
        let (views, warnings) = crate::views::compile(
            &toml::from_str::<crate::config::Config>(
                r#"
                [[views."pods@prod".refs]]
                path = "/metadata/annotations/example.com~1owner"
                kind = "teams"
                reverse = "cluster"
                "#,
            )
            .unwrap()
            .views,
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        let pods = ar("", "Pod", "pods");
        assert!(
            rules_for(&views, &pods, Some("prod"))
                .iter()
                .any(|r| r.kind == "teams")
        );
        assert!(
            rules_for(&views, &pods, Some("dev"))
                .iter()
                .all(|r| r.kind != "teams")
        );
        assert!(
            rules_for(&views, &pods, None)
                .iter()
                .all(|r| r.kind != "teams")
        );

        // Read backwards from a team, the rule's pods are listed in prod
        // whatever `reverse` says: that's the only namespace it describes.
        let mut kinds = cluster();
        kinds.0.push(kind("example.com", "Team", "teams", false));
        let team = obj(
            json!({"apiVersion": "example.com/v1", "kind": "Team", "metadata": {"name": "core"}}),
        );
        let plan = self::plan(
            &views,
            &kinds,
            &kind("example.com", "Team", "teams", false),
            &team,
            "dev",
        );
        let pods_rule = plan
            .backward
            .iter()
            .find(|b| b.from.plural == "pods")
            .unwrap();
        assert_eq!(pods_rule.scope, "prod");
    }

    #[test]
    fn view_keys_resolve_to_their_group() {
        let kinds = cluster();
        assert!(resolve_view_key(&kinds, "pods").is_some());
        assert!(resolve_view_key(&kinds, "v1/pods").is_some());
        assert!(resolve_view_key(&kinds, "apps/replicasets").is_some());
        assert!(resolve_view_key(&kinds, "apps/v1/replicasets").is_some());
        // A key naming another group must not resolve to the core kind.
        assert!(resolve_view_key(&kinds, "metrics.k8s.io/pods").is_none());
        assert!(resolve_view_key(&kinds, "nope").is_none());
        // A namespace suffix is not part of the plural.
        assert!(resolve_view_key(&kinds, "v1/pods@prod").is_some());
    }

    #[test]
    fn plan_for_a_pod_reads_owner_children_and_references() {
        let views = HashMap::new();
        let kinds = cluster();
        let pod = obj(json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": "db-0", "namespace": "db", "uid": "p-1",
                         "ownerReferences": [
                             // The owner's group picks the right `Cluster` of two.
                             {"apiVersion": "postgresql.cnpg.io/v1", "kind": "Cluster", "name": "db", "uid": "c-1"},
                             {"apiVersion": "unknown.io/v1", "kind": "Widget", "name": "w", "uid": "w-1"}]},
            "spec": {"nodeName": "ip-10-0-1-2",
                     "volumes": [{"name": "data", "persistentVolumeClaim": {"claimName": "data-db-0"}},
                                 {"name": "cfg", "configMap": {"name": "db-config"}},
                                 {"name": "proj", "projected": {"sources": [{"configMap": {"name": "db-config"}}]}}]}
        }));
        let plan = self::plan(&views, &kinds, &kind("", "Pod", "pods", true), &pod, "db");

        assert_eq!(plan.owners.len(), 1);
        assert_eq!(plan.owners[0].0.ar.group, "postgresql.cnpg.io");
        assert_eq!(plan.owners[0].1, "db");
        assert!(
            plan.warn
                .as_deref()
                .unwrap()
                .contains("Widget (unknown.io/v1)"),
            "{:?}",
            plan.warn
        );
        // Pods own nothing.
        assert!(plan.children.is_empty());

        let named: Vec<(&str, &str, &str)> = plan
            .forward
            .iter()
            .flat_map(|f| {
                f.refs
                    .iter()
                    .map(move |(n, ns)| (f.rule.relation.as_str(), n.as_str(), ns.as_str()))
            })
            .collect();
        assert!(named.contains(&("runs on", "ip-10-0-1-2", "db")));
        assert!(named.contains(&("mounts", "data-db-0", "db")));
        // The volume and the projected volume are two rules naming the same
        // ConfigMap; the gather dedups the rows they produce.
        assert_eq!(
            named.iter().filter(|(_, n, _)| *n == "db-config").count(),
            2
        );
        // Kinds not served (serviceaccounts) and empty pointers add nothing.
        assert!(named.iter().all(|(rel, _, _)| *rel != "runs as"));

        // Backwards: nothing built in names a pod.
        assert!(plan.backward.is_empty());
    }

    #[test]
    fn plan_for_a_claim_scopes_usages_by_the_rule() {
        let views = HashMap::new();
        let kinds = cluster();
        let pvc = obj(json!({
            "apiVersion": "v1", "kind": "PersistentVolumeClaim",
            "metadata": {"name": "data-db-0", "namespace": "db", "uid": "v-1"},
            "spec": {"volumeName": "pvc-1", "volumeAttributesClassName": "gold"}
        }));
        let source = kind("", "PersistentVolumeClaim", "persistentvolumeclaims", true);
        let plan = self::plan(&views, &kinds, &source, &pvc, "db");
        // Pods mount claims: listed in the claim's namespace.
        let pods = plan
            .backward
            .iter()
            .find(|b| b.from.plural == "pods")
            .unwrap();
        assert_eq!(
            (pods.rule.relation.as_str(), pods.scope.as_str()),
            ("mounts", "db")
        );
        // PV → PVC is `reverse = none`: not scanned.
        assert!(
            plan.backward
                .iter()
                .all(|b| b.from.plural != "persistentvolumes")
        );
        // The class is cluster-scoped: no namespace on the GET.
        let vac = plan
            .forward
            .iter()
            .find(|f| f.target.plural == "volumeattributesclasses")
            .unwrap();
        assert!(!vac.target.namespaced);

        // A cluster-scoped row takes the table's namespace for usages.
        let vac_obj = obj(
            json!({"apiVersion": "storage.k8s.io/v1", "kind": "VolumeAttributesClass",
                                 "metadata": {"name": "gold"}}),
        );
        let source = kind(
            "storage.k8s.io",
            "VolumeAttributesClass",
            "volumeattributesclasses",
            false,
        );
        let plan = self::plan(&views, &kinds, &source, &vac_obj, "shop");
        let claims = plan
            .backward
            .iter()
            .find(|b| b.from.plural == "persistentvolumeclaims")
            .unwrap();
        assert_eq!(claims.scope, "shop");
    }

    #[test]
    fn matching_gathered_objects() {
        let pod = obj(json!({"apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": "db-0", "namespace": "db",
                         "ownerReferences": [{"apiVersion": "apps/v1", "kind": "StatefulSet", "name": "db", "uid": "s-1"}]},
            "spec": {"volumes": [{"persistentVolumeClaim": {"claimName": "data-db-0"}}]}}));
        assert!(owned_by(&pod, Some("s-1")));
        assert!(!owned_by(&pod, Some("s-2")));
        assert!(!owned_by(&pod, None));

        let mounts = rules_for(&HashMap::new(), &ar("", "Pod", "pods"), None)
            .into_iter()
            .find(|r| r.path.contains("persistentVolumeClaim"))
            .unwrap();
        // Same name in the same namespace: a match; same name elsewhere: not.
        assert!(names_source(&pod, &mounts, "data-db-0", Some("db")));
        assert!(!names_source(&pod, &mounts, "data-db-0", Some("other")));
        assert!(!names_source(&pod, &mounts, "wal-db-0", Some("db")));
        // A cluster-scoped selection has no namespace to match.
        assert!(names_source(&pod, &mounts, "data-db-0", None));

        // With a namespace path, the pointed-at namespace decides, not the
        // referencing object's own.
        let pv = obj(json!({"apiVersion": "v1", "kind": "PersistentVolume",
            "metadata": {"name": "pvc-1"},
            "spec": {"claimRef": {"name": "data-db-0", "namespace": "db"}}}));
        let bound = rules_for(
            &HashMap::new(),
            &ar("", "PersistentVolume", "persistentvolumes"),
            None,
        )
        .into_iter()
        .find(|r| r.namespace_path.is_some())
        .unwrap();
        assert!(names_source(&pv, &bound, "data-db-0", Some("db")));
        assert!(!names_source(&pv, &bound, "data-db-0", Some("other")));
    }

    #[test]
    fn dedup_drops_repeats_wherever_they_sit() {
        let item = |relation: &str, name: &str| AdjacentItem {
            direction: Direction::Names,
            relation: relation.into(),
            kind: "ConfigMap".into(),
            plural: "configmaps".into(),
            namespace: Some("db".into()),
            name: name.into(),
            object: Box::new(obj(json!({"metadata": {"name": name}}))),
        };
        let mut items = vec![
            item("mounts", "a"),
            item("mounts", "b"),
            item("mounts", "a"),
            item("reads", "a"),
        ];
        dedup(&mut items);
        let left: Vec<(&str, &str)> = items
            .iter()
            .map(|i| (i.relation.as_str(), i.name.as_str()))
            .collect();
        assert_eq!(left, [("mounts", "a"), ("mounts", "b"), ("reads", "a")]);
    }
}
