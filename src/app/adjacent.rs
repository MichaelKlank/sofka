use super::*;

use crate::adjacent::{RefRule, Reverse, all_rules, children_for, pointer_values, rules_for};
use crate::store::AdjacentItem;

/// A kind resolved on the UI thread, ready to move into the gather task —
/// the registry isn't available there.
#[derive(Clone)]
pub(super) struct Resolved {
    ar: ApiResource,
    namespaced: bool,
    plural: String,
}

/// A rule read forwards from the selection: the objects it names, as
/// (name, namespace) pairs to read.
struct Forward {
    rule: RefRule,
    target: Resolved,
    refs: Vec<(String, String)>,
}

/// A rule read backwards: list `from` in `scope` and keep the objects whose
/// pointer names the selection.
struct Backward {
    rule: RefRule,
    from: Resolved,
    scope: String,
}

impl App {
    /// `u` / `:adjacent` — list the objects directly connected to the
    /// selection: its owners, the objects it owns, the objects its spec names,
    /// and the objects whose specs name it. Gathered off-thread; arrives as
    /// [`Msg::Adjacent`]. `⏎` lands on one in its regular view.
    pub(super) fn open_adjacent(&mut self) {
        if self.kind.is_none() {
            self.flash_warn("select a resource first");
            return;
        }
        // Helm rows are synthetic (backed by storage Secrets): nothing in
        // the API references them, and their references are the chart's.
        if matches!(self.kind_plural.as_str(), "helm" | "helmhistory") {
            self.flash_warn("adjacent is not available for Helm releases");
            return;
        }
        let Some(obj) = self.selected() else {
            self.flash_warn("no selection");
            return;
        };
        self.set_return_mode();
        let name = obj.metadata.name.clone().unwrap_or_default();
        self.adjacent_title = format!("{name} — adjacent");
        self.adjacent_items.clear();
        self.adjacent_state.select(None);
        self.adjacent_source = Some(obj);
        self.mode = Mode::Adjacent;
        self.spawn_adjacent();
    }

    /// `r` in the adjacent view — gather again for the same object.
    pub(super) fn refresh_adjacent(&mut self) {
        if self.adjacent_source.is_some() {
            self.spawn_adjacent();
        }
    }

    /// Whether a gather is still running (the view says "gathering…").
    pub fn adjacent_pending(&self) -> bool {
        self.adjacent_claim.is_some()
    }

    fn resolved(&self, kind: &str) -> Option<Resolved> {
        self.cluster.resolve(kind).map(|k| Resolved {
            plural: k.ar.plural.to_lowercase(),
            ar: k.ar,
            namespaced: k.namespaced,
        })
    }

    /// Resolve a `[views."…"]`-style key (`v1/pods`, `apps/deployments`,
    /// `pods`) to a kind, holding it to the group or apiVersion the key names.
    pub(super) fn resolve_view_key(&self, key: &str) -> Option<Resolved> {
        let resolved = self.resolved(crate::views::key_plural(key))?;
        let Some((prefix, _)) = key.rsplit_once('/') else {
            return Some(resolved);
        };
        let prefix = prefix.to_lowercase();
        let ar = &resolved.ar;
        (prefix == ar.api_version.to_lowercase() || prefix == ar.group.to_lowercase())
            .then_some(resolved)
    }

