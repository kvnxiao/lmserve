# Configuration and commands

Use one Compose file with a stable, explicit lowercase project `name`. A service
with `x-lmserve` metadata is a model entry, addressed by its service name. Ordinary
Compose fields define images, builds, commands, devices, ports, storage, and
dependencies. `podman-compose` 1.5.0 renders those fields; `lmserve` validates its
extension metadata and selected deployment.

## Extension schema

Project-level `x-lmserve` accepts:

| Field | Contract |
| --- | --- |
| `version` | Required integer `1` |
| `model-directory` | Optional absolute path after interpolation; defaults to `$XDG_CACHE_HOME/lmserve/models`, or `~/.cache/lmserve/models` |

Model service `x-lmserve` accepts:

| Field | Contract |
| --- | --- |
| `huggingface.repo` | Required repository identifier |
| `huggingface.revision` | Optional branch, tag, or full commit; omitted selects the repository default |
| `huggingface.file` | Optional repository-relative file; omitted downloads a snapshot |
| `companions` | Optional array of non-model service names; defaults to empty |
| `readiness.url` | Required host-reachable HTTP(S) URL without embedded credentials |
| `readiness.timeout` | Positive duration, for example `900s`; defaults to `900s` |

Unknown extension fields and unsupported schema versions are errors. Model files
cannot use absolute paths or parent traversal. Required dependencies remain
required when also listed as companions.

For each model, the CLI supplies `LMSERVE_MODEL_<NORMALIZED_SERVICE>_PATH`.
Normalization uppercases ASCII and replaces `-` and `.` with `_`; conflicting
names are rejected. Mount the corresponding variable exactly once as a read-only
bind mount. The selected path is a prepared snapshot directory or file. Configure
the engine to load that container path offline. Do not define reserved variables
in the host environment, `.env`, service environment, or service environment files.

Inactive models may remain unprepared. Validation and preparation use placeholder
paths where necessary; startup requires the selected model's real prepared path.
Snapshots expose regular files within the mount; single-file artifacts resolve
downloaded snapshot links before publication.

## Commands

| Command | Behavior |
| --- | --- |
| `list` | List configured entries and inspect recorded deployments |
| `validate [ENTRY]` | Validate one entry or all entries through the selected provider |
| `plan ACTION ENTRY` | Preview `start`, `stop`, `restart`, `switch`, `update-images`, or `update-models` |
| `plan update-images --all` / `plan update-models --all` | Preview all preparation targets |
| `update-images ENTRY` / `update-images --all` | Pull or build selected images; record their local identities |
| `update-models ENTRY` / `update-models --all` | Resolve, download, verify, and publish artifacts; clean superseded owned content |
| `start ENTRY` | Accept startup, or report an already active entry's operation |
| `stop ENTRY` | Accept recorded group shutdown or cancel startup |
| `restart ENTRY` | Replace the entry's serving session from current configuration |
| `switch ENTRY` | Stop the active model before starting the selected entry |
| `status [ENTRY]` | Inspect components, current model health, and operation outcomes |
| `logs ENTRY [--service SERVICE] [--follow]` | Read logs from a recorded group member |
| `health ENTRY` | Succeed only when the recorded model runs and its endpoint returns HTTP success |
| `cdi [--output PATH]` | Generate CDI through `nvidia-ctk` and atomically publish the output |

`validate` and `plan` do not prepare dependencies, create containers, or write
managed state. Validation requires the provider but does not require a GPU. Plans
inspect local state and identify unresolved remote changes, unavailable runtime
inspection, affected services, replacements, retained companions, and conditional
cache deletions. A plan does not reserve resources or guarantee later admission.

CDI output defaults to `$XDG_CONFIG_HOME/cdi/nvidia.yaml`, or
`~/.config/cdi/nvidia.yaml`. Configure rootless Podman to read the selected CDI
directory. Startup checks configured NVIDIA CDI devices without generating them.

## Supported configuration boundaries

The CLI accepts one Compose file; `include` and `extends` are rejected. YAML
anchors and merge keys are supported. Each selected service must have one replica
and an image or build definition. Model services cannot have automatic restart
policies. Reserved `io.lmserve.*` labels belong to the CLI.

Dependency cycles and dependencies that select another model are rejected.
Container dependencies using `volumes_from: container:...` are unsupported.
Admission checks selected container names and host ports against recorded and
unowned containers; a name collision does not establish ownership. Direct Podman
or Compose commands bypass CLI admission, and unrelated GPU processes remain
outside its control.
