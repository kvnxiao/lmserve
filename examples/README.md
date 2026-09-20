# User-owned Compose example

[compose.yaml](compose.yaml) illustrates a vLLM repository snapshot, a source-built NInfer
single-file model, and a persistent Open WebUI companion. It is a structural example with
placeholder references, not a verified serving recipe. No example has been deployed against a live
model or GPU.

Copy the file into your own configuration directory. Select compatible images, model
repositories/revisions, and artifact formats. Replace the NInfer source ref, Dockerfile path if
necessary, executable, arguments, and readiness path using the selected upstream revision's
documentation. Check vLLM command and Open WebUI environment settings against the chosen image
versions as well.

Both model services use the `model-api` network alias and container port `8000` so the companion
endpoint stays unchanged during a switch. Only one model may run. If the chosen NInfer server does
not provide the API required by the companion, remove `webui` from its companion list or supply a
compatible user-owned service. Set any required API authentication in your own configuration.
Offline variables and local model arguments must be supported by each selected engine; the CLI does
not infer engine-specific offline flags.

The example binds model and chat ports to `127.0.0.1`. Keep WebUI authentication enabled, establish
its administrator account, and review stored endpoint settings before exposing chat to other
machines. Its `webui-data` named volume preserves accounts, settings, and history across lifecycle
commands. Existing WebUI settings may require changes through WebUI administration; the CLI does not
rewrite them.

Prepare one entry with `lmserve update-images vllm` and `lmserve update-models vllm`, or use `--all`
to prepare both. After starting `vllm`, `lmserve switch ninfer` requires the second entry's
dependencies to be prepared. Image/model selection changes belong in your copied file. Repository
updates do not replace it.
