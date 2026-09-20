use crate::process;
use crate::state;
use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use camino::Utf8Path;
use camino::Utf8PathBuf;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Map;
use serde_json::Value;
use std::collections::BTreeSet;
use std::time::Duration;

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Source {
    pub(crate) repo: String,
    pub(crate) revision: Option<String>,
    pub(crate) file: Option<String>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Model {
    pub(crate) huggingface: Source,
    #[serde(default)]
    pub(crate) companions: Vec<String>,
    pub(crate) readiness: Readiness,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Readiness {
    pub(crate) url: String,
    #[serde(default = "default_timeout")]
    pub(crate) timeout: String,
}

fn default_timeout() -> String {
    "900s".to_owned()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct Extension {
    version: u32,
    model_directory: Option<Utf8PathBuf>,
}

pub(crate) struct Project {
    pub(crate) document: Value,
    pub(crate) directory: Utf8PathBuf,
    pub(crate) models: Vec<String>,
}

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct Selection {
    pub(crate) document: Value,
    pub(crate) directory: Utf8PathBuf,
    pub(crate) project: String,
    pub(crate) entry: String,
    pub(crate) root: Utf8PathBuf,
    pub(crate) model: Model,
    pub(crate) services: Vec<String>,
    pub(crate) required: BTreeSet<String>,
    pub(crate) model_path: Utf8PathBuf,
    pub(crate) sources: std::collections::BTreeMap<String, Option<Source>>,
}

pub(crate) fn field<'a>(value: &'a Value, name: &str) -> &'a Value {
    value.get(name).unwrap_or(&Value::Null)
}

pub(crate) fn object(value: &Value) -> Result<&Map<String, Value>> {
    value.as_object().context("expected a mapping")
}

pub(crate) fn set(value: &mut Value, name: &str, item: Value) -> Result<()> {
    value
        .as_object_mut()
        .context("expected a mapping")?
        .insert(name.to_owned(), item);
    Ok(())
}

pub(crate) fn parse(data: &[u8]) -> Result<Value> {
    let mut yaml: serde_yaml_ng::Value = serde_yaml_ng::from_slice(data)
        .map_err(|_redacted_error| anyhow::anyhow!("invalid Compose YAML"))?;
    yaml.apply_merge()
        .map_err(|_redacted_error| anyhow::anyhow!("invalid YAML merge"))?;
    serde_json::to_value(yaml).context("Compose mappings require string keys")
}

pub(crate) fn duration(text: &str) -> Result<Duration> {
    let duration: jiff::SignedDuration = text.parse().context("invalid duration")?;
    let duration: Duration = duration.try_into().context("duration must be positive")?;
    ensure!(
        !duration.is_zero() && std::time::Instant::now().checked_add(duration).is_some(),
        "duration must be positive and fit a monotonic deadline"
    );
    Ok(duration)
}

pub(crate) fn variable(service: &str) -> String {
    format!(
        "LMSERVE_MODEL_{}_PATH",
        service.to_ascii_uppercase().replace(['-', '.'], "_")
    )
}

fn valid_name(name: &str) -> bool {
    name.as_bytes()
        .first()
        .is_some_and(u8::is_ascii_alphanumeric)
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c))
}

impl Project {
    pub(crate) fn load(path: &Utf8Path) -> Result<Self> {
        let path = path.canonicalize_utf8().context("load Compose file")?;
        let directory = path
            .parent()
            .context("Compose file has no directory")?
            .to_owned();
        let document = parse(&fs_err::read(&path)?)?;
        ensure!(
            field(&document, "include").is_null(),
            "provider includes are unsupported; use one Compose file"
        );
        let services = object(field(&document, "services"))?;
        let mut models = Vec::new();
        let mut variables = BTreeSet::new();
        for (name, service) in services {
            ensure!(valid_name(name), "invalid service identifier");
            ensure!(
                field(service, "extends").is_null(),
                "provider extends is unsupported; use YAML anchors"
            );
            if service.get("x-lmserve").is_some() {
                models.push(name.clone());
                ensure!(
                    variables.insert(variable(name)),
                    "model names normalize to the same reserved variable"
                );
            }
        }
        ensure!(!models.is_empty(), "no model services configured");
        for key in &variables {
            ensure!(
                std::env::var_os(key).is_none(),
                "reserved model variable {key} is set in the environment"
            );
        }
        check_env_file(&directory.join(".env"), &variables, true)?;
        Ok(Self {
            document,
            directory,
            models,
        })
    }

