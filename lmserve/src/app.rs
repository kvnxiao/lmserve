use crate::artifacts;
use crate::cli::Action;
use crate::cli::Cli;
use crate::cli::Command;
use crate::cli::Target;
use crate::config;
use crate::config::Project;
use crate::lifecycle;
use crate::process;
use crate::runtime;
use crate::state;
use crate::state::Store;
use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use anyhow::ensure;
use camino::Utf8Path;
use camino::Utf8PathBuf;
use serde_json::json;
use std::collections::BTreeSet;
use std::process::Stdio;

pub(crate) fn run(cli: Cli) -> Result<()> {
    let store = Store::discover()?;
    match cli.command {
        Command::List => list(&store, &cli.file),
        Command::Validate { entry } => validate(&cli.file, &cli.provider, entry.as_deref()),
        Command::Plan { action, target } => plan(&store, &cli.file, &cli.provider, action, &target),
        Command::UpdateImages(target) => update(
            &store,
            &cli.file,
            &cli.provider,
            Action::UpdateImages,
            &target,
        ),
        Command::UpdateModels(target) => update(
            &store,
            &cli.file,
            &cli.provider,
            Action::UpdateModels,
            &target,
        ),
        Command::Start { entry } => submit(&store, &cli.file, &cli.provider, Action::Start, &entry),
        Command::Stop { entry } => submit(&store, &cli.file, &cli.provider, Action::Stop, &entry),
        Command::Restart { entry } => {
            submit(&store, &cli.file, &cli.provider, Action::Restart, &entry)
        }
        Command::Switch { entry } => {
            submit(&store, &cli.file, &cli.provider, Action::Switch, &entry)
        }
        Command::Status { entry } => status(&store, entry.as_deref()),
        Command::Logs {
            entry,
            service,
            follow,
        } => logs(&store, &entry, service.as_deref(), follow),
        Command::Health { entry } => health(&store, &entry),
        Command::Cdi { output } => cdi(output),
        Command::Worker { operation } => lifecycle::worker(&store, &operation),
    }
}

#[expect(
    clippy::print_stdout,
    reason = "CLI output reports command progress and results"
)]
pub(crate) fn report(message: &str) {
    println!("{message}");
}

fn submit(
    store: &Store,
    file: &Utf8Path,
    provider: &str,
    action: Action,
    entry: &str,
) -> Result<()> {
    match lifecycle::submit(store, file, provider, action, entry)? {
        lifecycle::Submission::Accepted(id) => report(&format!(
            "accepted {id}; use status to inspect startup, readiness, and the final outcome"
        )),
        lifecycle::Submission::Existing(id) => report(&format!(
            "{entry}: existing serving session {id}; use status for current state"
        )),
    }
    Ok(())
}

fn list(store: &Store, file: &Utf8Path) -> Result<()> {
    match Project::load(file) {
        Ok(project) => {
            for entry in project.models {
                report(&format!("configured: {entry}"));
            }
        }
        Err(error) => report(&format!("configuration unavailable: {error:#}")),
    }
    status(store, None)
}

fn validate(file: &Utf8Path, provider: &str, entry: Option<&str>) -> Result<()> {
    let project = Project::load(file)?;
    let entries = entry.map_or_else(|| project.models.clone(), |entry| vec![entry.to_owned()]);
    let mut failures = Vec::new();
    for entry in entries {
        match project.select(&entry, provider, None) {
            Ok(selection) => report(&format!(
                "{entry}: valid; selected services: {}",
                selection.services.join(", ")
            )),
            Err(error) => failures.push(format!("{entry}: {error:#}")),
        }
    }
    ensure!(
        failures.is_empty(),
        "configuration invalid: {}",
        failures.join("; ")
    );
    Ok(())
}

fn targets(project: &Project, target: &Target) -> Vec<String> {
    target
        .entry
        .as_ref()
        .map_or_else(|| project.models.clone(), |entry| vec![entry.clone()])
}

