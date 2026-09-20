use crate::config;
use crate::config::Selection;
use crate::process;
use crate::state;
use crate::state::Component;
use crate::state::Deployment;
use crate::state::State;
use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use serde_json::Value;
use std::collections::BTreeMap;
use std::process::Command;
use std::time::Duration;

pub(crate) const TOKEN_LABEL: &str = "io.lmserve.owner";
pub(crate) const MODEL_LABEL: &str = "io.lmserve.model";
pub(crate) const SERVICE_LABEL: &str = "io.lmserve.service";

pub(crate) struct Container {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) running: bool,
    pub(crate) labels: BTreeMap<String, String>,
    pub(crate) ports: Vec<Port>,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Port {
    address: String,
    number: u16,
    protocol: String,
}

pub(crate) fn containers() -> Result<Vec<Container>> {
    let list = process::podman(&["ps", "--all", "--format", "json"])?;
    let list = list.as_array().context("invalid Podman container list")?;
    if list.is_empty() {
        return Ok(Vec::new());
    }
    let mut args = vec!["container", "inspect"];
    for item in list {
        args.push(
            config::field(item, "Id")
                .as_str()
                .or_else(|| config::field(item, "ID").as_str())
                .context("missing container ID")?,
        );
    }
    let inspected = process::podman(&args)?;
    inspected
        .as_array()
        .context("invalid container inspection")?
        .iter()
        .map(parse_container)
        .collect()
}

fn parse_container(value: &Value) -> Result<Container> {
    let labels =
        serde_json::from_value(config::field(config::field(value, "Config"), "Labels").clone())
            .context("invalid container labels")?;
    let mut ports = Vec::new();
    for (target, bindings) in config::field(config::field(value, "NetworkSettings"), "Ports")
        .as_object()
        .into_iter()
        .flatten()
    {
        let protocol = target.split('/').nth(1).unwrap_or("tcp");
        if let Some(bindings) = bindings.as_array() {
            for binding in bindings {
                ports.push(Port {
                    address: config::field(binding, "HostIp")
                        .as_str()
                        .unwrap_or("0.0.0.0")
                        .to_owned(),
                    number: config::field(binding, "HostPort")
                        .as_str()
                        .context("invalid published port")?
                        .parse()?,
                    protocol: protocol.to_owned(),
                });
            }
        }
    }
    Ok(Container {
        id: config::field(value, "Id")
            .as_str()
            .context("missing inspected container ID")?
            .to_owned(),
        name: config::field(value, "Name")
            .as_str()
            .context("missing container name")?
            .trim_start_matches('/')
            .to_owned(),
        running: config::field(config::field(value, "State"), "Running")
            .as_bool()
            .context("missing container state")?,
        labels,
        ports,
    })
}

pub(crate) fn owned<'a>(
    component: &Component,
    service: &str,
    containers: &'a [Container],
) -> Result<Option<&'a Container>> {
    let found: Vec<_> = containers
        .iter()
        .filter(|container| {
            container.labels.get(TOKEN_LABEL) == Some(&component.token)
                && container
                    .labels
                    .get(SERVICE_LABEL)
                    .is_some_and(|label| label == service)
        })
        .collect();
    ensure!(
        found.len() <= 1,
        "multiple containers claim ownership for service {service}"
    );
    if let Some(container) = found.first() {
        ensure!(
            component.id.as_ref().is_none_or(|id| *id == container.id),
            "recorded container identity changed for {service}"
        );
    }
    Ok(found.first().copied())
}

pub(crate) fn active_keys(state: &State, containers: &[Container]) -> Result<Vec<String>> {
    let mut keys = Vec::new();
    for (key, deployment) in &state.deployments {
        if let Some(component) = deployment.components.get(&deployment.entry)
            && owned(component, &deployment.entry, containers)?
                .is_some_and(|container| container.running)
        {
            keys.push(key.clone());
        }
    }
    for container in containers.iter().filter(|c| {
        c.running
            && c.labels
                .get(MODEL_LABEL)
                .is_some_and(|value| value == "true")
    }) {
        ensure!(
            state.deployments.values().any(|deployment| deployment
                .components
                .get(&deployment.entry)
                .is_some_and(
                    |component| container.labels.get(TOKEN_LABEL) == Some(&component.token)
                )),
            "unrecorded managed model is running; refusing to start another model"
        );
    }
    Ok(keys)
}

