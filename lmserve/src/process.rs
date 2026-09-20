use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use camino::Utf8Path;
use serde_json::Value;
use std::io::Read;
use std::io::Seek;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::process::Stdio;
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;
use std::time::Instant;

pub(crate) const INSPECTION_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const PREPARATION_TIMEOUT: Duration = Duration::from_hours(24);
static EXECUTION_LEASE: OnceLock<std::fs::File> = OnceLock::new();
static CANCELLATION: OnceLock<camino::Utf8PathBuf> = OnceLock::new();

pub(crate) fn inherit_execution_lease(file: &std::fs::File, cancellation: &Utf8Path) -> Result<()> {
    CANCELLATION
        .set(cancellation.to_owned())
        .map_err(|_duplicate| anyhow::anyhow!("worker cancellation already set"))?;
    EXECUTION_LEASE
        .set(file.try_clone()?)
        .map_err(|_duplicate| anyhow::anyhow!("worker execution lease already set"))
}

pub(crate) fn output(
    command: &mut Command,
    input: Option<&[u8]>,
    timeout: Duration,
) -> Result<Vec<u8>> {
    execute(command, input, timeout, None)
}

fn execute(
    command: &mut Command,
    input: Option<&[u8]>,
    timeout: Duration,
    cancellation: Option<&Utf8Path>,
) -> Result<Vec<u8>> {
    let tool = command.get_program().to_string_lossy().into_owned();
    let mut stdin = tempfile::tempfile()?;
    stdin.write_all(input.unwrap_or_default())?;
    stdin.rewind()?;
    let mut stdout = tempfile::tempfile()?;
    let mut stderr = EXECUTION_LEASE
        .get()
        .map_or_else(tempfile::tempfile, std::fs::File::try_clone)?;
    stderr.set_len(0)?;
    stderr.rewind()?;
    let mut child = command
        .process_group(0)
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::from(stdout.try_clone()?))
        .stderr(Stdio::from(stderr.try_clone()?))
        .spawn()
        .with_context(|| format!("cannot execute {tool}; check that the tool is installed"))?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        let cancelled = cancellation.is_some_and(Utf8Path::exists);
        if cancelled || Instant::now() >= deadline {
            nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(i32::try_from(child.id())?),
                nix::sys::signal::Signal::SIGKILL,
            )
            .context("terminate timed-out subprocess group")?;
            child.wait().context("reap timed-out subprocess")?;
            if cancelled {
                bail!("provider startup cancelled");
            }
            bail!("{tool} exceeded its command deadline");
        }
        thread::sleep(Duration::from_millis(20));
    };
    stdout.rewind()?;
    stderr.rewind()?;
    let mut captured = Vec::new();
    stdout.take(16 * 1024 * 1024).read_to_end(&mut captured)?;
    if !status.success() {
        let mut diagnostic = String::new();
        stderr.take(1024 * 1024).read_to_string(&mut diagnostic)?;
        let diagnostic = diagnostic.to_ascii_lowercase();
        let category = if diagnostic.contains("no space left") {
            "insufficient storage"
        } else if diagnostic.contains("unauthorized")
            || diagnostic.contains("401")
            || diagnostic.contains("403")
        {
            "authentication failed"
        } else if diagnostic.contains("address already in use")
            || diagnostic.contains("already exists")
        {
            "resource conflict"
        } else {
            "tool operation failed"
        };
        bail!(
            "{tool}: {category} ({status}); raw tool output is omitted because it may contain credentials"
        );
    }
    Ok(captured)
}

pub(crate) fn podman(args: &[&str]) -> Result<Value> {
    let output = output(Command::new("podman").args(args), None, INSPECTION_TIMEOUT)
        .context("runtime inspection unavailable")?;
    serde_json::from_slice(&output).context("invalid Podman inspection response")
}

pub(crate) fn compose(
    provider: &str,
    directory: &Utf8Path,
    document: &Value,
    variables: &[(String, String)],
    args: &[&str],
    timeout: Duration,
    environment_file: Option<&Utf8Path>,
) -> Result<Vec<u8>> {
    let mut document = document.clone();
    if args.first() != Some(&"config") {
        escape_interpolation(&mut document);
    }
    let mut command = Command::new(provider);
    command
        .current_dir(directory)
        .env_remove("COMPOSE_PROJECT_DIR")
        .env_remove("COMPOSE_FILE")
        .env_remove("COMPOSE_PROJECT_NAME")
        .env_remove("COMPOSE_PROFILES")
        .envs(variables.iter().map(|(key, value)| (key, value)))
        .args(["--in-pod", "false", "-f", "-"]);
    if let Some(file) = environment_file {
        command.args(["--env-file", file.as_str()]);
    }
    command.args(args);
    let cancellation = if args.first() == Some(&"up") {
        CANCELLATION.get().map(camino::Utf8PathBuf::as_path)
    } else {
        None
    };
    execute(
        &mut command,
        Some(&serde_json::to_vec(&document)?),
        timeout,
        cancellation,
    )
}

fn escape_interpolation(value: &mut Value) {
    match value {
        Value::String(text) => *text = text.replace('$', "$$"),
        Value::Array(values) => values.iter_mut().for_each(escape_interpolation),
        Value::Object(values) => values.values_mut().for_each(escape_interpolation),
        _ => (),
    }
}

pub(crate) fn provider_version(provider: &str) -> Result<()> {
    const MINIMUM_VERSION: [u64; 3] = [1, 5, 0];

    let bytes = output(
        Command::new(provider).arg("--version"),
        None,
        INSPECTION_TIMEOUT,
    )?;
    let version = String::from_utf8_lossy(&bytes);
    anyhow::ensure!(
        version
            .lines()
            .filter_map(|line| line.trim().strip_prefix("podman-compose version "))
            .any(|version| {
                let mut parts = version.split('.');
                let mut parsed = [0_u64; 3];
                for component in &mut parsed {
                    let Some(part) = parts.next() else {
                        return false;
                    };
                    if !part.bytes().all(|byte| byte.is_ascii_digit()) {
                        return false;
                    }
                    let Ok(value) = part.parse::<u64>() else {
                        return false;
                    };
                    *component = value;
                }
                parts.next().is_none() && parsed >= MINIMUM_VERSION
            }),
        "unsupported provider: select podman-compose {}.{}.{} or newer explicitly",
        MINIMUM_VERSION[0],
        MINIMUM_VERSION[1],
        MINIMUM_VERSION[2]
    );
    Ok(())
}
