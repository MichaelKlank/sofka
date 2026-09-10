use super::*;
use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};

use http_body_util::{BodyExt, Full, StreamBody, combinators::BoxBody};
use hyper::body::{Bytes, Frame};
use serde_json::json;
use tokio::sync::mpsc::{self, Receiver};

enum Reply {
    Json(u16, Value),
    Events(String),
    Pending,
}

struct Seen {
    uri: http::Uri,
    accept: String,
    time: Instant,
}

fn table(name: &str, version: &str, value: Value) -> Value {
    json!({
        "kind": "Table", "apiVersion": "meta.k8s.io/v1",
        "metadata": {"resourceVersion": "10"},
        "columnDefinitions": [
            {"name": "Name", "type": "string", "format": "name"},
            {"name": "Count", "type": "integer"}
        ],
        "rows": [{"cells": [name, value], "object": {
            "apiVersion": "meta.k8s.io/v1", "kind": "PartialObjectMetadata",
            "metadata": {"name": name, "namespace": "team", "uid": name, "resourceVersion": version}
        }}]
    })
}

fn status(code: u16) -> Value {
    json!({"apiVersion": "v1", "kind": "Status", "status": "Failure",
        "reason": "TestError", "message": "test API error", "code": code})
}

fn mock(replies: Vec<Reply>) -> (Feed, Receiver<Msg>, Arc<Mutex<Vec<Seen>>>) {
    let replies = Arc::new(Mutex::new(VecDeque::from(replies)));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let requests = seen.clone();
    let client = Client::new(
        tower::service_fn(move |request: http::Request<kube::client::Body>| {
            requests.lock().unwrap().push(Seen {
                uri: request.uri().clone(),
                accept: request
                    .headers()
                    .get(http::header::ACCEPT)
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .into(),
                time: Instant::now(),
            });
            let reply = replies
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected API request");
            async move {
                let (code, body): (u16, BoxBody<Bytes, Infallible>) = match reply {
                    Reply::Json(code, value) => {
                        (code, Full::new(Bytes::from(value.to_string())).boxed())
                    }
                    Reply::Events(events) => (200, Full::new(Bytes::from(events)).boxed()),
                    Reply::Pending => (
                        200,
                        BodyExt::boxed(StreamBody::new(futures_util::stream::pending::<
                            std::result::Result<Frame<Bytes>, Infallible>,
                        >())),
                    ),
                };
                Ok::<_, Infallible>(http::Response::builder().status(code).body(body).unwrap())
            }
        }),
        "default",
    );
    let (tx, rx) = mpsc::channel(32);
    (
        Feed {
            client,
            request: Request::new("/apis/example.com/v1/namespaces/team/widgets"),
            params: ListParams::default()
                .labels("app=web")
                .fields("metadata.name=one"),
            resource: GroupVersionResource::gvr("example.com", "v1", "widgets"),
            generation: 7,
            tx,
        },
        rx,
        seen,
    )
}

fn query(uri: &http::Uri) -> std::collections::HashMap<String, String> {
    form_urlencoded::parse(uri.query().unwrap_or_default().as_bytes())
        .into_owned()
        .collect()
}

#[tokio::test]
async fn negotiates_tables_and_preserves_scope_across_pages() {
    let mut first = table("one", "8", json!(2));
    first["metadata"]["continue"] = json!("next/page+");
    let mut second = table("two", "9", json!(10));
    second["columnDefinitions"] = Value::Null;
    let (feed, _rx, seen) = mock(vec![Reply::Json(200, first), Reply::Json(200, second)]);
    let result = feed.list().await.unwrap().unwrap();
    assert_eq!(result.rows.len(), 2);
    assert_eq!(result.rows[1].cells[1], 10);
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    for request in seen.iter() {
        assert_eq!(request.accept, ACCEPT);
        assert_eq!(
            request.uri.path(),
            "/apis/example.com/v1/namespaces/team/widgets"
        );
        let params = query(&request.uri);
        assert_eq!(params["includeObject"], "Metadata");
        assert_eq!(params["labelSelector"], "app=web");
        assert_eq!(params["fieldSelector"], "metadata.name=one");
        assert_eq!(params["limit"], "500");
    }
    assert_eq!(query(&seen[1].uri)["continue"], "next/page+");
}

