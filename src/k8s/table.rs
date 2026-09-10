//! Table list and watch requests for the active generic resource view.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use futures_util::StreamExt;
use kube::Client;
use kube::api::{ListParams, WatchParams};
use kube::core::{GroupVersionResource, Request, WatchEvent};
use serde_json::Value;
use tokio::sync::mpsc::Sender;
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until, timeout};

use crate::server_table::{Column, Table, Update};
use crate::store::Msg;

const ACCEPT: &str = "application/json;as=Table;g=meta.k8s.io;v=v1,application/json";
const RETRY_INTERVAL: Duration = Duration::from_secs(5);
const REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

struct Feed {
    client: Client,
    request: Request,
    params: ListParams,
    resource: GroupVersionResource,
    generation: u64,
    tx: Sender<Msg>,
}

pub(super) fn spawn(
    client: Client,
    path: String,
    params: ListParams,
    resource: GroupVersionResource,
    generation: u64,
    tx: Sender<Msg>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        Feed {
            client,
            request: Request::new(path),
            params,
            resource,
            generation,
            tx,
        }
        .run(RETRY_INTERVAL, REFRESH_INTERVAL)
        .await;
    })
}

fn table_request(mut request: http::Request<Vec<u8>>) -> Result<http::Request<Vec<u8>>> {
    // Full objects continue to arrive through the resource watch.
    let uri = format!("{}&includeObject=Metadata", request.uri());
    *request.uri_mut() = uri.parse()?;
    request
        .headers_mut()
        .insert(http::header::ACCEPT, http::HeaderValue::from_static(ACCEPT));
    Ok(request)
}

fn api_code(error: &anyhow::Error) -> Option<u16> {
    match error.downcast_ref::<kube::Error>()? {
        kube::Error::Api(status) => Some(status.code),
        _ => None,
    }
}

impl Feed {
    async fn send(&self, update: Update) -> Result<()> {
        self.tx
            .send(Msg::ServerTable {
                generation: self.generation,
                resource: self.resource.clone(),
                update,
            })
            .await
            .context("resource view closed")
    }

    async fn report(&self, error: &anyhow::Error, last: &mut Option<String>) {
        let message = format!("Table columns: {error:#}");
        if last.as_ref() != Some(&message) {
            let _ = self
                .tx
                .send(Msg::ServerTableError {
                    generation: self.generation,
                    error: message.clone(),
                })
                .await;
            *last = Some(message);
        }
    }

    async fn list_page(&self, params: &ListParams) -> Result<Value> {
        let request = table_request(self.request.list(params)?)?;
        let result = timeout(REQUEST_TIMEOUT, self.client.request(request))
            .await
            .context("Table request timed out")?;
        match result {
            Err(kube::Error::Api(status)) if status.code == 406 => {
                let mut request = self.request.list(params)?;
                request.headers_mut().insert(
                    http::header::ACCEPT,
                    http::HeaderValue::from_static("application/json"),
                );
                Ok(timeout(REQUEST_TIMEOUT, self.client.request(request))
                    .await
                    .context("JSON fallback timed out")??)
            }
            other => Ok(other?),
        }
    }

    async fn list(&self) -> Result<Option<Table>> {
        let mut params = self.params.clone();
        params.limit = Some(500);
        let mut result: Option<Table> = None;
        let mut tokens = HashSet::new();
        let mut keys = HashSet::new();
        loop {
            let value = self.list_page(&params).await?;
            if value["kind"] != "Table" {
                ensure!(
                    value["items"].is_array(),
                    "invalid JSON fallback from Table request"
                );
                ensure!(result.is_none(), "Table response changed during pagination");
                return Ok(None);
            }
            let previous = result.as_ref().map_or(&[][..], |t| t.columns.as_slice());
            let page = Table::decode(value, previous)?;
            for row in &page.rows {
                ensure!(
                    keys.insert(row.key.clone()),
                    "Table contains duplicate resource rows"
                );
            }
            let next = page.continue_token.clone();
            if let Some(table) = &mut result {
                ensure!(
                    table.columns == page.columns,
                    "Table columns changed during pagination"
                );
                ensure!(
                    table.resource_version == page.resource_version,
                    "Table version changed during pagination"
                );
                table.rows.extend(page.rows);
                table.continue_token = next.clone();
            } else {
                result = Some(page);
            }
            let Some(token) = next else { return Ok(result) };
            ensure!(
                tokens.insert(token.clone()),
                "Table pagination did not advance"
            );
            params.continue_token = Some(token);
        }
    }

    /// Return false when this endpoint sends ordinary objects instead of Tables.
    async fn watch(&self, version: &str, mut columns: Vec<Column>) -> Result<bool> {
        let params = WatchParams {
            label_selector: self.params.label_selector.clone(),
            field_selector: self.params.field_selector.clone(),
            ..WatchParams::default().timeout(30).disable_bookmarks()
        };
        let request = table_request(self.request.watch(&params, version)?)?;
        let stream = self.client.request_events::<Value>(request).await?;
        futures_util::pin_mut!(stream);
        while let Some(event) = stream.next().await {
            let (value, delete) = match event? {
                WatchEvent::Added(value) | WatchEvent::Modified(value) => (value, false),
                WatchEvent::Deleted(value) => (value, true),
                WatchEvent::Bookmark(_) => continue,
                WatchEvent::Error(status) => return Err(kube::Error::Api(status).into()),
            };
            if value["kind"] != "Table" {
                ensure!(
                    value["metadata"].is_object(),
                    "invalid JSON fallback from Table watch"
                );
                return Ok(false);
            }
            let table = Table::decode(value, &columns)?;
            columns.clone_from(&table.columns);
            self.send(if delete {
                Update::Delete(table)
            } else {
                Update::Apply(table)
            })
            .await?;
        }
        Ok(true)
    }

    async fn run(self, retry_interval: Duration, refresh_interval: Duration) {
        let mut watch_supported = true;
        let mut last_error = None;
        while !self.tx.is_closed() {
            let started = Instant::now();
            match self.list().await {
                Ok(None) => {
                    let _ = self.send(Update::Unavailable).await;
                    return;
                }
                Ok(Some(table)) => {
                    let version = table.resource_version.clone();
                    let columns = table.columns.clone();
                    if self.send(Update::Replace(table)).await.is_err() {
                        return;
                    }
                    if watch_supported && !version.is_empty() {
                        match timeout(refresh_interval, self.watch(&version, columns)).await {
                            Ok(Ok(supported)) => {
                                watch_supported = supported;
                                last_error = None;
                            }
                            Ok(Err(error))
                                if matches!(
                                    api_code(&error),
                                    Some(400 | 404 | 405 | 406 | 422)
                                ) =>
                            {
                                watch_supported = false;
                            }
                            Ok(Err(error)) if api_code(&error) == Some(410) => {}
                            Ok(Err(error)) => self.report(&error, &mut last_error).await,
                            Err(_) => {
                                last_error = None;
                            }
                        }
                    } else {
                        last_error = None;
                    }
                }
                Err(error) => self.report(&error, &mut last_error).await,
            }
            // One list cycle per five seconds at most, independent of row count
            // or UI ticks. A healthy watch also refreshes relative time cells.
            sleep_until(started + retry_interval).await;
        }
    }
}

#[cfg(test)]
mod tests;
