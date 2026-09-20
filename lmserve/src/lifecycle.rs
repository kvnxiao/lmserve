use crate::artifacts;
use crate::cli::Action;
use crate::config;
use crate::config::Project;
use crate::config::Selection;
use crate::process;
use crate::runtime;
use crate::state;
use crate::state::Component;
use crate::state::Deployment;
use crate::state::Operation;
use crate::state::Phase;
use crate::state::ProcessIdentity;
use crate::state::Request;
use crate::state::State;
use crate::state::Store;
use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use anyhow::ensure;
use camino::Utf8Path;
use serde_json::Value;
use std::collections::BTreeMap;
use std::process::Command;
use std::process::Stdio;
use std::thread;
use std::time::Duration;
use std::time::Instant;

pub(crate) enum Submission {
    Accepted(String),
    Existing(String),
}

pub(crate) fn submit(
    store: &Store,
    file: &Utf8Path,
    provider: &str,
    action: Action,
    entry: &str,
) -> Result<Submission> {
    if action == Action::Stop {
        return submit_stop(store, entry).map(Submission::Accepted);
    }
    let _lock = store.lock("control")?;
    let mut state = store.read()?;
    state::reconcile_workers(&mut state);
    let current = runtime::containers()?;
    let mut previous = runtime::active_keys(&state, &current)?;
    if action == Action::Start
        && let Some(existing) = previous
            .iter()
            .filter_map(|key| state.deployments.get(key))
            .find(|deployment| deployment.entry == entry)
    {
        return Ok(Submission::Existing(existing.operation.clone()));
    }
    ensure!(
        !state
            .operations
            .values()
            .any(|operation| operation.phase.pending()),
        "another lifecycle operation is busy"
    );
    let _execution = store.lock("execution")?;
    let project = Project::load(file)?;
    let preview = project.select(entry, provider, None)?;
    let artifact = artifacts::prepared(&state, &preview)?;
    let mut selection = project.select(entry, provider, Some(&artifact.path))?;
    let key = state::key(&selection.project, entry);
    match action {
        Action::Start => ensure!(
            previous.is_empty(),
            "a managed model is active; use switch or restart explicitly"
        ),
        Action::Restart => ensure!(
            previous.iter().all(|active| active == &key),
            "another model is active; use switch"
        ),
        Action::Switch => (),
        _ => bail!("invalid lifecycle action"),
    }
    runtime::prerequisites(&selection)?;
    crate::inputs::validate(&selection)?;
    let mut components = runtime::components(&selection)?;
    for record in runtime::owned_groups(&state, &current)? {
        if !previous.contains(&record)
            && state
                .deployments
                .get(&record)
                .is_some_and(|deployment| deployment.project == selection.project)
        {
            previous.push(record);
        }
    }
    if state.deployments.contains_key(&key) && !previous.contains(&key) {
        previous.push(key.clone());
    }
    let retained = retained_companions(&selection, &state, &previous, &components, &current)?;
    runtime::conflicts(&selection, &state, &previous, &current)?;
    components.extend(retained.clone());
    let id = state::identifier()?;
    let environment_file = crate::inputs::capture(
        &mut selection,
        &store.directory.join(format!("inputs-{id}")),
    )?;
    let deployment = Deployment {
        project: selection.project.clone(),
        entry: entry.to_owned(),
        operation: id.clone(),
        readiness: selection.model.readiness.url.clone(),
        artifact,
        components,
    };
    let mut request = Request {
        provider: provider.to_owned(),
        environment_file,
        selection,
        deployment,
        previous,
        retained,
    };
    admit_worker(store, &mut state, &mut request, action)
}