#[tokio::test]
async fn ordinary_json_and_406_keep_the_resource_fallback() {
    let list = json!({"apiVersion": "example.com/v1", "kind": "WidgetList", "items": []});
    for rejected in [false, true] {
        let mut replies = Vec::new();
        if rejected {
            replies.push(Reply::Json(406, status(406)));
        }
        replies.push(Reply::Json(200, list.clone()));
        let (feed, mut rx, seen) = mock(replies);
        feed.run(RETRY_INTERVAL, REFRESH_INTERVAL).await;
        assert!(matches!(
            rx.recv().await,
            Some(Msg::ServerTable {
                update: Update::Unavailable,
                ..
            })
        ));
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), if rejected { 2 } else { 1 });
        if rejected {
            assert_eq!(seen[1].accept, "application/json");
            assert!(!query(&seen[1].uri).contains_key("includeObject"));
        }
    }
}

#[tokio::test]
async fn permission_and_server_errors_do_not_mean_tables_are_unsupported() {
    for code in [401, 403, 429, 500, 503] {
        let (feed, _rx, seen) = mock(vec![Reply::Json(code, status(code))]);
        let error = feed.list().await.unwrap_err();
        assert_eq!(api_code(&error), Some(code));
        assert_eq!(seen.lock().unwrap().len(), 1);
    }
    let (feed, _rx, _) = mock(vec![
        Reply::Json(406, status(406)),
        Reply::Json(403, status(403)),
    ]);
    assert_eq!(api_code(&feed.list().await.unwrap_err()), Some(403));
}

#[tokio::test]
async fn empty_and_null_rows_keep_the_column_definitions() {
    for empty in [Value::Null, json!([])] {
        let mut value = table("one", "8", Value::Null);
        value["rows"] = empty;
        let (feed, _rx, _) = mock(vec![Reply::Json(200, value)]);
        let table = feed.list().await.unwrap().unwrap();
        assert!(table.rows.is_empty());
        assert_eq!(table.columns.len(), 2);
    }
}

#[tokio::test]
async fn invalid_rows_and_schemas_return_errors() {
    let mut invalid = Vec::new();
    let mut value = table("one", "8", json!(1));
    value["rows"][0]["cells"] = json!(["one"]);
    invalid.push(value);
    let mut value = table("one", "8", json!(1));
    value["rows"][0]["object"] = Value::Null;
    invalid.push(value);
    let mut value = table("one", "8", json!(1));
    value["columnDefinitions"][1]["name"] = json!("NAME");
    invalid.push(value);
    let mut value = table("one", "8", json!(1));
    value["rows"] = json!([value["rows"][0], value["rows"][0]]);
    invalid.push(value);
    for value in invalid {
        let (feed, _rx, _) = mock(vec![Reply::Json(200, value)]);
        assert!(feed.list().await.is_err());
    }
}

#[tokio::test]
async fn inconsistent_pages_do_not_publish_partial_tables() {
    let mut first = table("one", "8", json!(1));
    first["metadata"]["continue"] = json!("next");
    let mut second = table("two", "9", json!(2));
    second["metadata"]["resourceVersion"] = json!("11");
    let (feed, _rx, _) = mock(vec![Reply::Json(200, first), Reply::Json(200, second)]);
    assert!(
        feed.list()
            .await
            .unwrap_err()
            .to_string()
            .contains("version changed")
    );
}

