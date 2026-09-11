//! Cluster-independent `sofka plugin` command output and orchestration.

use std::collections::{HashMap, HashSet};

use semver::Version;
use serde::Serialize;

use crate::plugin_catalog::{CatalogPlugin, CatalogSnapshot, CatalogVersion, VersionStatus};
use crate::plugin_install::{Activation, InstallLock, InstalledPackage};

#[derive(clap::Args, Debug, Clone)]
pub struct PluginArgs {
    #[command(subcommand)]
    pub command: PluginCommand,
}

#[derive(clap::Subcommand, Debug, Clone)]
pub enum PluginCommand {
    /// Search the official reviewed catalog.
    Search {
        /// Match plugin IDs, names, descriptions, and tags.
        #[arg(default_value = "")]
        query: String,
        /// Use previously cached catalog metadata.
        #[arg(long)]
        offline: bool,
        /// Print stable machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show one plugin version and its runtime behavior.
    Describe {
        /// Plugin ID, optionally followed by @VERSION.
        plugin: String,
        /// Use previously cached catalog metadata.
        #[arg(long)]
        offline: bool,
        /// Print stable machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Install or explicitly roll back one or more plugins.
    Install {
        /// Plugin IDs, optionally followed by @VERSION.
        #[arg(required = true)]
        plugins: Vec<String>,
        /// Use cached catalog metadata and artifacts.
        #[arg(long)]
        offline: bool,
    },
    /// Update managed plugins to the latest compatible versions.
    Update {
        /// Managed plugin IDs. With none, update every managed plugin.
        plugins: Vec<String>,
        /// Use cached catalog metadata and artifacts.
        #[arg(long)]
        offline: bool,
    },
    /// List installed managed and manual plugin packages without network access.
    List {
        /// Print stable machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Remove one or more catalog-managed plugins.
    Remove {
        /// Managed plugin IDs.
        #[arg(required = true)]
        plugins: Vec<String>,
    },
}

#[derive(Serialize)]
struct SearchRow<'a> {
    id: &'a str,
    display_name: &'a str,
    description: &'a str,
    tags: &'a [String],
    latest_version: Option<&'a str>,
    compatible: bool,
    installed: bool,
    installed_version: Option<&'a str>,
}

#[derive(Serialize)]
struct Description<'a> {
    id: &'a str,
    display_name: &'a str,
    description: &'a str,
    tags: &'a [String],
    publisher: &'a str,
    repository: &'a str,
    version: &'a str,
    status: &'a str,
    withdrawal_reason: Option<&'a str>,
    license: &'a str,
    readme: &'a str,
    sofka: &'a str,
    platforms: Vec<&'a str>,
    requirements: &'a [crate::plugin_catalog::RuntimeRequirement],
    command: &'a str,
    target: &'a str,
    output: &'a str,
    mutating: bool,
    confirm: bool,
    dangerous: bool,
    network_load: bool,
    installed: bool,
    installed_version: Option<&'a str>,
}

pub async fn run(args: &PluginArgs) -> Result<(), String> {
    match &args.command {
        PluginCommand::Search {
            query,
            offline,
            json,
        } => search(query, *offline, *json).await,
        PluginCommand::Describe {
            plugin,
            offline,
            json,
        } => describe(plugin, *offline, *json).await,
        PluginCommand::Install { plugins, offline } => install(plugins, *offline).await,
        PluginCommand::Update { plugins, offline } => update(plugins, *offline).await,
        PluginCommand::List { json } => list(*json),
        PluginCommand::Remove { plugins } => remove(plugins),
    }
}

async fn search(query: &str, offline: bool, json: bool) -> Result<(), String> {
    let snapshot = crate::plugin_catalog::load(offline).await?;
    offline_notice(&snapshot);
    let installed = installed_map()?;
    let rows: Vec<_> = snapshot
        .catalog
        .matching(query)
        .into_iter()
        .map(|plugin| {
            let latest = plugin.latest_compatible();
            let installed = installed.get(plugin.id.as_str());
            SearchRow {
                id: &plugin.id,
                display_name: &plugin.display_name,
                description: &plugin.description,
                tags: &plugin.tags,
                latest_version: latest.map(|release| release.version.as_str()),
                compatible: latest.is_some(),
                installed: installed.is_some(),
                installed_version: installed.and_then(|package| package.version.as_deref()),
            }
        })
        .collect();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&rows).map_err(|e| e.to_string())?
        );
    } else {
        for row in rows {
            let compatibility = row.latest_version.map_or("incompatible", |_| "compatible");
            let installed = row
                .installed_version
                .map_or(String::new(), |version| format!(" installed={version}"));
            println!(
                "{}\t{}\t{}{}\t{}",
                row.id,
                row.latest_version.unwrap_or("-"),
                compatibility,
                installed,
                row.description
            );
        }
    }
    Ok(())
}

