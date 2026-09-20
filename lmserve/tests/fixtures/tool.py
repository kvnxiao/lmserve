#!/usr/bin/python3
import json
import os
from pathlib import Path
import re
import sys
import time

root = Path(os.environ["LMSERVE_TEST_ROOT"])
tool = Path(sys.argv[0]).name
args = sys.argv[1:]
with (root / "calls.jsonl").open("a") as stream:
    stream.write(json.dumps([tool, *args]) + "\n")


def load():
    path = root / "runtime.json"
    return json.loads(path.read_text()) if path.exists() else {"containers": [], "images": {}}


def save(data):
    temporary = root / f"runtime-{os.getpid()}.json"
    temporary.write_text(json.dumps(data))
    temporary.replace(root / "runtime.json")


def fail(message):
    print(message, file=sys.stderr)
    sys.exit(1)


def interpolate(value):
    if isinstance(value, str):
        return re.sub(r"\$\$|\$\{([A-Za-z_][A-Za-z_0-9]*)\}",
                      lambda match: "$" if match.group(0) == "$$" else os.environ.get(match.group(1), ""), value)
    if isinstance(value, list):
        return [interpolate(item) for item in value]
    if isinstance(value, dict):
        return {key: interpolate(item) for key, item in value.items()}
    return value


def create_service(document, service, data):
    definition = document["services"][service]
    for dependency in definition.get("depends_on", []):
        create_service(document, dependency, data)
    assert definition["pull_policy"] == "never"
    assert "build" not in definition
    if os.environ.get("FAKE_FAIL_SERVICE") == service:
        fail("service failed: password=must-not-leak")
    name = definition.get("container_name", document["name"] + "_" + service + "_1")
    if any(container["Name"] == name for container in data["containers"]):
        return
    container = {"Id": "id-" + definition["labels"]["io.lmserve.owner"], "Name": name,
                 "Config": {"Labels": definition["labels"]},
                 "State": {"Running": os.environ.get("FAKE_COMPLETED_SERVICE") != service, "ExitCode": 0},
                 "NetworkSettings": {"Ports": {}}}
    data["containers"].append(container)
    save(data)
    (root / ("started-" + service)).write_text("started")
    (root / ("definition-" + service + ".json")).write_text(json.dumps(definition))
    if os.environ.get("FAKE_BLOCK_SERVICE") == service:
        deadline = time.monotonic() + 10
        while not (root / "release").exists() and time.monotonic() < deadline:
            time.sleep(0.01)


data = load()
if tool == "podman-compose":
    if args[:1] == ["--version"]:
        print("podman-compose version " + os.environ.get("FAKE_PROVIDER_VERSION", "1.5.0"))
        sys.exit(0)
    document = interpolate(json.load(sys.stdin))
    action = next(value for value in args[4:] if value in ("config", "pull", "build", "up"))
    if action == "config":
        print(json.dumps(document))
    elif action in ("pull", "build"):
        service = args[-1]
        definition = document["services"][service]
        image = definition.get("image", document["name"] + "_" + service)
        data["images"][image] = "sha256:" + image.replace(":", "_")
        save(data)
    elif action == "up":
        assert "--no-deps" not in args and "--no-build" in args and "--no-recreate" in args
        create_service(document, args[-1], data)
    else:
        fail("unsupported fake compose invocation")