    pub(crate) fn select(
        &self,
        entry: &str,
        provider: &str,
        prepared: Option<&Utf8Path>,
    ) -> Result<Selection> {
        ensure!(
            self.models.iter().any(|name| name == entry),
            "unknown model entry {entry}"
        );
        let model_path = prepared.map_or_else(
            || Utf8PathBuf::from(format!("/lmserve-unprepared/{entry}")),
            Utf8Path::to_owned,
        );
        let variables = self
            .models
            .iter()
            .map(|name| {
                (
                    variable(name),
                    if name == entry {
                        model_path.to_string()
                    } else {
                        format!("/lmserve-unprepared/{name}")
                    },
                )
            })
            .collect::<Vec<_>>();
        process::provider_version(provider)?;
        let rendered = process::compose(
            provider,
            &self.directory,
            &self.document,
            &variables,
            &["config"],
            process::INSPECTION_TIMEOUT,
            None,
        )
        .context("provider configuration validation failed")?;
        let mut document = parse(&rendered)?;
        let extension: Extension = serde_json::from_value(field(&document, "x-lmserve").clone())
            .map_err(|_redacted_error| {
                anyhow::anyhow!("invalid project x-lmserve fields or types")
            })?;
        ensure!(extension.version == 1, "unsupported x-lmserve version");
        let project = field(&document, "name")
            .as_str()
            .context("explicit Compose project name is required")?
            .to_owned();
        ensure!(
            valid_name(&project) && project == project.to_ascii_lowercase(),
            "invalid project name"
        );
        let root = extension
            .model_directory
            .unwrap_or(state::xdg("XDG_CACHE_HOME", ".cache")?.join("lmserve/models"));
        ensure!(root.is_absolute(), "model-directory must be absolute");
        let rendered_services = object(field(&document, "services"))?;
        let sources = self
            .models
            .iter()
            .map(|name| {
                (
                    name.clone(),
                    rendered_services
                        .get(name)
                        .and_then(|service| model_metadata(service).ok())
                        .map(|model| model.huggingface),
                )
            })
            .collect();
        let rendered_model = rendered_services
            .get(entry)
            .context("provider removed selected model")?;
        let model = model_metadata(rendered_model)?;
        validate_model(&model)?;
        validate_mount(rendered_model, &model_path)?;
        let (selected, required) = select_services(entry, &model, rendered_services)?;
        let reserved_variables = self.models.iter().map(|name| variable(name)).collect();
        let selected_services = selected
            .iter()
            .map(|name| {
                let service = rendered_services
                    .get(name)
                    .context("missing selected service")?;
                validate_service(name, service)?;
                check_service_environment(service, &self.directory, &reserved_variables)?;
                Ok((name.clone(), service.clone()))
            })
            .collect::<Result<Map<_, _>>>()?;
        set(&mut document, "services", Value::Object(selected_services))?;
        Ok(Selection {
            document,
            directory: self.directory.clone(),
            project,
            entry: entry.to_owned(),
            root,
            model,
            services: selected,
            required,
            model_path,
            sources,
        })
    }
}