fn admit_worker(
    store: &Store,
    state: &mut State,
    request: &mut Request,
    action: Action,
) -> Result<Submission> {
    let key = state::key(&request.selection.project, &request.selection.entry);
    let id = request.deployment.operation.clone();
    if let Some(old) = state.deployments.remove(&key) {
        let archived = format!("{key}@{}", old.operation);
        for item in &mut request.previous {
            if item == &key {
                item.clone_from(&archived);
            }
        }
        state.deployments.insert(archived, old);
    }
    state::atomic_write(&store.request_path(&id), &serde_json::to_vec(request)?)?;
    state
        .deployments
        .insert(key.clone(), request.deployment.clone());
    publish_worker(
        store,
        state,
        Operation {
            id: id.clone(),
            action,
            key,
            phase: Phase::Accepted,
            worker: None,
            error: None,
        },
    )?;
    Ok(Submission::Accepted(id))
}

pub(crate) fn retained_companions(
    selection: &Selection,
    state: &State,
    previous: &[String],
    components: &BTreeMap<String, Component>,
    current: &[runtime::Container],
) -> Result<BTreeMap<String, Component>> {
    let mut retained = BTreeMap::new();
    for deployment in previous.iter().filter_map(|key| state.deployments.get(key)) {
        if deployment.project != selection.project {
            continue;
        }
        for (name, component) in &deployment.components {
            if name == &deployment.entry || name == &selection.entry {
                continue;
            }
            if let Some(new) = components.get(name)
                && runtime::owned(component, name, current)?
                    .is_some_and(|container| container.running)
            {
                ensure!(
                    new.configuration == component.configuration && new.image == component.image,
                    "shared companion {name} changed; explicitly stop the deployment before starting it"
                );
                retained.insert(name.clone(), component.clone());
            }
        }
    }
    Ok(retained)
}

fn submit_stop(store: &Store, entry: &str) -> Result<String> {
    let snapshot = store.read()?;
    let key = entry_key(&snapshot, entry)?;
    for operation in snapshot
        .operations
        .values()
        .filter(|operation| operation.phase.pending())
    {
        ensure!(operation.key == key, "another lifecycle operation is busy");
        state::atomic_write(&store.cancel_path(&operation.id), b"cancel")?;
    }
    let _lock = wait_control(store)?;
    let mut state = store.read()?;
    state::reconcile_workers(&mut state);
    ensure!(
        !state
            .operations
            .values()
            .any(|operation| operation.phase.pending() && operation.key != key),
        "another lifecycle operation is busy"
    );
    let id = state::identifier()?;
    publish_worker(
        store,
        &mut state,
        Operation {
            id: id.clone(),
            action: Action::Stop,
            key,
            phase: Phase::Accepted,
            worker: None,
            error: None,
        },
    )?;
    Ok(id)
}

fn publish_worker(store: &Store, state: &mut State, mut operation: Operation) -> Result<()> {
    state
        .operations
        .insert(operation.id.clone(), operation.clone());
    store.save(state)?;
    let child = Command::new(std::env::current_exe()?)
        .args(["worker", &operation.id])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    match child {
        Ok(mut child) => {
            match ProcessIdentity::read(child.id()) {
                Ok(identity) => operation.worker = Some(identity),
                Err(error) => {
                    child.kill()?;
                    child.wait()?;
                    operation.phase = Phase::Failed;
                    operation.error = Some(format!("worker failed to start: {error:#}"));
                }
            }
            state
                .operations
                .insert(operation.id.clone(), operation.clone());
            store.save(state)?;
            ensure!(
                operation.phase == Phase::Accepted,
                "worker failed before acceptance"
            );
        }
        Err(error) => {
            operation.phase = Phase::Failed;
            operation.error = Some("worker could not be spawned".to_owned());
            state.operations.insert(operation.id.clone(), operation);
            store.save(state)?;
            return Err(error).context("spawn lifecycle worker");
        }
    }
    Ok(())
}

pub(crate) fn entry_key(state: &State, entry: &str) -> Result<String> {
    let keys: Vec<_> = state
        .deployments
        .iter()
        .filter(|(key, deployment)| deployment.entry == entry && !key.contains('@'))
        .map(|(key, _)| key.clone())
        .collect();
    ensure!(
        keys.len() == 1,
        "entry {entry} has no unique recorded deployment"
    );
    keys.first().cloned().context("missing deployment")
}