pub(crate) fn image_identity(reference: &str) -> Result<String> {
    let image = process::podman(&["image", "inspect", reference])
        .context("configured image is not prepared; run update-images")?;
    image
        .as_array()
        .and_then(|values| values.first())
        .and_then(|value| value.get("Id"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .context("missing image identity")
}

pub(crate) fn owned_groups(state: &State, containers: &[Container]) -> Result<Vec<String>> {
    let mut groups = Vec::new();
    for (key, deployment) in &state.deployments {
        for (service, component) in &deployment.components {
            if owned(component, service, containers)?.is_some_and(|container| container.running) {
                groups.push(key.clone());
                break;
            }
        }
    }
    Ok(groups)
}

pub(crate) fn image_reference(project: &str, name: &str, service: &Value) -> String {
    config::field(service, "image")
        .as_str()
        .map_or_else(|| format!("{project}_{name}"), str::to_owned)
}

pub(crate) fn components(selection: &Selection) -> Result<BTreeMap<String, Component>> {
    let mut components = BTreeMap::new();
    for (name, service) in config::object(config::field(&selection.document, "services"))? {
        let image = image_identity(&image_reference(&selection.project, name, service))?;
        let inspected = process::podman(&["image", "inspect", &image])?;
        let image_signal = inspected
            .as_array()
            .and_then(|items| items.first())
            .and_then(|item| item.pointer("/Config/StopSignal"))
            .and_then(Value::as_str)
            .filter(|signal| !signal.is_empty());
        let configuration =
            crate::inputs::identity(service, &selection.document, &selection.directory)?;
        let stop_seconds = config::duration(
            config::field(service, "stop_grace_period")
                .as_str()
                .unwrap_or("10s"),
        )?
        .as_secs()
        .max(1);
        components.insert(
            name.clone(),
            Component {
                token: state::identifier()?,
                id: None,
                image,
                configuration,
                stop_signal: config::field(service, "stop_signal")
                    .as_str()
                    .or(image_signal)
                    .unwrap_or("SIGTERM")
                    .to_owned(),
                stop_seconds,
                error: None,
            },
        );
    }
    Ok(components)
}

pub(crate) fn prerequisites(selection: &Selection) -> Result<()> {
    let info = process::podman(&["info", "--format", "json"])?;
    ensure!(
        config::field(
            config::field(config::field(&info, "host"), "security"),
            "rootless"
        )
        .as_bool()
            == Some(true),
        "rootless Podman is required"
    );
    let version = process::output(
        Command::new("podman").arg("--version"),
        None,
        process::INSPECTION_TIMEOUT,
    )?;
    let version = std::str::from_utf8(&version)?
        .trim()
        .strip_prefix("podman version ")
        .context("cannot determine Podman version")?;
    let mut parts = version.split('.');
    let major: u32 = parts
        .next()
        .context("missing Podman major version")?
        .parse()?;
    let minor: u32 = parts
        .next()
        .context("missing Podman minor version")?
        .parse()?;
    ensure!((major, minor) >= (4, 6), "Podman 4.6 or newer is required");
    external_resources(selection)?;
    let services = config::object(config::field(&selection.document, "services"))?;
    let devices = services
        .values()
        .filter_map(|service| config::field(service, "devices").as_array())
        .flatten()
        .filter_map(Value::as_str)
        .filter(|device| device.starts_with("nvidia.com/"))
        .collect::<Vec<_>>();
    if !devices.is_empty() {
        let output = process::output(
            Command::new("nvidia-ctk").args(["cdi", "list"]),
            None,
            process::INSPECTION_TIMEOUT,
        )
        .context("NVIDIA CDI inspection failed; generate CDI explicitly with lmserve cdi")?;
        let available = String::from_utf8_lossy(&output);
        for device in devices {
            ensure!(
                available.lines().any(|line| line.trim() == device),
                "configured NVIDIA CDI device is unavailable"
            );
        }
    }
    Ok(())
}

fn external_resources(selection: &Selection) -> Result<()> {
    let services = config::object(config::field(&selection.document, "services"))?;
    for (kind, resource) in [
        ("volumes", "volume"),
        ("networks", "network"),
        ("secrets", "secret"),
    ] {
        let mut referenced = std::collections::BTreeSet::new();
        for service in services.values() {
            let entries = config::field(service, kind);
            if let Some(mapping) = entries.as_object() {
                referenced.extend(mapping.keys().map(String::as_str));
                continue;
            }
            if entries.is_null()
                && kind == "networks"
                && config::field(service, "network_mode").is_null()
            {
                referenced.insert("default");
            }
            referenced.extend(
                entries
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|entry| {
                        entry
                            .as_str()
                            .or_else(|| config::field(entry, "source").as_str())
                            .map(|name| name.split(':').next().unwrap_or(name))
                    }),
            );
        }
        for name in referenced {
            let definition = config::field(config::field(&selection.document, kind), name);
            let external = config::field(definition, "external");
            if external.as_bool() != Some(true) && !external.is_object() {
                continue;
            }
            let resolved = config::field(definition, "name")
                .as_str()
                .or_else(|| config::field(external, "name").as_str())
                .unwrap_or(name);
            process::output(
                Command::new("podman").args([resource, "inspect", resolved]),
                None,
                process::INSPECTION_TIMEOUT,
            )
            .with_context(|| format!("required external {resource} {resolved} is unavailable"))?;
        }
    }
    Ok(())
}

pub(crate) fn conflicts(
    selection: &Selection,
    state: &State,
    previous: &[String],
    containers: &[Container],
) -> Result<()> {
    let mut selected_ports: Vec<Port> = Vec::new();
    let replacing: Vec<_> = previous
        .iter()
        .filter_map(|key| state.deployments.get(key))
        .flat_map(|deployment| deployment.components.values())
        .map(|component| &component.token)
        .collect();
    let services = config::object(config::field(&selection.document, "services"))?;
    for (name, service) in services {
        let container_name = config::field(service, "container_name")
            .as_str()
            .map_or_else(|| format!("{}_{name}_1", selection.project), str::to_owned);
        for container in containers
            .iter()
            .filter(|container| container.name == container_name)
        {
            let recognized = state
                .deployments
                .values()
                .filter(|deployment| deployment.project == selection.project)
                .filter_map(|deployment| deployment.components.get(name))
                .any(|component| container.labels.get(TOKEN_LABEL) == Some(&component.token));
            ensure!(
                recognized,
                "unowned container occupies the name required by service {name}"
            );
        }
        for port in service_ports(service)? {
            ensure!(
                !selected_ports.iter().any(|other| overlaps(&port, other)),
                "selected services publish conflicting ports"
            );
            for container in containers.iter().filter(|c| c.running) {
                let replaceable = container
                    .labels
                    .get(TOKEN_LABEL)
                    .is_some_and(|token| replacing.contains(&token));
                ensure!(
                    replaceable || !container.ports.iter().any(|other| overlaps(&port, other)),
                    "runtime port conflict for service {name}"
                );
            }
            if !containers.iter().any(|container| {
                container.running && container.ports.iter().any(|other| overlaps(&port, other))
            }) {
                probe_port(&port)
                    .with_context(|| format!("host port conflict for service {name}"))?;
            }
            selected_ports.push(port);
        }
    }
    Ok(())
}

fn probe_port(port: &Port) -> Result<()> {
    let address: std::net::IpAddr = port
        .address
        .parse()
        .context("published host address must be an IP address")?;
    let socket = std::net::SocketAddr::new(address, port.number);
    match port.protocol.as_str() {
        "tcp" => {
            std::net::TcpListener::bind(socket).context("TCP port is unavailable")?;
        }
        "udp" => {
            std::net::UdpSocket::bind(socket).context("UDP port is unavailable")?;
        }
        _ => anyhow::bail!("unsupported published port protocol"),
    }
    Ok(())
}

fn overlaps(left: &Port, right: &Port) -> bool {
    left.number == right.number
        && left.protocol == right.protocol
        && (left.address == right.address
            || ["", "0.0.0.0", "::"].contains(&left.address.as_str())
            || ["", "0.0.0.0", "::"].contains(&right.address.as_str()))
}

fn service_ports(service: &Value) -> Result<Vec<Port>> {
    let mut result = Vec::new();
    for port in config::field(service, "ports")
        .as_array()
        .into_iter()
        .flatten()
    {
        let (address, published, protocol) = if let Some(short) = port.as_str() {
            let (binding, protocol) = short.split_once('/').unwrap_or((short, "tcp"));
            let fields: Vec<_> = binding.rsplitn(3, ':').collect();
            let Some(published) = fields.get(1) else {
                continue;
            };
            (
                fields
                    .get(2)
                    .copied()
                    .unwrap_or("0.0.0.0")
                    .trim_matches(['[', ']'])
                    .to_owned(),
                (*published).to_owned(),
                protocol.to_owned(),
            )
        } else {
            let value = config::field(port, "published");
            if value.is_null() {
                continue;
            }
            (
                config::field(port, "host_ip")
                    .as_str()
                    .unwrap_or("0.0.0.0")
                    .to_owned(),
                value
                    .as_str()
                    .map_or_else(|| value.to_string(), str::to_owned),
                config::field(port, "protocol")
                    .as_str()
                    .unwrap_or("tcp")
                    .to_owned(),
            )
        };
        let (start, end) = published
            .split_once('-')
            .unwrap_or((&published, &published));
        let start: u16 = start.parse().context("invalid published port")?;
        let end: u16 = end.parse().context("invalid published port range")?;
        ensure!(start <= end, "invalid published port range");
        for number in start..=end {
            if number != 0 {
                result.push(Port {
                    address: address.clone(),
                    number,
                    protocol: protocol.clone(),
                });
            }
        }
    }
    Ok(result)
}

pub(crate) fn stop(component: &Component, service: &str) -> Result<()> {
    let current = containers()?;
    if let Some(container) = owned(component, service, &current)?.filter(|c| c.running) {
        process::output(
            Command::new("podman").args([
                "stop",
                "--time",
                &component.stop_seconds.to_string(),
                &container.id,
            ]),
            None,
            Duration::from_secs(component.stop_seconds.saturating_add(30)),
        )?;
        let current = containers()?;
        ensure!(
            !owned(component, service, &current)?.is_some_and(|c| c.running),
            "service {service} did not exit; refusing replacement"
        );
    }
    Ok(())
}

pub(crate) fn ready(url: &str, timeout: Duration) -> Result<bool> {
    let client = reqwest::blocking::Client::builder()
        .timeout(timeout)
        .connect_timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    match client.get(url).send() {
        Ok(response) => Ok(response.status().is_success()),
        Err(error) if error.is_timeout() || error.is_connect() || error.is_request() => Ok(false),
        Err(error) => Err(error.without_url()).context("readiness request failed"),
    }
}

pub(crate) fn refresh(deployment: &mut Deployment, current: &[Container]) -> Result<()> {
    for (name, component) in &mut deployment.components {
        if let Some(container) = owned(component, name, current)? {
            component.id = Some(container.id.clone());
        }
    }
    Ok(())
}

pub(crate) fn model_health(deployment: &Deployment, current: &[Container]) -> Result<&'static str> {
    let model = deployment
        .components
        .get(&deployment.entry)
        .context("missing model record")?;
    let Some(container) = owned(model, &deployment.entry, current)? else {
        return Ok("absent");
    };
    if !container.running {
        return Ok("exited");
    }
    if ready(&deployment.readiness, Duration::from_secs(3))? {
        Ok("ready")
    } else {
        Ok("unready")
    }
}
