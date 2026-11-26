# SPDX-License-Identifier: EUPL-1.2
"""Versioned MessagePack frames for Python tests of native workers."""

import struct

VERSION = 2
MAX_FRAME_BYTES = 12 * 1024 * 1024


def frame(request_id, payload, *, limit=MAX_FRAME_BYTES):
    from msgpack import packb

    body = packb({"version": VERSION, "request_id": request_id, "payload": payload}, use_bin_type=True)
    if len(body) > limit:
        raise ValueError(f"worker request exceeds {limit} bytes")
    return struct.pack(">I", len(body)) + body


def write_frame(stream, request_id, payload, *, limit=MAX_FRAME_BYTES):
    stream.write(frame(request_id, payload, limit=limit))
    stream.flush()


def read_frame(stream, request_id, *, limit=MAX_FRAME_BYTES):
    from msgpack import unpackb

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
    envelope = unpackb(body, raw=False)
    if envelope["version"] != VERSION or envelope["request_id"] != request_id:
        raise ValueError(f"unexpected worker reply version or request id for request {request_id}")
    return envelope["payload"]