async fn describe(request: &str, offline: bool, json: bool) -> Result<(), String> {
    let snapshot = crate::plugin_catalog::load(offline).await?;
    offline_notice(&snapshot);
    let (plugin, release) = described_release(&snapshot, request)?;
    let installed = installed_map()?;
    let installed = installed.get(plugin.id.as_str());
    let description = Description {
        id: &plugin.id,
        display_name: &plugin.display_name,
        description: &plugin.description,
        tags: &plugin.tags,
        publisher: &plugin.publisher,
        repository: &plugin.repository,
        version: &release.version,
        status: match release.status {
            VersionStatus::Active => "active",
            VersionStatus::Withdrawn => "withdrawn",
        },
        withdrawal_reason: release.withdrawal_reason.as_deref(),
        license: &release.license,
        readme: &release.readme,
        sofka: &release.sofka,
        platforms: release
            .artifacts
            .iter()
            .map(|artifact| artifact.platform.as_str())
            .collect(),
        requirements: &release.requirements,
        command: &release.command,
        target: &release.target,
        output: &release.output,
        mutating: release.mutating,
        confirm: release.confirm,
        dangerous: release.dangerous,
        network_load: release.network_load,
        installed: installed.is_some(),
        installed_version: installed.and_then(|package| package.version.as_deref()),
    };
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&description).map_err(|e| e.to_string())?
        );
    } else {
        println!("{} ({})", description.display_name, description.id);
        println!("version: {} ({})", description.version, description.status);
        if let Some(reason) = description.withdrawal_reason {
            println!("withdrawal: {reason}");
        }
        println!("description: {}", description.description);
        println!("publisher: {}", description.publisher);
        println!("repository: {}", description.repository);
        println!("license: {}", description.license);
        println!("requires sofka: {}", description.sofka);
        println!("platforms: {}", description.platforms.join(", "));
        println!("command: {}", description.command);
        println!("target: {}", description.target);
        println!("output: {}", description.output);
        println!("mutating: {}", description.mutating);
        println!(
            "confirmation: {}",
            description.confirm || description.dangerous
        );
        println!("network load: {}", description.network_load);
        println!(
            "installed: {}",
            description.installed_version.unwrap_or("no")
        );
        for requirement in description.requirements {
            println!("requires: {} — {}", requirement.name, requirement.install);
        }
    }
    Ok(())
}

async fn install(requests: &[String], offline: bool) -> Result<(), String> {
    let snapshot = crate::plugin_catalog::load(offline).await?;
    offline_notice(&snapshot);
    report_missing_requirements(&snapshot, requests)?;
    let config = crate::plugin_catalog::config_dir()?;
    let _lock = InstallLock::acquire(&config)?;
    let prepared = crate::plugin_install::prepare(&snapshot, requests, offline).await?;
    activate(prepared)?;
    println!("Run :reload in an existing sofka session to load the changes.");
    Ok(())
}

async fn update(requested: &[String], offline: bool) -> Result<(), String> {
    let installed = crate::plugin_install::installed()?;
    let managed: HashMap<_, _> = installed
        .iter()
        .filter(|package| package.managed)
        .map(|package| (package.id.as_str(), package))
        .collect();
    let ids: Vec<String> = if requested.is_empty() {
        let mut ids: Vec<_> = managed.keys().map(|id| (*id).to_string()).collect();
        ids.sort();
        ids
    } else {
        requested.to_vec()
    };
    if ids.is_empty() {
        println!("No managed plugins are installed.");
        return Ok(());
    }
    for id in &ids {
        let package = managed
            .get(id.as_str())
            .ok_or_else(|| format!("plugin {id} is not a managed installation"))?;
        if package.modified {
            return Err(format!(
                "plugin {id} at {} has local modifications; update refused",
                package.path.display()
            ));
        }
    }
    let snapshot = crate::plugin_catalog::load(offline).await?;
    offline_notice(&snapshot);
    let mut updates = Vec::new();
    for id in ids {
        let current = managed
            .get(id.as_str())
            .and_then(|package| package.version.as_deref())
            .and_then(|version| Version::parse(version).ok())
            .ok_or_else(|| format!("plugin {id} has an invalid installed version"))?;
        let selected = snapshot.catalog.select(&id)?;
        let available = Version::parse(&selected.version.version)
            .map_err(|e| format!("plugin {id} catalog version: {e}"))?;
        if available > current {
            updates.push(id);
        } else {
            println!("{id} is current at {current}");
        }
    }
    if updates.is_empty() {
        return Ok(());
    }
    report_missing_requirements(&snapshot, &updates)?;
    let config = crate::plugin_catalog::config_dir()?;
    let _lock = InstallLock::acquire(&config)?;
    let prepared = crate::plugin_install::prepare(&snapshot, &updates, offline).await?;
    activate(prepared)?;
    println!("Run :reload in an existing sofka session to load the changes.");
    Ok(())
}