pub(crate) fn worker(store: &Store, id: &str) -> Result<()> {
    ensure!(
        id.len() == 64 && id.bytes().all(|c| c.is_ascii_hexdigit()),
        "invalid operation identifier"
    );
    nix::unistd::setsid().context("detach lifecycle worker from terminal")?;
    let operation = {
        let _lock = wait_control(store)?;
        store
            .read()?
            .operations
            .get(id)
            .cloned()
            .context("operation is not recorded")?
    };
    ensure!(
        operation.worker.as_ref() == Some(&ProcessIdentity::read(std::process::id())?),
        "worker does not own operation"
    );
    let result = execute_operation(store, &operation);
    let _lock = wait_control(store)?;
    let mut state = store.read()?;
    let recorded = state
        .operations
        .get_mut(id)
        .context("operation disappeared")?;
    match &result {
        Ok(()) => {
            recorded.phase = if operation.action == Action::Stop {
                Phase::Stopped
            } else {
                Phase::Ready
            }
        }
        Err(error) => {
            recorded.phase = if store.cancel_path(id).exists() {
                Phase::Cancelled
            } else {
                Phase::Failed
            };
            recorded.error = Some(format!("{error:#}"));
        }
    }
    store.save(&state)?;
    result
}

fn execute_operation(store: &Store, operation: &Operation) -> Result<()> {
    let execution = wait_lock(store, "execution")?;
    process::inherit_execution_lease(&execution, &store.cancel_path(&operation.id))?;
    if operation.action == Action::Stop {
        return stop_deployment(store, operation);
    }
    let request: Request =
        serde_json::from_slice(&fs_err::read(store.request_path(&operation.id))?)?;
    start_deployment(store, operation, &request)
}

pub(crate) fn wait_control(store: &Store) -> Result<std::fs::File> {
    wait_lock(store, "control")
}

fn wait_lock(store: &Store, name: &str) -> Result<std::fs::File> {
    let deadline = Instant::now() + process::INSPECTION_TIMEOUT;
    loop {
        match store.lock(name) {
            Ok(lock) => return Ok(lock),
            Err(error)
                if Instant::now() >= deadline
                    || !matches!(
                        error.downcast_ref::<std::fs::TryLockError>(),
                        Some(std::fs::TryLockError::WouldBlock)
                    ) =>
            {
                return Err(error);
            }
            Err(_) => thread::sleep(Duration::from_millis(20)),
        }
    }
}

fn phase(store: &Store, id: &str, phase: Phase) -> Result<()> {
    let _lock = wait_control(store)?;
    let mut state = store.read()?;
    state
        .operations
        .get_mut(id)
        .context("missing operation")?
        .phase = phase;
    store.save(&state)
}

fn cancelled(store: &Store, id: &str) -> Result<()> {
    ensure!(!store.cancel_path(id).exists(), "startup cancelled");
    Ok(())
}

fn start_deployment(store: &Store, operation: &Operation, request: &Request) -> Result<()> {
    cancelled(store, &operation.id)?;
    {
        let _lock = wait_control(store)?;
        let state = store.read()?;
        let current = runtime::containers()?;
        let active = runtime::active_keys(&state, &current)?;
        ensure!(
            active.iter().all(|key| request.previous.contains(key)),
            "runtime changed after acceptance; another model is active"
        );
        runtime::conflicts(&request.selection, &state, &request.previous, &current)?;
        runtime::prerequisites(&request.selection)?;
        crate::inputs::validate(&request.selection)?;
        artifacts::readable(&request.deployment.artifact.path)?;
        artifacts::verify_manifest(&request.deployment.artifact)?;
        for component in request.deployment.components.values() {
            runtime::image_identity(&component.image)?;
        }
    }
    phase(store, &operation.id, Phase::Stopping)?;
    for key in &request.previous {
        let state = store.read()?;
        if let Some(deployment) = state.deployments.get(key) {
            stop_group(deployment, &request.retained)?;
        }
    }
    phase(store, &operation.id, Phase::Starting)?;
    let result = launch_and_wait(store, operation, request);
    if let Err(error) = result {
        let state = store.read()?;
        let deployment = state
            .deployments
            .get(&operation.key)
            .context("deployment disappeared")?;
        let cleanup = stop_group(deployment, &BTreeMap::new());
        return match cleanup {
            Ok(()) => Err(error),
            Err(cleanup) => Err(error.context(format!("startup cleanup also failed: {cleanup:#}"))),
        };
    }
    let _lock = wait_control(store)?;
    let mut state = store.read()?;
    for key in &request.previous {
        if let Some(deployment) = state.deployments.get_mut(key) {
            deployment
                .components
                .retain(|name, _| !request.retained.contains_key(name));
        }
    }
    store.save(&state)
}

