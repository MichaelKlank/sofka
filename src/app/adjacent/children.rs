use super::*;
use futures_util::{StreamExt, stream::FuturesUnordered};
use kube::api::ListParams;
use std::collections::VecDeque;
use tokio::time::{Duration, Instant, timeout, timeout_at};

#[derive(Clone, Copy)]
struct Limits {
    concurrency: usize,
    page_size: u32,
    requests: usize,
    objects: usize,
    request_time: Duration,
    total_time: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            concurrency: 4,
            page_size: 200,
            requests: 200,
            objects: 20_000,
            request_time: Duration::from_secs(5),
            total_time: Duration::from_secs(30),
        }
    }
}

impl App {
    pub fn can_discover_children(&self) -> bool {
        self.kind
            .as_ref()
            .is_some_and(|k| k.namespaced && k.is_custom())
            && self.adjacent_source.as_ref().is_some_and(|o| {
                o.metadata
                    .namespace
                    .as_ref()
                    .is_some_and(|ns| !ns.is_empty())
                    && o.metadata.uid.as_ref().is_some_and(|uid| !uid.is_empty())
            })
    }

    pub(in crate::app) fn discover_children(&mut self) {
        if !self.can_discover_children() {
            self.flash_warn("child discovery requires a namespaced custom resource with a UID");
            return;
        }
        if self.adjacent_pending() {
            self.flash_warn("wait for the initial adjacent lookup to finish");
            return;
        }
        self.cancel_children();
        let source = self.adjacent_source.as_ref().unwrap();
        let search = Search {
            client: self.cluster.client.clone(),
            kinds: self.cluster.child_kinds.clone(),
            namespace: source.metadata.namespace.clone().unwrap(),
            uid: source.metadata.uid.clone().unwrap(),
            discovery_warnings: self.cluster.discovery_warnings.clone(),
            limits: Limits::default(),
        };
        self.child_status = "children: searching".into();
        self.child_task = Some(tokio::spawn(search.run(
            self.tx.clone(),
            self.generation,
            self.child_request,
        )));
    }

    pub(in crate::app) fn cancel_children(&mut self) {
        self.child_request = self.child_request.wrapping_add(1);
        if let Some(task) = self.child_task.take() {
            task.abort();
            self.child_status = "children: incomplete (search cancelled)".into();
        }
    }
}

struct Search {
    client: Client,
    kinds: Vec<crate::k8s::Kind>,
    namespace: String,
    uid: String,
    discovery_warnings: Vec<String>,
    limits: Limits,
}