fn update(
    store: &Store,
    file: &Utf8Path,
    provider: &str,
    action: Action,
    target: &Target,
) -> Result<()> {
    let project = Project::load(file)?;
    let mut failures = Vec::new();
    let mut seen = BTreeSet::new();
    for entry in targets(&project, target) {
        let result = project
            .select(&entry, provider, None)
            .and_then(|selection| match action {
                Action::UpdateImages => {
                    artifacts::update_images(store, &selection, provider, &mut seen)
                }
                Action::UpdateModels => {
                    report(&format!("{entry}: resolving and preparing model artifacts"));
                    let prepared = artifacts::update(store, &selection)?;
                    report(&format!(
                        "{entry}: prepared {} at {}",
                        prepared.revision, prepared.path
                    ));
                    Ok(())
                }
                _ => bail!("invalid preparation action"),
            });
        if let Err(error) = result {
            report(&format!("{entry}: failed: {error:#}"));
            failures.push(entry);
        }
    }
    ensure!(
        failures.is_empty(),
        "preparation failed for {}",
        failures.join(", ")
    );
    Ok(())
}

fn plan(
    store: &Store,
    file: &Utf8Path,
    provider: &str,
    action: Action,
    target: &Target,
) -> Result<()> {
    ensure!(
        !target.all || matches!(action, Action::UpdateImages | Action::UpdateModels),
        "--all is only valid for update plans"
    );
    let mut state = store.read()?;
    state::reconcile_workers(&mut state);
    if action == Action::Stop {
        let entry = target
            .entry
            .as_deref()
            .context("stop plan requires an entry")?;
        let key = lifecycle::entry_key(&state, entry)?;
        let deployment = state.deployments.get(&key).context("missing deployment")?;
        report(&json!({"action": action, "entry": entry, "services": deployment.components.keys().collect::<Vec<_>>(),
            "interrupts_requests": true, "preserves_volumes_images_artifacts": true}).to_string());
        return Ok(());
    }
    let project = Project::load(file)?;
    let runtime = runtime::containers();
    for entry in targets(&project, target) {
        let selection = project.select(&entry, provider, None)?;
        let prepared = artifacts::prepared(&state, &selection);
        let active = runtime
            .as_ref()
            .ok()
            .map(|containers| runtime::active_keys(&state, containers))
            .transpose()?;
        let images = config::object(config::field(&selection.document, "services"))?.iter().map(|(name, service)| {
            let reference = runtime::image_reference(&selection.project, name, service);
            let local = runtime::image_identity(&reference);
            json!({"service": name, "image": reference, "local_identity": local.as_ref().ok(), "inspection_error": local.err().map(|error| error.to_string())})
        }).collect::<Vec<_>>();
        let previous = runtime
            .as_ref()
            .ok()
            .map(|current| runtime::owned_groups(&state, current))
            .transpose()?
            .unwrap_or_default();
        let retention = runtime.as_ref().ok().map(|current| {
            runtime::components(&selection).and_then(|components| {
                lifecycle::retained_companions(&selection, &state, &previous, &components, current)
            })
        });
        let retained = retention.as_ref().and_then(|result| result.as_ref().ok());
        let retained_tokens: BTreeSet<_> = retained
            .into_iter()
            .flat_map(|components| components.values())
            .map(|component| component.token.as_str())
            .collect();
        let replacements = previous
            .iter()
            .filter_map(|key| state.deployments.get(key))
            .flat_map(|deployment| &deployment.components)
            .filter(|(_, component)| !retained_tokens.contains(component.token.as_str()))
            .map(|(name, component)| json!({"service": name, "container": component.id}))
            .collect::<Vec<_>>();
        let key = state::key(&selection.project, &entry);
        let deletion = state.prepared.get(&key).filter(|prepared| {
            prepared.root == selection.root
                && !state.prepared.iter().any(|(other, item)| {
                    other != &key && item.owned_directory == prepared.owned_directory
                })
                && !active
                    .as_ref()
                    .into_iter()
                    .flatten()
                    .filter_map(|key| state.deployments.get(key))
                    .any(|deployment| {
                        deployment.artifact.owned_directory == prepared.owned_directory
                    })
        });
        report(&json!({"action": action, "entry": entry, "services": selection.services,
            "active_deployments": active, "runtime_inspection_error": runtime.as_ref().err().map(ToString::to_string),
            "artifact_prepared": prepared.is_ok(), "images": images,
            "replaces_session": matches!(action, Action::Restart | Action::Switch),
            "interrupts_requests": matches!(action, Action::Restart | Action::Switch),
            "retained_companions": retained.map(|components| components.keys().collect::<Vec<_>>()),
            "retention_error": retention.as_ref().and_then(|result| result.as_ref().err()).map(ToString::to_string),
            "replaced_components": if matches!(action, Action::Start | Action::Restart | Action::Switch) { replacements } else { vec![] },
            "preparation": if action == Action::UpdateModels { "resolve configured revision and download missing content" } else if action == Action::UpdateImages { "pull configured images and build configured sources" } else { "none; missing dependencies reject startup" },
            "proposed_cache_deletions": if action == Action::UpdateModels { deletion.map(|prepared| &prepared.owned_directory) } else { None },
            "deletion_condition": "superseded, owned, and unreferenced after successful replacement",
            "remote_changes_and_download_size": "unresolved"}).to_string());
    }
    Ok(())
}