fn stop_group(deployment: &Deployment, retained: &BTreeMap<String, Component>) -> Result<()> {
    let mut failed = Vec::new();
    if let Some(model) = deployment.components.get(&deployment.entry)
        && let Err(error) = runtime::stop(model, &deployment.entry)
    {
        failed.push(format!("{}: {error:#}", deployment.entry));
    }
    for (name, component) in &deployment.components {
        if name == &deployment.entry
            || retained
                .get(name)
                .is_some_and(|keep| keep.token == component.token)
        {
            continue;
        }
        if let Err(error) = runtime::stop(component, name) {
            failed.push(format!("{name}: {error:#}"));
        }
    }
    ensure!(failed.is_empty(), "shutdown failed: {}", failed.join("; "));
    Ok(())
}

fn stop_deployment(store: &Store, operation: &Operation) -> Result<()> {
    phase(store, &operation.id, Phase::Stopping)?;
    let state = store.read()?;
    let deployment = state
        .deployments
        .get(&operation.key)
        .context("missing recorded deployment")?;
    let current = runtime::containers()?;
    let mut retained = BTreeMap::new();
    for key in runtime::active_keys(&state, &current)? {
        if key != operation.key
            && let Some(active) = state.deployments.get(&key)
        {
            retained.extend(active.components.clone());
        }
    }
    stop_group(deployment, &retained)
}

fn launch_and_wait(store: &Store, operation: &Operation, request: &Request) -> Result<()> {
    let deadline = Instant::now() + config::duration(&request.selection.model.readiness.timeout)?;
    let document = serving_document(request)?;
    remove_replaced_containers(store, request)?;
    launch_service(
        store,
        operation,
        request,
        &document,
        &request.selection.entry,
        deadline,
    )?;
    wait_ready(store, operation, request, deadline)?;
    phase(store, &operation.id, Phase::Companions)?;
    for name in &request.selection.model.companions {
        if request.retained.contains_key(name) || request.selection.required.contains(name) {
            continue;
        }
        let deadline = Instant::now()
            + config::duration(&request.selection.model.readiness.timeout)?
                .min(process::INSPECTION_TIMEOUT);
        let result = launch_service(store, operation, request, &document, name, deadline);
        if let Err(error) = result {
            cancelled(store, &operation.id)?;
            let _lock = wait_control(store)?;
            let mut state = store.read()?;
            state
                .deployments
                .get_mut(&operation.key)
                .and_then(|deployment| deployment.components.get_mut(name))
                .context("missing companion record")?
                .error = Some(format!("{error:#}"));
            store.save(&state)?;
        }
    }
    Ok(())
}

fn wait_ready(
    store: &Store,
    operation: &Operation,
    request: &Request,
    deadline: Instant,
) -> Result<()> {
    phase(store, &operation.id, Phase::Waiting)?;
    loop {
        cancelled(store, &operation.id)?;
        let current = runtime::containers()?;
        let model = request
            .deployment
            .components
            .get(&request.selection.entry)
            .context("missing model component")?;
        ensure!(
            runtime::owned(model, &request.selection.entry, &current)?
                .is_some_and(|container| container.running),
            "model exited before readiness"
        );
        let remaining = deadline.saturating_duration_since(Instant::now());
        ensure!(!remaining.is_zero(), "readiness deadline expired");
        if runtime::ready(
            &request.selection.model.readiness.url,
            remaining.min(Duration::from_secs(3)),
        )? {
            return Ok(());
        }
        thread::sleep(remaining.min(Duration::from_millis(200)));
    }
}