#[tokio::test]
async fn watches_cells_when_later_events_omit_headers() {
    let initial = table("one", "8", json!(1));
    let columns = Table::decode(initial.clone(), &[]).unwrap().columns;
    let mut updated = table("one", "9", json!(20));
    updated.as_object_mut().unwrap().remove("columnDefinitions");
    let mut deleted = table("one", "10", Value::Null);
    deleted["columnDefinitions"] = Value::Null;
    let events = [
        json!({"type": "ADDED", "object": initial}),
        json!({"type": "MODIFIED", "object": updated}),
        json!({"type": "DELETED", "object": deleted}),
        json!({"type": "ERROR", "object": status(410)}),
    ]
    .into_iter()
    .map(|v| format!("{v}\n"))
    .collect();
    let (feed, mut rx, seen) = mock(vec![Reply::Events(events)]);
    assert_eq!(
        api_code(&feed.watch("8", columns).await.unwrap_err()),
        Some(410)
    );
    assert!(matches!(
        rx.recv().await,
        Some(Msg::ServerTable {
            generation: 7,
            update: Update::Apply(_),
            ..
        })
    ));
    let Some(Msg::ServerTable {
        update: Update::Apply(updated),
        ..
    }) = rx.recv().await
    else {
        panic!("missing update")
    };
    assert_eq!(updated.rows[0].cells[1], 20);
    assert!(matches!(
        rx.recv().await,
        Some(Msg::ServerTable {
            update: Update::Delete(_),
            ..
        })
    ));
    let seen = seen.lock().unwrap();
    let params = query(&seen[0].uri);
    assert_eq!(params["watch"], "true");
    assert_eq!(params["resourceVersion"], "8");
    assert_eq!(params["labelSelector"], "app=web");
    assert_eq!(params["fieldSelector"], "metadata.name=one");
    assert_eq!(params["includeObject"], "Metadata");
}

#[tokio::test]
async fn ordinary_watch_events_select_polling() {
    let events = format!(
        "{}\n",
        json!({"type": "ADDED", "object": {
            "kind": "Widget", "apiVersion": "example.com/v1", "metadata": {"name": "one"}
        }})
    );
    let (feed, _rx, _) = mock(vec![Reply::Events(events)]);
    assert!(!feed.watch("8", Vec::new()).await.unwrap());
}

#[tokio::test]
async fn refreshes_after_watch_rejection_silence_and_expiry() {
    let retry = Duration::from_millis(30);
    let refresh = Duration::from_millis(70);
    for (reply, min_wait) in [
        (Reply::Json(406, status(406)), retry),
        (Reply::Pending, refresh),
        (
            Reply::Events(format!(
                "{}\n",
                json!({"type": "ERROR", "object": status(410)})
            )),
            retry,
        ),
    ] {
        let (feed, mut rx, seen) = mock(vec![
            Reply::Json(200, table("one", "8", json!(1))),
            reply,
            Reply::Json(200, table("one", "8", json!(2))),
            Reply::Pending,
        ]);
        let began = Instant::now();
        let task = tokio::spawn(feed.run(retry, refresh));
        timeout(Duration::from_secs(2), async {
            let mut snapshots = 0;
            while let Some(msg) = rx.recv().await {
                match msg {
                    Msg::ServerTable {
                        update: Update::Replace(table),
                        ..
                    } => {
                        snapshots += 1;
                        assert_eq!(table.rows[0].cells[1], snapshots);
                        if snapshots == 2 {
                            break;
                        }
                    }
                    Msg::ServerTableError { error, .. } => panic!("{error}"),
                    _ => panic!("unexpected Table message"),
                }
            }
            assert_eq!(snapshots, 2);
        })
        .await
        .unwrap();
        task.abort();
        let seen = seen.lock().unwrap();
        assert!(seen[2].time.duration_since(began) >= min_wait);
    }
}

#[tokio::test]
async fn retries_transient_list_failure() {
    let (feed, mut rx, seen) = mock(vec![
        Reply::Json(503, status(503)),
        Reply::Json(200, table("one", "8", json!(2))),
        Reply::Pending,
    ]);
    let task = tokio::spawn(feed.run(Duration::from_millis(30), REFRESH_INTERVAL));
    timeout(Duration::from_secs(2), async {
        assert!(matches!(
            rx.recv().await,
            Some(Msg::ServerTableError { .. })
        ));
        assert!(matches!(
            rx.recv().await,
            Some(Msg::ServerTable {
                update: Update::Replace(_),
                ..
            })
        ));
    })
    .await
    .unwrap();
    task.abort();
    assert_eq!(seen.lock().unwrap()[1].accept, ACCEPT);
}
