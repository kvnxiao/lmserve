use crate::config;
use crate::config::Selection;
use crate::state;
use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use camino::Utf8Path;
use camino::Utf8PathBuf;
use serde_json::Value;

pub(crate) fn identity(service: &Value, document: &Value, directory: &Utf8Path) -> Result<String> {
    let mut files = Vec::new();
    for path in paths(service, document, directory)? {
        files.push((path.clone(), fs_err::read(path)?));
    }
    let dotenv = directory.join(".env");
    if dotenv.exists() {
        files.push((dotenv.clone(), fs_err::read(dotenv)?));
    }
    Ok(state::digest(&serde_json::to_vec(&(service, files))?))
}

fn paths(service: &Value, document: &Value, directory: &Utf8Path) -> Result<Vec<Utf8PathBuf>> {
    let mut paths = Vec::new();
    for item in entries(config::field(service, "env_file")) {
        let name = item
            .as_str()
            .or_else(|| config::field(&item, "path").as_str())
            .context("invalid env_file path")?;
        let path = directory.join(name);
        if config::field(&item, "required").as_bool() != Some(false) || path.exists() {
            paths.push(path);
        }
    }
    for kind in ["configs", "secrets"] {
        for item in entries(config::field(service, kind)) {
            let name = item
                .as_str()
                .or_else(|| config::field(&item, "source").as_str())
                .context("invalid configuration source")?;
            if let Some(path) =
                config::field(config::field(config::field(document, kind), name), "file").as_str()
            {
                paths.push(directory.join(path));
            }
        }
    }
    Ok(paths)
}

fn entries(value: &Value) -> Vec<Value> {
    value.as_array().cloned().unwrap_or_else(|| {
        if value.is_null() {
            vec![]
        } else {
            vec![value.clone()]
        }
    })
}

pub(crate) fn validate(selection: &Selection) -> Result<()> {
    for service in config::object(config::field(&selection.document, "services"))?.values() {
        for path in paths(service, &selection.document, &selection.directory)? {
            fs_err::File::open(path)?;
        }
        for mount in entries(config::field(service, "volumes")) {
            let source = if let Some(text) = mount.as_str() {
                text.split(':')
                    .next()
                    .filter(|source| source.starts_with('/') || source.starts_with('.'))
            } else if config::field(&mount, "type").as_str() == Some("bind") {
                config::field(&mount, "source").as_str()
            } else {
                None
            };
            if let Some(source) = source {
                ensure!(
                    selection.directory.join(source).exists(),
                    "required bind-mount source is missing"
                );
            }
        }
    }
    Ok(())
}

pub(crate) fn capture(selection: &mut Selection, destination: &Utf8Path) -> Result<Utf8PathBuf> {
    state::private_directory(destination)?;
    let dotenv = destination.join("dotenv");
    let content = match fs_err::read(selection.directory.join(".env")) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error.into()),
    };
    state::atomic_write(&dotenv, &content)?;
    let services = selection
        .document
        .get_mut("services")
        .and_then(Value::as_object_mut)
        .context("missing services")?;
    for service in services.values_mut() {
        let mut files = Vec::new();
        for item in entries(config::field(service, "env_file")) {
            let name = item
                .as_str()
                .or_else(|| config::field(&item, "path").as_str())
                .context("invalid env_file path")?;
            let path = selection.directory.join(name);
            if config::field(&item, "required").as_bool() == Some(false) && !path.exists() {
                continue;
            }
            files.push(Value::String(copy(&path, destination)?.to_string()));
        }
        if service.get("env_file").is_some() {
            config::set(service, "env_file", Value::Array(files))?;
        }
    }
    for kind in ["configs", "secrets"] {
        let Some(definitions) = selection
            .document
            .get_mut(kind)
            .and_then(Value::as_object_mut)
        else {
            continue;
        };
        for definition in definitions.values_mut() {
            if let Some(path) = config::field(definition, "file").as_str() {
                let captured = copy(&selection.directory.join(path), destination)?;
                config::set(definition, "file", Value::String(captured.to_string()))?;
            }
        }
    }
    Ok(dotenv)
}

fn copy(path: &Utf8Path, directory: &Utf8Path) -> Result<Utf8PathBuf> {
    let destination = directory.join(state::digest(path.as_str().as_bytes()));
    state::atomic_write(&destination, &fs_err::read(path)?)?;
    Ok(destination)
}
