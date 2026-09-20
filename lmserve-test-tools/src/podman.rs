use crate::Container;
use crate::Fixture;
use crate::after;
use crate::emit;
use crate::emit_json;
use crate::env_matches;
use crate::last;
use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use anyhow::ensure;
use serde_json::json;
use std::collections::BTreeMap;

pub(super) fn run(fixture: &mut Fixture, args: &[&str]) -> Result<()> {
    match args {
        ["--version", ..] => emit(&format!(
            "podman version {}",
            std::env::var("FAKE_PODMAN_VERSION").unwrap_or_else(|_| "5.5.0".to_owned())
        )),
        ["ps", ..] => list(fixture, args),
        ["network", "exists", ..] => Ok(()),
        ["volume" | "network" | "secret", "inspect", ..] => inspect_external(fixture, args),
        ["pull", ..] => fixture.image(last(args)?),
        ["build", ..] => fixture.image(after(args, "-t")?),
        ["create", ..] => create(fixture, args),
        ["start", ..] => {
            for container in &mut fixture.state.containers {
                if container.name == last(args)? {
                    container.state.running = true;
                }
            }
            fixture.save()
        }
        ["wait", ..] => emit("0"),
        ["container", "inspect", ids @ ..] => emit_json(
            &fixture
                .state
                .containers
                .iter()
                .filter(|container| ids.contains(&container.id.as_str()))
                .collect::<Vec<_>>(),
        ),
        ["image", "inspect", reference, ..] => {
            let identity = fixture
                .state
                .images
                .get(*reference)
                .map(String::as_str)
                .or_else(|| {
                    fixture
                        .state
                        .images
                        .values()
                        .any(|identity| identity == reference)
                        .then_some(*reference)
                })
                .context("missing image")?;
            emit_json(&json!([{"Id": identity}]))
        }
        ["info", ..] => emit_json(&json!({"host": {"security": {"rootless": true}}})),
        ["stop", ..] => {
            ensure!(std::env::var_os("FAKE_STOP_FAIL").is_none(), "stop failed");
            for container in &mut fixture.state.containers {
                if container.id == last(args)? {
                    container.state.running = false;
                }
            }
            fixture.save()
        }
        ["rm", ..] => {
            let id = last(args)?;
            fixture
                .state
                .containers
                .retain(|container| container.id != id);
            fixture.save()
        }
        ["logs", ..] => emit("fixture model log"),
        _ => bail!("unsupported fake runtime invocation"),
    }
}

fn list(fixture: &Fixture, args: &[&str]) -> Result<()> {
    let label = if args.contains(&"--filter") {
        Some(
            after(args, "--filter")?
                .trim_start_matches("label=")
                .split_once('=')
                .context("invalid label filter")?,
        )
    } else {
        None
    };
    let mut containers = Vec::new();
    for container in &fixture.state.containers {
        if label.is_some_and(|(key, value)| {
            container
                .config
                .labels
                .get(key)
                .is_none_or(|actual| actual != value)
        }) {
            continue;
        }
        let mut value = serde_json::to_value(container)?;
        let object = value
            .as_object_mut()
            .context("container must be an object")?;
        object.insert("Names".to_owned(), json!([container.name]));
        object.insert(
            "Labels".to_owned(),
            serde_json::to_value(&container.config.labels)?,
        );
        containers.push(value);
    }
    emit_json(&containers)
}

fn inspect_external(fixture: &Fixture, args: &[&str]) -> Result<()> {
    let name = last(args)?;
    let calls = fs_err::read_to_string(fixture.root.join("calls.jsonl"))?;
    let mut inspections = 0;
    for line in calls.lines() {
        let call: Vec<String> = serde_json::from_str(line)?;
        if call
            .iter()
            .skip(1)
            .map(String::as_str)
            .eq(args.iter().copied())
        {
            inspections += 1;
        }
    }
    ensure!(
        !(env_matches("FAKE_MISSING_EXTERNAL", name)
            || env_matches("FAKE_EXTERNAL_DISAPPEARS", name) && inspections > 1),
        "external resource missing"
    );
    emit_json(&json!([{"Name": name}]))
}

fn create(fixture: &mut Fixture, args: &[&str]) -> Result<()> {
    let name = args
        .iter()
        .find_map(|arg| arg.strip_prefix("--name="))
        .context("missing container name")?;
    let mut labels = BTreeMap::new();
    for pair in args.windows(2) {
        if let ["--label", label] = pair {
            let (key, value) = label.split_once('=').context("invalid label")?;
            labels.insert(key.to_owned(), value.to_owned());
        }
    }
    let pull_policy = args
        .iter()
        .find_map(|arg| arg.strip_prefix("--pull="))
        .map_or_else(|| after(args, "--pull"), Ok)?;
    ensure!(pull_policy == "never", "startup cannot pull images");
    ensure!(
        !fixture
            .state
            .containers
            .iter()
            .any(|container| container.name == name),
        "container already exists"
    );
    let container = Container::new(name.to_owned(), labels, false)?;
    let id = container.id.clone();
    fixture.state.containers.push(container);
    fixture.save()?;
    emit(&id)
}