    fn spawn_adjacent(&mut self) {
        let Some(obj) = self.adjacent_source.clone() else {
            return;
        };
        let Some(kind) = self.kind.clone() else {
            return;
        };
        let source = Resolved {
            plural: self.kind_plural.clone(),
            ar: kind.ar.clone(),
            namespaced: kind.namespaced,
        };
        let ns = obj.metadata.namespace.clone().unwrap_or_default();
        // A cluster-scoped row has no namespace of its own: its usages are
        // looked up where the table is looking.
        let scope_ns = if ns.is_empty() {
            self.namespace.clone()
        } else {
            ns.clone()
        };
        let mut warn: Option<String> = None;

        let owners: Vec<(Resolved, String)> = obj
            .metadata
            .owner_references
            .iter()
            .flatten()
            .filter_map(|o| match self.resolved(&o.kind.to_lowercase()) {
                Some(r) => Some((r, o.name.clone())),
                None => {
                    warn.get_or_insert(format!("owner kind {} is not served here", o.kind));
                    None
                }
            })
            .collect();
        let children: Vec<Resolved> = children_for(&self.user_views, &kind.ar)
            .iter()
            .filter_map(|p| self.resolved(p))
            .collect();
        let value = serde_json::to_value(&obj).unwrap_or(Value::Null);
        // A rule whose target kind isn't served here — a class from a CSI
        // driver that isn't installed — simply contributes nothing.
        let forward: Vec<Forward> = rules_for(&self.user_views, &kind.ar)
            .into_iter()
            .filter_map(|rule| {
                let target = self.resolved(&rule.kind)?;
                let names = pointer_values(&value, &rule.path);
                if names.is_empty() {
                    return None;
                }
                let namespaces = rule
                    .namespace_path
                    .as_deref()
                    .map(|p| pointer_values(&value, p))
                    .unwrap_or_default();
                let refs = names
                    .into_iter()
                    .enumerate()
                    .map(|(i, name)| {
                        let target_ns = namespaces.get(i).cloned().unwrap_or_else(|| ns.clone());
                        (name, target_ns)
                    })
                    .collect();
                Some(Forward { rule, target, refs })
            })
            .collect();
        let backward: Vec<Backward> = all_rules(&self.user_views)
            .into_iter()
            .filter(|rule| rule.reverse != Reverse::None)
            .filter_map(|rule| {
                let target = self.resolved(&rule.kind)?;
                if target.ar.plural != source.ar.plural || target.ar.group != source.ar.group {
                    return None;
                }
                let from = self.resolve_view_key(&rule.from)?;
                let scope = match rule.reverse {
                    Reverse::Namespace if from.namespaced => scope_ns.clone(),
                    _ => String::new(),
                };
                Some(Backward { rule, from, scope })
            })
            .collect();

        let client = self.cluster.client.clone();
        let tx = self.tx.clone();
        let genr = self.generation;
        let title = self.adjacent_title.clone();
        let claim = self.claim_status(format!(
            "gathering what's adjacent to {}…",
            obj.metadata.name.clone().unwrap_or_default()
        ));
        self.adjacent_claim = Some(claim);
        self.adjacent_request = self.adjacent_request.wrapping_add(1);
        let request = self.adjacent_request;

        tokio::spawn(async move {
            let mut items: Vec<AdjacentItem> = Vec::new();
            let source_uid = obj.metadata.uid.clone();
            let source_name = obj.metadata.name.clone().unwrap_or_default();

            for (r, name) in owners {
                let owner_ns = if r.namespaced { ns.as_str() } else { "" };
                if let Some(o) = get_or_warn(&client, &r, owner_ns, &name, &mut warn).await {
                    items.push(item("↑ owned by", &r, o));
                }
            }
            for r in children {
                let scope = if r.namespaced { ns.as_str() } else { "" };
                for o in list_or_warn(&client, &r.ar, r.namespaced, scope, &mut warn).await {
                    let owned = source_uid.is_some()
                        && o.metadata
                            .owner_references
                            .iter()
                            .flatten()
                            .any(|own| Some(&own.uid) == source_uid.as_ref());
                    if owned {
                        items.push(item("↓ owns", &r, o));
                    }
                }
            }
            for Forward {
                rule,
                target: r,
                refs,
            } in forward
            {
                for (name, target_ns) in refs {
                    let scope = if r.namespaced { target_ns.as_str() } else { "" };
                    if let Some(o) = get_or_warn(&client, &r, scope, &name, &mut warn).await {
                        items.push(item(&format!("→ {}", rule.relation), &r, o));
                    }
                }
            }
            for Backward {
                rule,
                from: r,
                scope,
            } in backward
            {
                for o in list_or_warn(&client, &r.ar, r.namespaced, &scope, &mut warn).await {
                    let v = serde_json::to_value(&o).unwrap_or(Value::Null);
                    if !pointer_values(&v, &rule.path).contains(&source_name) {
                        continue;
                    }
                    // A namespaced target is named within a namespace: the
                    // rule's namespace path when it has one, else the
                    // referencing object's own.
                    if source.namespaced {
                        let named_ns = match &rule.namespace_path {
                            Some(p) => pointer_values(&v, p).into_iter().next(),
                            None => o.metadata.namespace.clone(),
                        };
                        if named_ns.as_deref() != Some(ns.as_str()) {
                            continue;
                        }
                    }
                    items.push(item(&format!("← {}", rule.relation), &r, o));
                }
            }
            items.dedup_by(|a, b| {
                a.relation == b.relation
                    && a.plural == b.plural
                    && a.namespace == b.namespace
                    && a.name == b.name
            });
            let _ = tx
                .send(Msg::Adjacent {
                    generation: genr,
                    request,
                    claim,
                    title,
                    items,
                    warn,
                })
                .await;
        });
    }