elif tool == "podman":
    if args[:1] == ["--version"]:
        print("podman version " + os.environ.get("FAKE_PODMAN_VERSION", "5.5.0"))
    elif args[:1] == ["ps"]:
        containers = [dict(container, Names=[container["Name"]], Labels=container["Config"]["Labels"]) for container in data["containers"]]
        if "--filter" in args:
            label = args[args.index("--filter") + 1].removeprefix("label=")
            key, value = label.split("=", 1)
            containers = [container for container in containers if container["Labels"].get(key) == value]
        print(json.dumps(containers))
    elif args[:2] == ["network", "exists"]:
        pass
    elif len(args) >= 2 and args[0] in ("volume", "network", "secret") and args[1] == "inspect":
        disappearing = os.environ.get("FAKE_EXTERNAL_DISAPPEARS") == args[-1]
        inspections = sum(json.loads(line)[1:] == args for line in (root / "calls.jsonl").read_text().splitlines())
        if os.environ.get("FAKE_MISSING_EXTERNAL") == args[-1] or (disappearing and inspections > 1):
            fail("external resource missing")
        print(json.dumps([{"Name": args[-1]}]))
    elif args[:1] in (["pull"], ["build"]):
        reference = args[-1] if args[0] == "pull" else args[args.index("-t") + 1]
        data["images"][reference] = "sha256:" + reference.replace(":", "_")
        save(data)
    elif args[:1] == ["create"]:
        name = next(arg.removeprefix("--name=") for arg in args if arg.startswith("--name="))
        labels = dict(args[index + 1].split("=", 1) for index, arg in enumerate(args) if arg == "--label")
        assert "io.lmserve.owner" in labels
        assert "--pull" in args and args[args.index("--pull") + 1] == "never"
        if any(container["Name"] == name for container in data["containers"]):
            fail("container already exists")
        container = {"Id": "id-" + labels["io.lmserve.owner"], "Name": name,
                     "Config": {"Labels": labels}, "State": {"Running": False, "ExitCode": 0},
                     "NetworkSettings": {"Ports": {}}}
        data["containers"].append(container)
        save(data)
        print(container["Id"])
    elif args[:1] == ["start"]:
        for container in data["containers"]:
            if container["Name"] == args[-1]:
                container["State"]["Running"] = True
        save(data)
    elif args[:1] == ["wait"]:
        print("0")
    elif args[:2] == ["container", "inspect"]:
        print(json.dumps([container for container in data["containers"] if container["Id"] in args[2:]]))
    elif args[:2] == ["image", "inspect"]:
        reference = args[2]
        identity = data["images"].get(reference)
        if identity is None and reference in data["images"].values():
            identity = reference
        if identity is None:
            fail("missing image")
        print(json.dumps([{"Id": identity}]))
    elif args[:1] == ["info"]:
        print(json.dumps({"host": {"security": {"rootless": True}}}))
    elif args[:1] == ["stop"]:
        if os.environ.get("FAKE_STOP_FAIL"):
            fail("stop failed")
        for container in data["containers"]:
            if container["Id"] == args[-1]:
                container["State"]["Running"] = False
        save(data)
    elif args[:1] == ["rm"]:
        data["containers"] = [container for container in data["containers"] if container["Id"] != args[-1]]
        save(data)
    elif args[:1] == ["logs"]:
        print("fixture model log")
    else:
        fail("unsupported fake runtime invocation")
elif tool == "hf":
    if args[:2] == ["models", "info"]:
        print(json.dumps({"sha": os.environ.get("FAKE_REVISION", "a" * 40)}))
    elif args[:1] == ["download"]:
        if os.environ.get("FAKE_DOWNLOAD_FAIL"):
            fail("no space left on device: token=must-not-leak")
        cache = Path(args[args.index("--cache-dir") + 1])
        revision = args[args.index("--revision") + 1]
        snapshot = cache / "models--fixture" / "snapshots" / revision
        blob = cache / "models--fixture" / "blobs" / "content"
        blob.parent.mkdir(parents=True)
        blob.write_text(revision)
        snapshot.mkdir(parents=True)
        filename = args[2] if not args[2].startswith("--") else "model.bin"
        artifact = snapshot / filename
        artifact.parent.mkdir(parents=True, exist_ok=True)
        artifact.symlink_to(blob)
        print(artifact if filename != "model.bin" else snapshot)
    else:
        fail("unsupported fake hf invocation")
elif tool == "nvidia-ctk":
    if args == ["cdi", "list"]:
        print("nvidia.com/gpu=all")
    elif args[:2] == ["cdi", "generate"]:
        Path(args[args.index("--output") + 1]).write_text(json.dumps({"cdiVersion": "0.6.0", "devices": []}))
    else:
        fail("unsupported fake CDI invocation")
else:
    fail("unexpected executable")