fn remove_replaced_containers(store: &Store, request: &Request) -> Result<()> {
    let _lock = wait_control(store)?;
    cancelled(store, &request.deployment.operation)?;
    let current = runtime::containers()?;
    let state = store.read()?;
    let mut removed = std::collections::BTreeSet::new();
    for previous in &request.previous {
        let Some(deployment) = state.deployments.get(previous) else {
            continue;
        };
        for (name, old) in &deployment.components {
            if request.retained.contains_key(name)
                || !request.deployment.components.contains_key(name)
            {
                continue;
            }
            let Some(container) = runtime::owned(old, name, &current)? else {
                continue;
            };
            ensure!(!container.running, "old service {name} has not exited");
            if removed.insert(container.id.clone()) {
                process::output(
                    Command::new("podman").args(["rm", &container.id]),
                    None,
                    process::INSPECTION_TIMEOUT,
                )?;
            }
        }
    }
    Ok(())
}

fn launch_service(
    store: &Store,
    operation: &Operation,
    request: &Request,
    document: &Value,
    name: &str,
    deadline: Instant,
) -> Result<()> {
    let _lock = wait_control(store)?;
    cancelled(store, &operation.id)?;
    let remaining = deadline.saturating_duration_since(Instant::now());
    ensure!(
        !remaining.is_zero(),
        "startup deadline expired before service launch"
    );
    let result = process::compose(
        &request.provider,
        &request.selection.directory,
        document,
        &[],
        &["up", "-d", "--no-build", "--no-recreate", name],
        remaining,
        Some(&request.environment_file),
    );
    let mut state = store.read()?;
    let current = runtime::containers()?;
    let deployment = state
        .deployments
        .get_mut(&operation.key)
        .context("missing deployment")?;
    runtime::refresh(deployment, &current)?;
    let component = deployment
        .components
        .get(name)
        .context("missing component")?
        .clone();
    store.save(&state)?;
    result?;
    ensure!(
        runtime::owned(&component, name, &current)?.is_some_and(|container| container.running),
        "service {name} did not start"
    );
    Ok(())
}

fn serving_document(request: &Request) -> Result<Value> {
    let mut document = request.selection.document.clone();
    let services = document
        .get_mut("services")
        .and_then(Value::as_object_mut)
        .context("missing services")?;
    for (name, service) in services {
        let component = request
            .deployment
            .components
            .get(name)
            .context("missing image record")?;
        config::set(service, "image", Value::String(component.image.clone()))?;
        config::set(service, "pull_policy", Value::String("never".to_owned()))?;
        let object = service.as_object_mut().context("invalid service")?;
        object.remove("build");
        object.remove("profiles");
        let mut labels = match object.remove("labels") {
            Some(Value::Object(labels)) => labels,
            Some(Value::Array(labels)) => labels
                .iter()
                .map(|item| {
                    let value = item.as_str().context("invalid label")?;
                    let (key, value) = value.split_once('=').unwrap_or((value, ""));
                    Ok((key.to_owned(), Value::String(value.to_owned())))
                })
                .collect::<Result<_>>()?,
            None => serde_json::Map::new(),
            _ => bail!("invalid labels"),
        };
        ensure!(
            !labels.keys().any(|key| key.starts_with("io.lmserve.")),
            "io.lmserve labels are reserved"
        );
        labels.insert(
            runtime::TOKEN_LABEL.to_owned(),
            Value::String(component.token.clone()),
        );
        labels.insert(
            runtime::SERVICE_LABEL.to_owned(),
            Value::String(name.clone()),
        );
        labels.insert(
            runtime::MODEL_LABEL.to_owned(),
            Value::String((name == &request.selection.entry).to_string()),
        );
        config::set(service, "labels", Value::Object(labels))?;
    }
    Ok(document)
}