impl Search {
    async fn run(self, tx: tokio::sync::mpsc::Sender<Msg>, generation: u64, request: u64) {
        let deadline = Instant::now() + self.limits.total_time;
        let mut queue: VecDeque<_> = self
            .kinds
            .into_iter()
            .map(|k| (k, None::<String>))
            .collect();
        let total = queue.len();
        let mut pending = FuturesUnordered::new();
        let mut requests = 0;
        let mut objects = 0;
        let mut searched = 0;
        let mut failures = self.discovery_warnings.len();
        let mut reason = self.discovery_warnings.first().cloned();
        let mut limited = false;
        loop {
            if Instant::now() >= deadline {
                limited = true;
                reason = Some("time limit reached".into());
                break;
            }
            while pending.len() < self.limits.concurrency && requests < self.limits.requests {
                let Some((kind, token)) = queue.pop_front() else {
                    break;
                };
                requests += 1;
                let client = self.client.clone();
                let ns = self.namespace.clone();
                let limits = self.limits;
                pending.push(async move {
                    let api: Api<DynamicObject> = Api::namespaced_with(client, &ns, &kind.ar);
                    let mut params = ListParams::default().limit(limits.page_size);
                    if let Some(token) = token {
                        params = params.continue_token(&token);
                    }
                    let result = match timeout(limits.request_time, api.list(&params)).await {
                        Ok(result) => result.map_err(|e| e.to_string()),
                        Err(_) => Err("request timed out".into()),
                    };
                    (kind, result)
                });
            }
            if pending.is_empty() {
                if !queue.is_empty() {
                    limited = true;
                    reason = Some("request limit reached".into());
                }
                break;
            }
            let (kind, result) = match timeout_at(deadline, pending.next()).await {
                Ok(Some(result)) => result,
                Ok(None) => break,
                Err(_) => {
                    limited = true;
                    reason = Some("time limit reached".into());
                    break;
                }
            };
            let mut items = Vec::new();
            match result {
                Ok(page) => {
                    let available = self.limits.objects.saturating_sub(objects);
                    let overflow = page.items.len() > available;
                    let r = kind_ref(kind.clone());
                    for object in page.items.into_iter().take(available) {
                        objects += 1;
                        if object.metadata.namespace.as_deref() == Some(self.namespace.as_str())
                            && owned_by(&object, Some(&self.uid))
                        {
                            items.push(item(Direction::Child, "owns", &r, object));
                        }
                    }
                    if let Some(token) = page.metadata.continue_.filter(|t| !t.is_empty()) {
                        queue.push_back((kind, Some(token)));
                    } else if !overflow {
                        searched += 1;
                    }
                    if overflow
                        || (objects >= self.limits.objects
                            && (!queue.is_empty() || !pending.is_empty()))
                    {
                        limited = true;
                        reason = Some("object limit reached".into());
                    }
                }
                Err(error) => {
                    failures += 1;
                    reason.get_or_insert_with(|| format!("{}: {error}", kind.title()));
                }
            }
            if tx
                .send(Msg::AdjacentChildren {
                    generation,
                    request,
                    items,
                    status: format!(
                        "children: searching ({searched}/{total} kinds, {objects} objects checked)"
                    ),
                    done: false,
                })
                .await
                .is_err()
            {
                return;
            }
            if limited {
                break;
            }
        }
        // Dropping page futures cancels all outstanding HTTP requests.
        drop(pending);
        let status = if failures > 0 || limited {
            format!(
                "children: incomplete ({searched}/{total} kinds; {failures} errors; {})",
                reason.unwrap_or_else(|| "search limit reached".into())
            )
        } else {
            format!("children: complete ({searched}/{total} kinds, {objects} objects checked)")
        };
        let _ = tx
            .send(Msg::AdjacentChildren {
                generation,
                request,
                items: Vec::new(),
                status,
                done: true,
            })
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    fn fixture(delay: Duration, repeat: bool, denied: bool) -> (Search, Arc<AtomicUsize>) {
        let requests = Arc::new(AtomicUsize::new(0));
        let seen = requests.clone();
        let client = Client::new(
            tower::service_fn(move |req: http::Request<kube::client::Body>| {
                seen.fetch_add(1, Ordering::SeqCst);
                assert!(req.uri().path().contains("/namespaces/prod/"));
                assert!(req.uri().query().unwrap().contains("limit=200"));
                let second = req.uri().query().unwrap().contains("continue=");
                async move {
                    tokio::time::sleep(delay).await;
                    let (code, body) = if denied && second {
                        (
                            403,
                            json!({"kind":"Status", "apiVersion":"v1", "status":"Failure", "reason":"Forbidden", "message":"access denied", "code":403}),
                        )
                    } else {
                        (
                            200,
                            json!({"apiVersion":"v1", "kind":"PodList",
                            "metadata":{"continue": if repeat || !second {"next"} else {""}},
                            "items":[
                                {"apiVersion":"v1", "kind":"Pod", "metadata":{"name":if second {"second"} else {"first"}, "namespace":"prod", "ownerReferences":[{"apiVersion":"example.io/v1", "kind":"Widget", "name":"parent", "uid":"parent-uid"}]}},
                                {"apiVersion":"v1", "kind":"Pod", "metadata":{"name":"old-owner", "namespace":"prod", "ownerReferences":[{"apiVersion":"example.io/v1", "kind":"Widget", "name":"parent", "uid":"old-uid"}]}}
                            ]}),
                        )
                    };
                    Ok::<_, std::convert::Infallible>(
                        http::Response::builder()
                            .status(code)
                            .body(http_body_util::Full::new(hyper::body::Bytes::from(
                                body.to_string(),
                            )))
                            .unwrap(),
                    )
                }
            }),
            "default",
        );
        let search = Search {
            client,
            kinds: vec![crate::k8s::Cluster::fake().resolve("pods").unwrap()],
            namespace: "prod".into(),
            uid: "parent-uid".into(),
            discovery_warnings: Vec::new(),
            limits: Limits::default(),
        };
        (search, requests)
    }

    async fn results(search: Search) -> (Vec<AdjacentItem>, String) {
        let (tx, mut rx) = tokio::sync::mpsc::channel(256);
        search.run(tx, 1, 2).await;
        let mut found = Vec::new();
        let mut status = String::new();
        while let Some(Msg::AdjacentChildren {
            items,
            status: update,
            ..
        }) = rx.recv().await
        {
            found.extend(items);
            status = update;
        }
        (found, status)
    }

    #[tokio::test]
    async fn pagination_matches_only_the_current_owner_uid() {
        let (search, calls) = fixture(Duration::ZERO, false, false);
        let (items, status) = results(search).await;
        assert_eq!(
            items.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(),
            ["first", "second"]
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(status.contains("complete (1/1"), "{status}");
    }

    #[tokio::test]
    async fn forbidden_page_keeps_previous_results() {
        let (search, _) = fixture(Duration::ZERO, false, true);
        let (items, status) = results(search).await;
        assert_eq!(items.len(), 1);
        assert!(status.contains("incomplete"), "{status}");
        assert!(status.contains("access denied"), "{status}");
    }

    #[tokio::test]
    async fn request_limit_stops_repeated_continuation_tokens() {
        let (mut search, calls) = fixture(Duration::ZERO, true, false);
        search.limits.requests = 3;
        let (items, status) = results(search).await;
        assert_eq!(items.len(), 3);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert!(
            status.contains("incomplete") && status.contains("request limit"),
            "{status}"
        );
    }

    #[tokio::test]
    async fn object_limit_counts_objects_with_other_owners() {
        let (mut search, calls) = fixture(Duration::ZERO, true, false);
        search.limits.objects = 2;
        let (items, status) = results(search).await;
        assert_eq!(items.len(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(
            status.contains("incomplete") && status.contains("object limit"),
            "{status}"
        );
    }

    #[tokio::test]
    async fn oversized_page_is_incomplete() {
        let (mut search, _) = fixture(Duration::ZERO, false, false);
        search.limits.objects = 1;
        let (items, status) = results(search).await;
        assert_eq!(items.len(), 1);
        assert!(status.contains("object limit"), "{status}");
    }

    #[tokio::test]
    async fn request_timeout_and_total_timeout_are_incomplete() {
        for total in [false, true] {
            let (mut search, _) = fixture(Duration::from_secs(1), false, false);
            if total {
                search.limits.total_time = Duration::from_millis(10);
            } else {
                search.limits.request_time = Duration::from_millis(10);
            }
            let (items, status) = results(search).await;
            assert!(items.is_empty());
            assert!(status.contains("incomplete"), "{status}");
            assert!(
                status.contains(if total { "time limit" } else { "timed out" }),
                "{status}"
            );
        }
    }

    #[tokio::test]
    async fn discovery_failure_cannot_produce_a_complete_empty_result() {
        let (mut search, _) = fixture(Duration::ZERO, false, false);
        search.kinds.clear();
        search
            .discovery_warnings
            .push("API discovery access denied".into());
        let (items, status) = results(search).await;
        assert!(items.is_empty());
        assert!(status.contains("incomplete") && status.contains("API discovery access denied"));
    }

    #[tokio::test]
    async fn concurrency_is_bounded_and_aborting_stops_new_requests() {
        let (mut search, calls) = fixture(Duration::from_secs(60), false, false);
        search.kinds = vec![search.kinds[0].clone(); 10];
        let (tx, _rx) = tokio::sync::mpsc::channel(256);
        let task = tokio::spawn(search.run(tx, 1, 2));
        tokio::time::timeout(Duration::from_secs(1), async {
            while calls.load(Ordering::SeqCst) < 4 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }
}