fn select_services(
    entry: &str,
    model: &Model,
    services: &Map<String, Value>,
) -> Result<(Vec<String>, BTreeSet<String>)> {
    let mut selected = Vec::new();
    visit(entry, entry, services, &mut BTreeSet::new(), &mut selected)?;
    let required = selected.iter().cloned().collect();
    for companion in &model.companions {
        ensure!(companion != entry, "model cannot be its own companion");
        visit(
            companion,
            entry,
            services,
            &mut BTreeSet::new(),
            &mut selected,
        )?;
    }
    Ok((selected, required))
}

fn model_metadata(service: &Value) -> Result<Model> {
    serde_json::from_value(field(service, "x-lmserve").clone())
        .map_err(|_redacted_error| anyhow::anyhow!("invalid model x-lmserve fields or types"))
}

fn validate_model(model: &Model) -> Result<()> {
    let source = &model.huggingface;
    ensure!(
        !source.repo.is_empty()
            && source.repo.split('/').count() <= 2
            && source.repo.split('/').all(valid_name),
        "invalid Hugging Face repository identifier"
    );
    if let Some(revision) = &source.revision {
        ensure!(
            !revision.is_empty()
                && !revision.starts_with('-')
                && !revision.chars().any(char::is_control),
            "invalid model revision"
        );
    }
    if let Some(file) = &source.file {
        ensure!(
            !file.is_empty()
                && !file.starts_with('-')
                && !file.contains('\\')
                && file
                    .split('/')
                    .all(|part| !part.is_empty() && part != "." && part != "..")
                && !Utf8Path::new(file).is_absolute(),
            "artifact file must be repository-relative without parent traversal"
        );
    }
    let url = reqwest::Url::parse(&model.readiness.url).context("invalid readiness URL")?;
    ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none(),
        "readiness URL must be HTTP(S) without credentials"
    );
    duration(&model.readiness.timeout)?;
    Ok(())
}

fn validate_service(name: &str, service: &Value) -> Result<()> {
    let labels = field(service, "labels");
    let reserved = labels
        .as_object()
        .is_some_and(|mapping| mapping.keys().any(|key| key.starts_with("io.lmserve.")))
        || labels.as_array().is_some_and(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .any(|label| label.starts_with("io.lmserve."))
        });
    ensure!(!reserved, "io.lmserve labels are reserved");
    ensure!(
        field(service, "image")
            .as_str()
            .is_some_and(|s| !s.is_empty())
            || !field(service, "build").is_null(),
        "service {name} requires image or build"
    );
    ensure!(
        field(service, "scale").as_u64().unwrap_or(1) == 1
            && field(field(service, "deploy"), "replicas")
                .as_u64()
                .unwrap_or(1)
                == 1,
        "service {name} must run one replica"
    );
    ensure!(
        service.get("x-lmserve").is_none()
            || field(service, "restart").is_null()
            || field(service, "restart").as_str() == Some("no"),
        "service {name} cannot have an automatic restart policy"
    );
    ensure!(
        service.get("x-lmserve").is_none()
            || field(field(service, "deploy"), "restart_policy").is_null(),
        "service {name} cannot have a deploy restart policy"
    );
    if let Some(grace) = field(service, "stop_grace_period").as_str() {
        duration(grace)?;
    }
    Ok(())
}

fn validate_mount(service: &Value, path: &Utf8Path) -> Result<()> {
    let mounts = field(service, "volumes")
        .as_array()
        .context("model requires a read-only bind mount using its reserved variable")?;
    let mut matching = mounts.iter().filter(|mount| {
        mount.as_str().map_or_else(
            || field(mount, "source").as_str() == Some(path.as_str()),
            |short| short.split(':').next() == Some(path.as_str()),
        )
    });
    let mount = matching
        .next()
        .context("model requires a read-only bind mount using its reserved variable")?;
    ensure!(
        matching.next().is_none(),
        "model path must be mounted exactly once"
    );
    let read_only = mount.as_str().map_or_else(
        || {
            field(mount, "type").as_str() == Some("bind")
                && field(mount, "read_only").as_bool() == Some(true)
        },
        |short| {
            short.split(':').nth(2).is_some_and(|options| {
                options.split(',').any(|item| item == "ro")
                    && !options.split(',').any(|item| item == "rw")
            })
        },
    );
    ensure!(read_only, "model mount must be a read-only bind mount");
    Ok(())
}

