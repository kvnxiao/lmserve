import os
import sys
from pathlib import Path

cache = Path(os.environ["HF_HUB_CACHE"]).resolve()
network_attempts = []
cache_writes = []


def audit(event, args):
    if event in {"socket.connect", "socket.getaddrinfo", "socket.sendto"}:
        network_attempts.append(event)
        raise AssertionError(f"unexpected network operation: {event}")
    if event == "open" and isinstance(args[0], (str, bytes)):
        path = Path(os.fsdecode(args[0])).absolute()
        if path.is_relative_to(cache) and args[2] & (
            os.O_WRONLY | os.O_RDWR | os.O_CREAT | os.O_TRUNC | os.O_APPEND
        ):
            cache_writes.append(str(path))
            raise AssertionError(f"unexpected cache write: {path}")


sys.addaudithook(audit)

from huggingface_hub import hf_hub_download, snapshot_download
from huggingface_hub.errors import LocalEntryNotFoundError
from transformers import AutoConfig, AutoTokenizer

repo = "fixture/first-repo"
commit = "a" * 40
snapshot = Path(snapshot_download(repo))
assert snapshot == cache / "models--fixture--first-repo" / "snapshots" / commit
assert AutoConfig.from_pretrained(repo).lmserve_revision == commit
assert AutoTokenizer.from_pretrained(repo).encode(commit) == [1]
assert Path(hf_hub_download(repo, "model.bin")).read_text() == commit

for filename, revision in [("missing.bin", "main"), ("model.bin", "b" * 40)]:
    try:
        hf_hub_download(repo, filename, revision=revision)
    except LocalEntryNotFoundError:
        pass
    else:
        raise AssertionError(f"resolved unprepared content: {filename}@{revision}")

assert not network_attempts, network_attempts
assert not cache_writes, cache_writes