fn activate(prepared: Vec<crate::plugin_install::PreparedPackage>) -> Result<(), String> {
    let mut failed = Vec::new();
    for package in prepared {
        let id = package.id.clone();
        let version = package.version.clone();
        match package.activate() {
            Ok(action) => println!(
                "{} {id}@{version}",
                match action {
                    Activation::Installed => "installed",
                    Activation::Updated => "updated",
                    Activation::RolledBack => "rolled back",
                    Activation::Unchanged => "already installed",
                }
            ),
            Err(error) => {
                eprintln!("{id}: {error}");
                failed.push(id);
            }
        }
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(format!("failed to activate: {}", failed.join(", ")))
    }
}

fn list(json: bool) -> Result<(), String> {
    let packages = crate::plugin_install::installed()?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&packages).map_err(|e| e.to_string())?
        );
    } else {
        for package in packages {
            let kind = if package.managed { "managed" } else { "manual" };
            let modified = if package.modified { " modified" } else { "" };
            println!(
                "{}\t{}\t{kind}{modified}\t{}",
                package.id,
                package.version.as_deref().unwrap_or("-"),
                package.path.display()
            );
        }
    }
    Ok(())
}

fn remove(ids: &[String]) -> Result<(), String> {
    let config = crate::plugin_catalog::config_dir()?;
    let _lock = InstallLock::acquire(&config)?;
    for (id, path) in crate::plugin_install::remove(ids)? {
        println!("removed {id} from {}", path.display());
    }
    println!("Run :reload in an existing sofka session to load the changes.");
    Ok(())
}

fn installed_map() -> Result<HashMap<String, InstalledPackage>, String> {
    Ok(crate::plugin_install::installed()?
        .into_iter()
        .filter(|package| package.managed)
        .map(|package| (package.id.clone(), package))
        .collect())
}

fn report_missing_requirements(
    snapshot: &CatalogSnapshot,
    requests: &[String],
) -> Result<(), String> {
    let mut reported = HashSet::new();
    for request in requests {
        let selection = snapshot.catalog.select(request)?;
        for requirement in &selection.version.requirements {
            if reported.insert((selection.plugin.id.as_str(), requirement.name.as_str()))
                && crate::plugins::executable(&requirement.name).is_none()
            {
                eprintln!(
                    "warning: {} requires {} — {}",
                    selection.plugin.id, requirement.name, requirement.install
                );
            }
        }
    }
    Ok(())
}

fn described_release<'a>(
    snapshot: &'a CatalogSnapshot,
    request: &str,
) -> Result<(&'a CatalogPlugin, &'a CatalogVersion), String> {
    let (id, exact) = crate::plugin_catalog::parse_request(request)?;
    let plugin = snapshot.catalog.find(id)?;
    let release = if let Some(exact) = exact {
        let wanted = Version::parse(exact)
            .map_err(|e| format!("invalid requested version {exact:?}: {e}"))?;
        plugin
            .versions
            .iter()
            .find(|release| Version::parse(&release.version).ok().as_ref() == Some(&wanted))
            .ok_or_else(|| format!("plugin {id} has no version {exact}"))?
    } else {
        plugin
            .latest_compatible()
            .or_else(|| {
                plugin
                    .versions
                    .iter()
                    .filter_map(|release| {
                        Version::parse(&release.version)
                            .ok()
                            .map(|version| (version, release))
                    })
                    .max_by(|(a, _), (b, _)| a.cmp(b))
                    .map(|(_, release)| release)
            })
            .ok_or_else(|| format!("plugin {id} has no published versions"))?
    };
    Ok((plugin, release))
}

fn offline_notice(snapshot: &CatalogSnapshot) {
    if snapshot.offline {
        eprintln!(
            "using cached catalog from {} ago; withdrawal information may be stale",
            crate::plugin_catalog::age(snapshot.fetched_at)
        );
    }
}
