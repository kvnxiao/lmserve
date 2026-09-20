# State, storage, and recovery

## Preparation and retention

`update-images` pulls the configured image or builds the configured context. A build definition
selects a build even when the service also has an image name. Updates record image identities and
available source revision labels without recreating running containers. Local source checkouts
remain user-owned.

`update-models` resolves the configured Hugging Face revision through `hf`; an omitted revision
selects the current remote `main` commit. It stages missing content, verifies readable files,
publishes a completed artifact, and then removes eligible superseded content. Identical prepared
content is reused. Failed downloads preserve the previous prepared revision; cleanup failures are
reported separately and can be retried. Temporary storage must accommodate the download and
published copy alongside the previous revision.

Repository artifacts contain a Hugging Face cache with `models--OWNER--REPOSITORY/snapshots/COMMIT/`
and `refs/main` pointing to that exact commit. Preparing another revision creates a separate cache
without changing the previous artifact's local ref. Files are stored once in the published cache.
Single-file artifacts contain only the selected file.

Before startup and before stopping a model for replacement, the CLI checks the recorded file sizes
and readability, cache file set, sole snapshot revision, and exact `refs/main` contents. These
checks do not hash model files. HF offline settings make missing required content fail locally
instead of downloading it; startup does not resolve remote revisions.

An active model blocks updates to its artifact source. Lifecycle work and model updates hold
filesystem locks to prevent conflicting changes. Batch updates continue independent eligible entries
and return failure if a target fails.

Managed content is stored under the project's `.lmserve` namespace inside the model root,
independently of the user's general Hugging Face cache. Cleanup preserves prepared references,
active deployment references, and configured exact revisions. Unknown ownership or unresolved
references preserve content and produce a cleanup error. Retention does not provide historical
rollback. Changing `model-directory` requires preparation at the new root; old data and active
mounts remain in place.

## Lifecycle outcomes

Accepted operations run in detached, short-lived workers. Records distinguish `accepted`,
`starting`, `waiting`, `stopping`, `ready`, `stopped`, `failed`, `cancelled`, and `interrupted`.
Serving containers continue after workers exit. There is no permanent daemon or automatic recovery
after host reboot or WSL shutdown.

Startup uses captured configuration and resolved image/artifact identities. Another active model
requires `switch`; `restart` replaces the same entry. The worker confirms the previous model has
exited before launching its replacement. Unchanged shared companions may remain running. Changed
companion definitions, file inputs, or images require an explicit deployment stop/start.

The worker waits for the model endpoint to return HTTP success. Optional companion failure is
recorded separately and does not stop a ready model. After model startup fails, the worker attempts
to stop its owned group, including retained companions, and reports any shutdown failure. Preflight
failure leaves the current deployment running. Failed replacement does not restore the previous
model or select a fallback.

`stop` uses recorded containers and their stop settings, including after the model exits or the
Compose file becomes unavailable. Shutdown may interrupt requests. Ordinary stop and startup cleanup
preserve images, model files, companion volumes, and shared networks. Stopping a stale entry
preserves companions transferred to the active deployment. Stopped-container logs remain available
until recreation.

## Inspection and recovery

Persistent state is stored under `$XDG_STATE_HOME/lmserve`, or `~/.local/state/lmserve`. Keep it
separate from the model cache and preserve it while managed containers exist: it records ownership,
container identities, artifact paths, companion membership, and operation results.

After a worker is interrupted, run `lmserve status` to inspect the recorded operation and actual
containers. Run `lmserve stop ENTRY` to stop the recorded group, then explicitly start or switch
when prerequisites are available. Missing configuration does not prevent recorded status, health,
logs, or shutdown. Unavailable runtime inspection is reported as an error; it does not establish
that containers have stopped. Unknown managed containers block another model's admission rather than
being adopted.

`status` and `health` check current model health. A later model exit leaves companions running until
an explicit command; a companion exit leaves the model running. No continuous watcher restarts
either component.

## Credentials and access

Model downloads use the user's existing `hf` authentication. The CLI does not perform interactive
login. Container credentials and application authentication remain user-owned configuration.

Accepted requests capture rendered configuration, `.env`, service environment files, and file-backed
secrets/configs under the state directory. These files may contain credentials. New state
directories use mode `0700`, and published files use private temporary-file permissions. Raw
subprocess output is omitted from diagnostic summaries; worker stderr may remain in the private
execution-lock file. Protect state backups and existing state-directory permissions accordingly.
Container logs are displayed as produced by the application.

Examples publish host ports on localhost. Remote access requires deliberate network binding and
application authentication or a user-managed authenticated proxy. Open WebUI stores accounts,
settings, and chat history in its named volume; keep authentication enabled and establish its
administrator account before exposing it. The CLI does not configure a VPN, proxy, or chat accounts.
