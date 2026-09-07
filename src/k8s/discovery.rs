use std::cmp::Reverse;

use anyhow::{Context, Result, ensure};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::APIResourceList;
use kube::Client;
use kube::core::Version;
use kube::core::discovery::v2::APIGroupDiscoveryList;

use super::{ApiResource, Kind};

pub(super) struct Resource {
    pub kind: Kind,
    pub short_names: Vec<String>,
}

// Keep short names from the wire documents. kube's Discovery conversion drops them.
pub(super) async fn discover(client: &Client) -> Result<Vec<Resource>> {
    let mut resources = match aggregated(client).await {
        Ok(resources) => resources,
        Err(_) => legacy(client).await.context("running API discovery")?,
    };
    // Select the most stable served version of each kind, including kinds absent
    // from the group's preferred version.
    resources.sort_by_cached_key(|r| {
        (
            r.kind.ar.group.clone(),
            r.kind.ar.kind.clone(),
            Reverse(Version::parse(&r.kind.ar.version).priority()),
        )
    });
    resources
        .dedup_by(|a, b| a.kind.ar.group == b.kind.ar.group && a.kind.ar.kind == b.kind.ar.kind);
    Ok(resources)
}

async fn aggregated(client: &Client) -> Result<Vec<Resource>> {
    let groups = client.list_api_groups_aggregated().await?;
    let core = client.list_core_api_versions_aggregated().await?;
    // Legacy responses deserialize as empty lists when negotiation is unsupported.
    ensure!(
        !groups.items.is_empty() || !core.items.is_empty(),
        "aggregated discovery is unavailable"
    );
    let mut resources = Vec::new();
    append_aggregated(&mut resources, groups)?;
    append_aggregated(&mut resources, core)?;
    Ok(resources)
}

fn append_aggregated(out: &mut Vec<Resource>, list: APIGroupDiscoveryList) -> Result<()> {
    for group in list.items {
        let name = group.metadata.and_then(|m| m.name).unwrap_or_default();
        ensure!(!group.versions.is_empty(), "empty API group: {name}");
        for version in group.versions {
            let version_name = version.version.unwrap_or_default();
            for resource in version.resources {
                let plural = resource.resource.unwrap_or_default();
                if plural.contains('/') {
                    continue;
                }
                out.push(Resource {
                    kind: Kind {
                        ar: ApiResource {
                            group: name.clone(),
                            version: version_name.clone(),
                            api_version: api_version(&name, &version_name),
                            kind: resource
                                .response_kind
                                .and_then(|k| k.kind)
                                .unwrap_or_default(),
                            plural,
                        },
                        namespaced: resource.scope.as_deref() == Some("Namespaced"),
                    },
                    short_names: resource.short_names,
                });
            }
        }
    }
    Ok(())
}

async fn legacy(client: &Client) -> Result<Vec<Resource>> {
    let mut resources = Vec::new();
    for group in client.list_api_groups().await?.groups {
        ensure!(
            !group.versions.is_empty(),
            "empty API group: {}",
            group.name
        );
        for version in group.versions {
            let list = client
                .list_api_group_resources(&version.group_version)
                .await?;
            append_legacy(&mut resources, list)?;
        }
    }
    let core = client.list_core_api_versions().await?;
    ensure!(!core.versions.is_empty(), "empty core API group");
    for version in core.versions {
        append_legacy(
            &mut resources,
            client.list_core_api_resources(&version).await?,
        )?;
    }
    Ok(resources)
}

fn append_legacy(out: &mut Vec<Resource>, list: APIResourceList) -> Result<()> {
    let gv: kube::core::GroupVersion = list.group_version.parse()?;
    for resource in list.resources {
        if resource.name.contains('/') {
            continue;
        }
        out.push(Resource {
            kind: Kind {
                ar: ApiResource {
                    group: resource.group.unwrap_or_else(|| gv.group.clone()),
                    version: resource.version.unwrap_or_else(|| gv.version.clone()),
                    api_version: gv.api_version(),
                    kind: resource.kind,
                    plural: resource.name,
                },
                namespaced: resource.namespaced,
            },
            short_names: resource.short_names.unwrap_or_default(),
        });
    }
    Ok(())
}

fn api_version(group: &str, version: &str) -> String {
    if group.is_empty() {
        version.to_string()
    } else {
        format!("{group}/{version}")
    }
}
