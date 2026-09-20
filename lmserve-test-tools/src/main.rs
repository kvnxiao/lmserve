//! Simulate host tools for isolated lmserve subprocess tests.

mod artifacts;
mod compose;
mod podman;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use camino::Utf8Path;
use camino::Utf8PathBuf;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::io::Write;

#[derive(Default, Deserialize, Serialize)]
struct State {
    containers: Vec<Container>,
    images: BTreeMap<String, String>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "PascalCase")]
struct Container {
    id: String,
    name: String,
    config: ContainerConfig,
    state: ContainerState,
    network_settings: Value,
}

#[derive(Deserialize, Serialize)]
struct ContainerConfig {
    #[serde(rename = "Labels")]
    labels: BTreeMap<String, String>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "PascalCase")]
struct ContainerState {
    running: bool,
    #[serde(default)]
    exit_code: i32,
}

impl Container {
    fn new(name: String, labels: BTreeMap<String, String>, running: bool) -> Result<Self> {
        let owner = labels
            .get("io.lmserve.owner")
            .context("missing owner label")?;
        Ok(Self {
            id: format!("id-{owner}"),
            name,
            config: ContainerConfig { labels },
            state: ContainerState {
                running,
                exit_code: 0,
            },
            network_settings: json!({"Ports": {}}),
        })
    }
}

struct Fixture {
    root: Utf8PathBuf,
    state: State,
}

impl Fixture {
    fn save(&self) -> Result<()> {
        let temporary = self
            .root
            .join(format!("runtime-{}.json", std::process::id()));
        fs_err::write(&temporary, serde_json::to_vec(&self.state)?)?;
        fs_err::rename(temporary, self.root.join("runtime.json"))?;
        Ok(())
    }

    fn image(&mut self, reference: &str) -> Result<()> {
        self.state.images.insert(
            reference.to_owned(),
            format!("sha256:{}", reference.replace(':', "_")),
        );
        self.save()
    }
}

fn field<'a>(value: &'a Value, name: &str) -> &'a Value {
    value.get(name).unwrap_or(&Value::Null)
}

fn text(value: &Value) -> Result<&str> {
    value.as_str().context("expected fixture string")
}

fn after<'a>(args: &'a [&str], flag: &str) -> Result<&'a str> {
    args.windows(2)
        .find_map(|pair| match pair {
            [key, value] if *key == flag => Some(*value),
            _ => None,
        })
        .with_context(|| format!("missing argument {flag}"))
}

fn last<'a>(args: &'a [&str]) -> Result<&'a str> {
    args.last().copied().context("missing final argument")
}

fn env_matches(name: &str, value: &str) -> bool {
    std::env::var(name).is_ok_and(|actual| actual == value)
}

fn emit(value: &str) -> Result<()> {
    writeln!(std::io::stdout().lock(), "{value}")?;
    Ok(())
}

fn emit_json(value: &impl Serialize) -> Result<()> {
    emit(&serde_json::to_string(value)?)
}

fn main() -> Result<()> {
    let mut arguments = std::env::args();
    let program = arguments.next().context("missing executable name")?;
    let tool = Utf8Path::new(&program)
        .file_name()
        .context("missing tool name")?;
    let root =
        Utf8PathBuf::from(std::env::var("LMSERVE_TEST_ROOT").context("missing fixture root")?);
    let args: Vec<String> = arguments.collect();
    let invocation: Vec<&str> = std::iter::once(tool)
        .chain(args.iter().map(String::as_str))
        .collect();
    let mut log = fs_err::OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join("calls.jsonl"))?;
    let mut record = serde_json::to_vec(&invocation)?;
    record.push(b'\n');
    log.write_all(&record)?;
    let state = match fs_err::read(root.join("runtime.json")) {
        Ok(bytes) => serde_json::from_slice(&bytes)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => State::default(),
        Err(error) => return Err(error.into()),
    };
    let mut fixture = Fixture { root, state };
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match tool {
        "podman-compose" => compose::run(&mut fixture, &args),
        "podman" => podman::run(&mut fixture, &args),
        "hf" => artifacts::hf(&args),
        "nvidia-ctk" => artifacts::cdi(&args),
        _ => bail!("unexpected executable {tool}"),
    }
}
