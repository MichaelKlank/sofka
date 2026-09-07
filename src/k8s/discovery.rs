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

pub(super) struct Discovered {
    pub resources: Vec<Resource>,
    pub warnings: Vec<String>,
}

// Keep short names from the wire documents. kube's Discovery conversion drops them.
pub(super) async fn discover(client: &Client) -> Result<Discovered> {
    let mut warnings = Vec::new();
    let mut resources = match aggregated(client).await {
        Ok(Some(resources)) => resources,
        Ok(None) => legacy(client, &mut warnings).await?,
        Err(e) => {
            warnings.push(format!(
                "Aggregated API discovery failed: {e}. Sofka read each API group separately."
            ));
            legacy(client, &mut warnings).await?
        }
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
    Ok(Discovered {
        resources,
        warnings,
    })
}

async fn aggregated(client: &Client) -> Result<Option<Vec<Resource>>> {
    let groups = client.list_api_groups_aggregated().await?;
    let core = client.list_core_api_versions_aggregated().await?;
    // Legacy responses deserialize as empty lists when negotiation is unsupported.
    if groups.items.is_empty() && core.items.is_empty() {
        return Ok(None);
    }
    let mut resources = Vec::new();
    append_aggregated(&mut resources, groups)?;
    append_aggregated(&mut resources, core)?;
    Ok(Some(resources))
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

async fn legacy(client: &Client, warnings: &mut Vec<String>) -> Result<Vec<Resource>> {
    let mut resources = Vec::new();
    for group in client
        .list_api_groups()
        .await
        .context("running API discovery")?
        .groups
    {
        if group.versions.is_empty() {
            warnings.push(format!(
                "API discovery could not read {}: the group has no versions",
                group.name
            ));
            continue;
        }
        for version in group.versions {
            let gv = version.group_version;
            let result = match client.list_api_group_resources(&gv).await {
                Ok(list) => append_legacy(&mut resources, list),
                Err(e) => Err(e.into()),
            };
            if let Err(e) = result {
                warnings.push(format!("API discovery could not read {gv}: {e}"));
            }
        }
    }
    let core = client
        .list_core_api_versions()
        .await
        .context("running API discovery")?;
    ensure!(!core.versions.is_empty(), "empty core API group");
    for version in core.versions {
        let result = match client.list_core_api_resources(&version).await {
            Ok(list) => append_legacy(&mut resources, list),
            Err(e) => Err(e.into()),
        };
        if let Err(e) = result {
            warnings.push(format!("API discovery could not read {version}: {e}"));
        }
    }
    ensure!(
        !resources.is_empty(),
        "running API discovery: sofka could not read any API group"
    );
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
