# SPDX-License-Identifier: EUPL-1.2
"""Drive native worker pipes through the production codec in a test helper."""

import atexit
import base64
import json
import os
from pathlib import Path
import shutil
import struct
import subprocess
import threading

MAX_FRAME_BYTES = 12 * 1024 * 1024
MAX_LINE_BYTES = MAX_FRAME_BYTES * 8
_bridge = None
_lock = threading.Lock()


def _json_bytes(value):
    if isinstance(value, bytes):
        return list(value)
    raise TypeError(f"unexpected worker test value: {type(value).__name__}")


def _stop_bridge():
    _bridge.stdin.close()
    try:
        _bridge.wait(timeout=2)
    except subprocess.TimeoutExpired:
        _bridge.kill()
        _bridge.wait()
    _bridge.stdout.close()


def _convert(executable, **request):
    global _bridge
    with _lock:
        if _bridge is None:
            binary = os.environ.get("NUSCHL_WORKER_TEST_CODEC")
            if binary is None:
                binary = executable if executable.is_file() else shutil.which(executable.name) or executable
            _bridge = subprocess.Popen([binary], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
            atexit.register(_stop_bridge)
        line = json.dumps(request, default=_json_bytes).encode()
        if len(line) >= MAX_LINE_BYTES:
            raise ValueError("test codec request exceeds its size limit")
        _bridge.stdin.write(line + b"\n")
        _bridge.stdin.flush()
        result = _bridge.stdout.readline(MAX_LINE_BYTES)
        if not result.endswith(b"\n"):
            raise ValueError("missing or oversized test codec reply")
        reply = json.loads(result)
        if "error" in reply:
            raise ValueError(f"{request['role']} test codec: {reply['error']}")
        return reply["value"]


class WorkerCodec:
    def __init__(self, role, worker_executable):
        self.role = role
        worker = Path(worker_executable).absolute()
        suffix = ".exe" if worker.suffix == ".exe" else ""
        self.executable = worker.with_name("worker-test-codec" + suffix)

    def frame(self, request_id, payload, *, limit=MAX_FRAME_BYTES):
        if self.role != "math":
            fields = dict(payload)
            tag = fields.pop("kind")
            payload = {tag: fields} if fields else tag
        encoded = _convert(self.executable, op="encode", role=self.role,
                           request_id=request_id, payload=payload)
        data = base64.b64decode(encoded, validate=True)
        if len(data) > limit + 4:
            raise ValueError(f"worker request exceeds {limit} bytes")
        return data

    def write_frame(self, stream, request_id, payload, *, limit=MAX_FRAME_BYTES):
        stream.write(self.frame(request_id, payload, limit=limit))
        stream.flush()

    def read_frame(self, stream, request_id, *, limit=MAX_FRAME_BYTES):
        header = stream.read(4)
        if not header:
            return None
        if len(header) != 4:
            raise ValueError("incomplete worker frame header")
        size, = struct.unpack(">I", header)
        if not 0 < size <= limit:
            raise ValueError(f"invalid worker frame size {size}")
        body = stream.read(size)
        if len(body) != size:
            raise ValueError("incomplete worker frame body")
        reply = _convert(self.executable, op="decode", role=self.role,
                         request_id=request_id, frame=base64.b64encode(header + body).decode("ascii"))
        if isinstance(reply, str):
            tag, value = reply, None
        else:
            (tag, value), = reply.items()
        if self.role in ("call_audio", "live_media", "transfer"):
            fields = {"kind": tag, **(value or {})}
            if "opus" in fields:
                fields["opus"] = bytes(fields["opus"])
            return fields
        if self.role == "media" and tag == "packet":
            value = bytes(value)
        return {"status": tag, "value": value} if value is not None else {"status": tag}