    pub(super) fn cancel_adjacent_request(&mut self) {
        self.adjacent_request = self.adjacent_request.wrapping_add(1);
        if let Some(claim) = self.adjacent_claim.take() {
            self.clear_claimed_status(claim);
        }
    }

    pub(super) fn key_adjacent(&mut self, key: KeyEvent) {
        let len = self.adjacent_items.len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.mode = Mode::Table;
                self.restore_selection();
            }
            KeyCode::Char('j') | KeyCode::Down => list_step(&mut self.adjacent_state, len, true),
            KeyCode::Char('k') | KeyCode::Up => list_step(&mut self.adjacent_state, len, false),
            KeyCode::Char('g') | KeyCode::Home => {
                if len > 0 {
                    self.adjacent_state.select(Some(0));
                }
            }
            KeyCode::Char('G') | KeyCode::End => {
                if len > 0 {
                    self.adjacent_state.select(Some(len - 1));
                }
            }
            KeyCode::Char('r') => self.refresh_adjacent(),
            KeyCode::Enter => self.adjacent_goto(),
            KeyCode::Char('y') => self.adjacent_yaml(),
            KeyCode::Char('d') => self.adjacent_describe(),
            _ => {}
        }
        if !matches!(self.mode, Mode::Adjacent | Mode::Detail) {
            self.cancel_adjacent_request();
        }
    }

    fn adjacent_selected(&self) -> Option<AdjacentItem> {
        self.adjacent_state
            .selected()
            .and_then(|i| self.adjacent_items.get(i))
            .cloned()
    }

    /// ⏎ — land on the object in its own kind's view, name-filtered, where
    /// every action applies to it.
    fn adjacent_goto(&mut self) {
        let Some(it) = self.adjacent_selected() else {
            self.flash_warn("no object selected");
            return;
        };
        self.navigate_to_target(&crate::explain::Target {
            plural: it.plural,
            namespace: it.namespace,
            name: it.name,
        });
    }

    /// `y` — the row's YAML, already fetched by the gather; `esc` returns here.
    fn adjacent_yaml(&mut self) {
        let Some(it) = self.adjacent_selected() else {
            self.flash_warn("no object selected");
            return;
        };
        self.set_return_mode();
        self.show_yaml(&it.object);
    }

    /// `d` — `kubectl describe` the row; `esc` returns here.
    fn adjacent_describe(&mut self) {
        let Some(it) = self.adjacent_selected() else {
            self.flash_warn("no object selected");
            return;
        };
        self.set_return_mode();
        self.describe_object(it.plural, &it.object);
    }
}

fn item(relation: &str, r: &Resolved, o: DynamicObject) -> AdjacentItem {
    AdjacentItem {
        relation: relation.to_string(),
        kind: r.ar.kind.clone(),
        plural: r.plural.clone(),
        namespace: o.metadata.namespace.clone(),
        name: o.metadata.name.clone().unwrap_or_default(),
        object: Box::new(o),
    }
}

/// Read one object by name. A 404 is "not there" (a reference to something
/// gone), any other failure is recorded in `warn`.
async fn get_or_warn(
    client: &Client,
    r: &Resolved,
    ns: &str,
    name: &str,
    warn: &mut Option<String>,
) -> Option<DynamicObject> {
    let api: Api<DynamicObject> = if r.namespaced && !ns.is_empty() {
        Api::namespaced_with(client.clone(), ns, &r.ar)
    } else {
        Api::all_with(client.clone(), &r.ar)
    };
    match api.get(name).await {
        Ok(o) => Some(o),
        Err(kube::Error::Api(ae)) if ae.code == 404 => None,
        Err(e) => {
            warn.get_or_insert(format!("reading {}/{name}: {e}", r.plural));
            None
        }
    }
}
