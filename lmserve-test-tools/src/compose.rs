use crate::Container;
use crate::Fixture;
use crate::emit;
use crate::emit_json;
use crate::env_matches;
use crate::field;
use crate::last;
use crate::text;
use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use serde_json::Value;
use std::time::Duration;
use std::time::Instant;

pub(super) fn run(fixture: &mut Fixture, args: &[&str]) -> Result<()> {
    if args.first() == Some(&"--version") {
        return emit(&format!(
            "podman-compose version {}",
            std::env::var("FAKE_PROVIDER_VERSION").unwrap_or_else(|_| "1.5.0".to_owned())
        ));
    }
    let mut document: Value = serde_json::from_reader(std::io::stdin().lock())?;
    interpolate(&mut document);
    let action = args
        .iter()
        .skip(4)
        .find(|arg| ["config", "pull", "build", "up"].contains(arg))
        .context("unsupported fake compose invocation")?;
    if *action == "config" {
        return emit_json(&document);
    }
    let service = last(args)?;
    if *action == "up" {
        ensure!(
            !args.contains(&"--no-deps")
                && args.contains(&"--no-build")
                && args.contains(&"--no-recreate"),
            "invalid startup flags"
        );
        return create_service(fixture, &document, service);
    }
    let definition = field(field(&document, "services"), service);
    let image = match field(definition, "image").as_str() {
        Some(image) => image.to_owned(),
        None => format!("{}_{service}", text(field(&document, "name"))?),
    };
    fixture.image(&image)
}

fn interpolate(value: &mut Value) {
    match value {
        Value::String(value) => *value = interpolate_string(value),
        Value::Array(values) => values.iter_mut().for_each(interpolate),
        Value::Object(values) => values.values_mut().for_each(interpolate),
        _ => (),
    }
}

fn interpolate_string(input: &str) -> String {
    let mut rest = input;
    let mut output = String::new();
    while let Some(index) = rest.find('$') {
        let (prefix, tail) = rest.split_at(index);
        output.push_str(prefix);
        rest = tail.strip_prefix('$').unwrap_or(tail);
        if let Some(tail) = rest.strip_prefix('$') {
            output.push('$');
            rest = tail;
            continue;
        }
        if let Some((name, tail)) = rest.strip_prefix('{').and_then(|tail| tail.split_once('}')) {
            let valid = name.chars().enumerate().all(|(index, character)| {
                character == '_'
                    || character.is_ascii_alphabetic()
                    || (index > 0 && character.is_ascii_digit())
            });
            if !name.is_empty() && valid {
                output.push_str(&std::env::var(name).unwrap_or_default());
                rest = tail;
                continue;
            }
        }
        output.push('$');
    }
    output.push_str(rest);
    output
}

fn create_service(fixture: &mut Fixture, document: &Value, service: &str) -> Result<()> {
    let definition = field(field(document, "services"), service);
    let dependencies = field(definition, "depends_on");
    let dependencies: Vec<&str> = match dependencies {
        Value::Object(values) => values.keys().map(String::as_str).collect(),
        Value::Array(values) => values.iter().map(text).collect::<Result<_>>()?,
        _ => Vec::new(),
    };
    for dependency in dependencies {
        create_service(fixture, document, dependency)?;
    }
    ensure!(
        field(definition, "pull_policy") == "never" && definition.get("build").is_none(),
        "startup must use prepared images"
    );
    ensure!(
        !env_matches("FAKE_FAIL_SERVICE", service),
        "service failed: password=must-not-leak"
    );
    let name = match field(definition, "container_name").as_str() {
        Some(name) => name.to_owned(),
        None => format!("{}_{service}_1", text(field(document, "name"))?),
    };
    if fixture
        .state
        .containers
        .iter()
        .any(|container| container.name == name)
    {
        return Ok(());
    }
    let labels = serde_json::from_value(field(definition, "labels").clone())?;
    fixture.state.containers.push(Container::new(
        name,
        labels,
        !env_matches("FAKE_COMPLETED_SERVICE", service),
    )?);
    fixture.save()?;
    fs_err::write(fixture.root.join(format!("started-{service}")), "started")?;
    fs_err::write(
        fixture.root.join(format!("definition-{service}.json")),
        serde_json::to_vec(definition)?,
    )?;
    if env_matches("FAKE_BLOCK_SERVICE", service) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !fixture.root.join("release").exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    Ok(())
}