fn status(store: &Store, entry: Option<&str>) -> Result<()> {
    let mut state = store.read()?;
    state::reconcile_workers(&mut state);
    let inspected = runtime::containers();
    for (key, deployment) in &state.deployments {
        if entry.is_some_and(|entry| entry != deployment.entry) {
            continue;
        }
        let mut components = Vec::new();
        let model_health = inspected
            .as_ref()
            .ok()
            .map(|current| runtime::model_health(deployment, current))
            .transpose()?
            .unwrap_or("runtime unavailable");
        for (name, component) in &deployment.components {
            let running = inspected
                .as_ref()
                .ok()
                .map(|containers| {
                    runtime::owned(component, name, containers)
                        .map(|container| container.map(|container| container.running))
                })
                .transpose()?
                .flatten();
            components.push(
                json!({"service": name, "container": component.id, "running": running,
                "image": component.image, "error": component.error}),
            );
        }
        report(&json!({"deployment": key, "operation": deployment.operation, "artifact_revision": deployment.artifact.revision,
            "artifact_path": deployment.artifact.path, "model_health": model_health, "components": components}).to_string());
    }
    for operation in state.operations.values() {
        if entry.is_none_or(|entry| {
            state
                .deployments
                .get(&operation.key)
                .is_some_and(|deployment| deployment.entry == entry)
        }) {
            report(&serde_json::to_string(operation)?);
        }
    }
    if let Err(error) = inspected {
        bail!("runtime inspection unavailable: {error:#}");
    }
    Ok(())
}

fn health(store: &Store, entry: &str) -> Result<()> {
    let state = store.read()?;
    let key = lifecycle::entry_key(&state, entry)?;
    let deployment = state.deployments.get(&key).context("missing deployment")?;
    let current = runtime::containers()?;
    let health = runtime::model_health(deployment, &current)?;
    ensure!(health == "ready", "{entry}: {health}");
    report(&format!("{entry}: ready"));
    Ok(())
}

fn logs(store: &Store, entry: &str, service: Option<&str>, follow: bool) -> Result<()> {
    let state = store.read()?;
    let key = lifecycle::entry_key(&state, entry)?;
    let deployment = state.deployments.get(&key).context("missing deployment")?;
    let service = service.unwrap_or(entry);
    let component = deployment
        .components
        .get(service)
        .context("service is not in the recorded group")?;
    let current = runtime::containers()?;
    let container =
        runtime::owned(component, service, &current)?.context("recorded container is absent")?;
    let mut command = std::process::Command::new("podman");
    command.arg("logs");
    if follow {
        command.arg("--follow");
    }
    let result = command.arg(&container.id).stdin(Stdio::null()).status()?;
    ensure!(result.success(), "container logs failed");
    Ok(())
}

fn cdi(output: Option<Utf8PathBuf>) -> Result<()> {
    let output =
        output.unwrap_or(state::xdg("XDG_CONFIG_HOME", ".config")?.join("cdi/nvidia.yaml"));
    let output = if output.is_absolute() {
        output
    } else {
        Utf8PathBuf::from_path_buf(std::env::current_dir()?)
            .map_err(|_redacted_error| anyhow::anyhow!("working directory is not UTF-8"))?
            .join(output)
    };
    let parent = output.parent().context("CDI output has no parent")?;
    state::private_directory(parent)?;
    let temporary = tempfile::NamedTempFile::new_in(parent)?;
    process::output(
        std::process::Command::new("nvidia-ctk")
            .args(["cdi", "generate", "--output"])
            .arg(temporary.path()),
        None,
        process::INSPECTION_TIMEOUT,
    )?;
    let bytes = fs_err::read(temporary.path())?;
    let document = config::parse(&bytes)?;
    ensure!(
        config::field(&document, "cdiVersion").is_string()
            && config::field(&document, "devices").is_array(),
        "NVIDIA toolkit returned incomplete CDI configuration"
    );
    state::atomic_write(&output, &bytes)?;
    report(&format!(
        "generated {output}; configure rootless Podman to read this CDI directory"
    ));
    Ok(())
}
