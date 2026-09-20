use crate::cli::Action;
use crate::config::Selection;
use crate::config::Source;
use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use camino::Utf8Path;
use camino::Utf8PathBuf;
use fs_err::os::unix::fs::OpenOptionsExt;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::Write;
use std::os::unix::fs::DirBuilderExt;
use std::time::SystemTime;

#[derive(Default, Deserialize, Serialize)]
pub(crate) struct State {
    pub(crate) deployments: BTreeMap<String, Deployment>,
    pub(crate) prepared: BTreeMap<String, Prepared>,
    pub(crate) operations: BTreeMap<String, Operation>,
    #[serde(default)]
    pub(crate) images: BTreeMap<String, ImagePreparation>,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct ImagePreparation {
    pub(crate) reference: String,
    pub(crate) identity: String,
    pub(crate) source_revision: Option<String>,
}

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct Prepared {
    pub(crate) source: Source,
    pub(crate) revision: String,
    pub(crate) root: Utf8PathBuf,
    pub(crate) path: Utf8PathBuf,
    pub(crate) owned_directory: Utf8PathBuf,
    pub(crate) files: BTreeMap<String, u64>,
}

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct Deployment {
    pub(crate) project: String,
    pub(crate) entry: String,
    pub(crate) operation: String,
    pub(crate) readiness: String,
    pub(crate) artifact: Prepared,
    pub(crate) components: BTreeMap<String, Component>,
}

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct Component {
    pub(crate) token: String,
    pub(crate) id: Option<String>,
    pub(crate) image: String,
    pub(crate) configuration: String,
    pub(crate) stop_signal: String,
    pub(crate) stop_seconds: u64,
    pub(crate) error: Option<String>,
}

#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Phase {
    Accepted,
    Starting,
    Waiting,
    Companions,
    Stopping,
    Ready,
    Stopped,
    Failed,
    Cancelled,
    Interrupted,
}

impl Phase {
    pub(crate) fn pending(self) -> bool {
        matches!(
            self,
            Self::Accepted | Self::Starting | Self::Waiting | Self::Companions | Self::Stopping
        )
    }
}

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct Operation {
    pub(crate) id: String,
    pub(crate) action: Action,
    pub(crate) key: String,
    pub(crate) phase: Phase,
    pub(crate) worker: Option<ProcessIdentity>,
    pub(crate) error: Option<String>,
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct ProcessIdentity {
    pid: u32,
    start: String,
    boot: String,
}

impl ProcessIdentity {
    pub(crate) fn read(pid: u32) -> Result<Self> {
        let process_stat = fs_err::read_to_string(format!("/proc/{pid}/stat"))?;
        let tail = process_stat
            .rsplit_once(')')
            .context("invalid process stat")?
            .1;
        let fields: Vec<_> = tail.split_whitespace().collect();
        ensure!(fields.first() != Some(&"Z"), "worker has exited");
        let start = fields
            .get(19)
            .context("missing process birth time")?
            .to_string();
        let boot = fs_err::read_to_string("/proc/sys/kernel/random/boot_id")?;
        Ok(Self { pid, start, boot })
    }

    pub(crate) fn alive(&self) -> bool {
        Self::read(self.pid).is_ok_and(|identity| identity == *self)
    }
}

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct Request {
    pub(crate) provider: String,
    pub(crate) environment_file: Utf8PathBuf,
    pub(crate) selection: Selection,
    pub(crate) deployment: Deployment,
    pub(crate) previous: Vec<String>,
    pub(crate) retained: BTreeMap<String, Component>,
}

pub(crate) struct Store {
    pub(crate) directory: Utf8PathBuf,
}

impl Store {
    pub(crate) fn discover() -> Result<Self> {
        Ok(Self {
            directory: xdg("XDG_STATE_HOME", ".local/state")?.join("lmserve"),
        })
    }

    pub(crate) fn initialize(&self) -> Result<()> {
        private_directory(&self.directory)
    }

    pub(crate) fn lock(&self, name: &str) -> Result<File> {
        self.initialize()?;
        let file = fs_err::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(self.directory.join(format!("{name}.lock")))?
            .into_file();
        file.try_lock()
            .context("another operation is busy; retry after it completes")?;
        Ok(file)
    }

    pub(crate) fn read(&self) -> Result<State> {
        match fs_err::read(self.directory.join("state.json")) {
            Ok(data) => serde_json::from_slice(&data)
                .context("invalid control state; refusing to infer ownership"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) fn save(&self, state: &State) -> Result<()> {
        atomic_write(
            &self.directory.join("state.json"),
            &serde_json::to_vec(state)?,
        )
    }

    pub(crate) fn request_path(&self, id: &str) -> Utf8PathBuf {
        self.directory.join(format!("request-{id}.json"))
    }
    pub(crate) fn cancel_path(&self, id: &str) -> Utf8PathBuf {
        self.directory.join(format!("cancel-{id}"))
    }
}

pub(crate) fn private_directory(path: &Utf8Path) -> Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .with_context(|| format!("create private directory {path}"))
}

pub(crate) fn atomic_write(path: &Utf8Path, contents: &[u8]) -> Result<()> {
    let parent = path.parent().context("output path has no parent")?;
    private_directory(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(contents)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).context("publish completed file")?;
    fs_err::File::open(parent)?.sync_all()?;
    Ok(())
}

pub(crate) fn xdg(variable: &str, fallback: &str) -> Result<Utf8PathBuf> {
    let path = match std::env::var(variable) {
        Ok(value) if !value.is_empty() => Utf8PathBuf::from(value),
        _ => Utf8PathBuf::from(
            std::env::var("HOME").context("HOME is required when XDG paths are unset")?,
        )
        .join(fallback),
    };
    ensure!(path.is_absolute(), "{variable} must be absolute");
    Ok(path)
}

pub(crate) fn key(project: &str, entry: &str) -> String {
    format!("{project}/{entry}")
}

pub(crate) fn digest(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .flat_map(|byte| {
            let digits = b"0123456789abcdef";
            [
                char::from(*digits.get(usize::from(byte >> 4)).unwrap_or(&b'0')),
                char::from(*digits.get(usize::from(byte & 15)).unwrap_or(&b'0')),
            ]
        })
        .collect()
}

pub(crate) fn identifier() -> Result<String> {
    let elapsed = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?;
    Ok(digest(
        format!("{}-{}", std::process::id(), elapsed.as_nanos()).as_bytes(),
    ))
}

pub(crate) fn reconcile_workers(state: &mut State) {
    for operation in state.operations.values_mut() {
        if operation.phase.pending()
            && !operation
                .worker
                .as_ref()
                .is_some_and(ProcessIdentity::alive)
        {
            operation.phase = Phase::Interrupted;
            operation.error =
                Some("worker interrupted; inspect recorded containers before recovery".to_owned());
        }
    }
}