fn visit(
    name: &str,
    model: &str,
    services: &Map<String, Value>,
    visiting: &mut BTreeSet<String>,
    ordered: &mut Vec<String>,
) -> Result<()> {
    ensure!(
        !visiting.contains(name),
        "dependency cycle at service {name}"
    );
    if ordered.iter().any(|item| item == name) {
        return Ok(());
    }
    let service = services
        .get(name)
        .with_context(|| format!("missing companion or dependency {name}"))?;
    ensure!(
        name == model || service.get("x-lmserve").is_none(),
        "deployment would start another model {name}"
    );
    visiting.insert(name.to_owned());
    for dependency in dependencies(service)? {
        visit(&dependency, model, services, visiting, ordered)?;
    }
    visiting.remove(name);
    ordered.push(name.to_owned());
    Ok(())
}

fn dependencies(service: &Value) -> Result<BTreeSet<String>> {
    let mut names = BTreeSet::new();
    let dependencies = field(service, "depends_on");
    if let Some(mapping) = dependencies.as_object() {
        names.extend(mapping.keys().cloned());
    } else if let Some(list) = dependencies.as_array() {
        for item in list {
            names.insert(item.as_str().context("invalid dependency name")?.to_owned());
        }
    } else {
        ensure!(dependencies.is_null(), "invalid depends_on");
    }
    for key in ["network_mode", "ipc", "pid"] {
        if let Some(value) = field(service, key)
            .as_str()
            .and_then(|s| s.strip_prefix("service:"))
        {
            names.insert(value.to_owned());
        }
    }
    for key in ["links", "volumes_from"] {
        if let Some(list) = field(service, key).as_array() {
            for item in list {
                let name = item
                    .as_str()
                    .context("invalid service dependency")?
                    .split(':')
                    .next()
                    .context("empty dependency")?;
                ensure!(
                    name != "container",
                    "unowned container dependencies are unsupported"
                );
                names.insert(name.to_owned());
            }
        }
    }
    Ok(names)
}

fn check_env_file(path: &Utf8Path, variables: &BTreeSet<String>, optional: bool) -> Result<()> {
    let contents = match fs_err::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if optional && error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for line in contents.lines() {
        let line = line.trim().strip_prefix("export ").unwrap_or(line.trim());
        let key = line
            .split('=')
            .next()
            .unwrap_or("")
            .trim()
            .trim_matches(['\'', '"']);
        ensure!(
            !variables.contains(key),
            "reserved model variable {key} is set in an environment file"
        );
    }
    Ok(())
}

fn check_service_environment(
    service: &Value,
    directory: &Utf8Path,
    variables: &BTreeSet<String>,
) -> Result<()> {
    let environment = field(service, "environment");
    if let Some(mapping) = environment.as_object() {
        ensure!(
            !mapping.keys().any(|key| variables.contains(key)),
            "reserved model variable in service environment"
        );
    }
    if let Some(list) = environment.as_array() {
        for item in list {
            let key = item.as_str().unwrap_or("").split('=').next().unwrap_or("");
            ensure!(
                !variables.contains(key),
                "reserved model variable in service environment"
            );
        }
    }
    let value = field(service, "env_file");
    let files = value.as_array().cloned().unwrap_or_else(|| {
        if value.is_null() {
            vec![]
        } else {
            vec![value.clone()]
        }
    });
    for file in files {
        let path = file
            .as_str()
            .or_else(|| field(&file, "path").as_str())
            .context("invalid env_file")?;
        check_env_file(
            &directory.join(path),
            variables,
            field(&file, "required").as_bool() == Some(false),
        )?;
    }
    Ok(())
}
