#!/usr/bin/env python3
"""Fake IPFS provider for the publisher guards: a Kubo RPC API, an IPFS Pinning Service API
and a path gateway on one loopback port.

    fake_ipfs.py <state-dir>

prints the port it listens on, then serves until killed. It derives CIDs itself (SHA-256 raw
leaves and dag-pb directory nodes), so a publisher that computes a different CID for the same
bytes is caught rather than echoed. Files under <state-dir>:

    log        one line per call that changes provider state
    requests   one line per request of any kind
    knobs/     an empty file named after a fault switches that fault on:
                 wrong-cid  psa-wrong-cid  psa-stuck
                 psa-remote-node (the service's source is a node other than this fake)
                 gateway-200  gateway-no-cors  gateway-bad-range  gateway-down
"""

import base64
import hashlib
import json
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, unquote, urlsplit

STATE = Path(sys.argv[1])
TOKEN = "test-token"
LOCK = threading.Lock()
BLOCKS = {}  # cid -> bytes (a whole single-leaf file, or a directory node)
KUBO_PINS = {}  # cid -> name
PSA_PINS = []  # {"requestid", "status", "pin": {"cid", "name", "origins"}}


def cid_of(codec, block):
    digest = hashlib.sha256(block).digest()
    raw = bytes([0x01, codec, 0x12, 0x20]) + digest
    return "b" + base64.b32encode(raw).decode().lower().rstrip("=")


def knob(name):
    return (STATE / "knobs" / name).exists()


def record(line):
    with open(STATE / "log", "a") as handle:
        handle.write(line + "\n")


def varint(data, at):
    value = shift = 0
    while True:
        byte = data[at]
        at += 1
        value |= (byte & 0x7F) << shift
        shift += 7
        if byte < 0x80:
            return value, at


def fields(data):
    """Yields (field number, value) of a protobuf message of varint and bytes fields."""
    at = 0
    while at < len(data):
        key, at = varint(data, at)
        if key & 7 == 0:
            value, at = varint(data, at)
        else:
            length, at = varint(data, at)
            value, at = data[at : at + length], at + length
        yield key >> 3, value


def directory_links(block):
    """Maps entry name to child CID for a dag-pb directory node."""
    links = {}
    for number, link in fields(block):
        if number == 2:
            entry = dict(fields(link))
            links[entry[2].decode()] = "b" + base64.b32encode(entry[1]).decode().lower().rstrip("=")
    return links


def multipart_payload(content_type, body):
    boundary = content_type.split("boundary=")[1].strip('"').encode()
    part = body.split(b"--" + boundary)[1]
    return part.split(b"\r\n\r\n", 1)[1][: -len(b"\r\n")]


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_):
        pass

    def reply(self, status, body=b"", headers=()):
        if isinstance(body, (dict, list)):
            body = json.dumps(body).encode()
        self.send_response(status)
        for name, value in headers:
            self.send_header(name, value)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def handle_any(self):
        url = urlsplit(self.path)
        query = {key: values[0] for key, values in parse_qs(url.query).items()}
        body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
        with LOCK:
            with open(STATE / "requests", "a") as handle:
                handle.write(f"{self.command} {self.path}\n")
            if url.path.startswith("/api/v0/"):
                self.kubo(url.path[len("/api/v0/") :], query, body)
            elif url.path.startswith("/psa/"):
                self.pinning_service(url.path[len("/psa") :], query, body)
            elif url.path.startswith("/ipfs/"):
                self.gateway(unquote(url.path[len("/ipfs/") :]))
            else:
                self.reply(404)

    do_GET = do_POST = do_DELETE = handle_any

    # --- Kubo RPC ---
    def kubo(self, command, query, body):
        if self.command != "POST":
            return self.reply(405)
        if command == "id":
            addresses = ["/ip4/127.0.0.1/tcp/4001/p2p/fake", "/ip4/203.0.113.7/tcp/4001/p2p/fake"]
            return self.reply(200, {"ID": "fake", "Addresses": addresses})
        if command == "add":
            explicit = {"cid-version": "1", "raw-leaves": "true", "chunker": "size-1048576"}
            if any(query.get(key) != value for key, value in explicit.items()):
                return self.reply(400, {"Message": "import parameters must be explicit"})
            data = multipart_payload(self.headers["Content-Type"], body)
            cid = cid_of(0x55, data + b"!" if knob("wrong-cid") else data)
            BLOCKS[cid] = data
            if query.get("pin") == "true":
                KUBO_PINS[cid] = query.get("pin-name", "")
            record(f"kubo add {query.get('pin-name', '')} {cid}")
            return self.reply(200, {"Name": "file", "Hash": cid, "Size": str(len(data))})
        if command == "block/put":
            data = multipart_payload(self.headers["Content-Type"], body)
            cid = cid_of(0x70, data)
            BLOCKS[cid] = data
            record(f"kubo block {cid}")
            return self.reply(200, {"Key": cid, "Size": len(data)})
        if command == "swarm/connect":
            record(f"kubo connect {query['arg']}")
            return self.reply(200, {"Strings": ["connect success"]})
        cid = query.get("arg")
        if command == "pin/add":
            if cid not in BLOCKS or any(c not in BLOCKS for c in directory_links(BLOCKS[cid]).values()):
                return self.reply(500, {"Message": "block not found", "Type": "error"})
            KUBO_PINS[cid] = query.get("name", "")
            record(f"kubo pin {query.get('name', '')} {cid}")
            return self.reply(200, {"Pins": [cid]})
        if command == "pin/rm":
            if cid not in KUBO_PINS:
                return self.reply(500, {"Message": "not pinned", "Type": "error"})
            record(f"kubo unpin {KUBO_PINS.pop(cid)} {cid}")
            return self.reply(200, {"Pins": [cid]})
        if command == "pin/ls" and cid:
            if cid not in KUBO_PINS:
                return self.reply(500, {"Message": f"path '{cid}' is not pinned", "Type": "error"})
            return self.reply(200, {"Keys": {cid: {"Type": "recursive", "Name": ""}}})
        if command == "pin/ls":
            lines = [
                json.dumps({"Cid": pinned, "Type": "recursive", "Name": name})
                for pinned, name in sorted(KUBO_PINS.items())
                if query.get("name", "") in name
            ]
            return self.reply(200, ("\n".join(lines) + "\n").encode())
        return self.reply(404, {"Message": f"unsupported {command}"})

    # --- Pinning Service API ---
    def pinning_service(self, path, query, body):
        if self.headers.get("Authorization") != f"Bearer {TOKEN}":
            return self.reply(401, {"error": {"reason": "UNAUTHORIZED"}})

        def shown(pin):
            answer = dict(pin, created="2026-01-01T00:00:00Z", delegates=["/ip4/203.0.113.9/tcp/4001/p2p/delegate"])
            if knob("psa-wrong-cid"):
                answer["pin"] = dict(pin["pin"], cid=cid_of(0x55, b"something else"))
            return answer

        if path == "/pins" and self.command == "POST":
            wanted = json.loads(body)
            # The service can only fetch what the node named in `origins` holds.
            if not wanted.get("origins") or not (knob("psa-remote-node") or wanted["cid"] in BLOCKS):
                return self.reply(400, {"error": {"reason": "BAD_REQUEST"}})
            pin = {"requestid": f"request-{len(PSA_PINS) + 1}", "status": "pinning", "pin": wanted}
            PSA_PINS.append(pin)
            record(f"psa pin {wanted['name']} {wanted['cid']}")
            return self.reply(202, shown(pin))
        if path == "/pins" and self.command == "GET":
            statuses = query.get("status", "pinned").split(",")
            results = [
                shown(pin)
                for pin in PSA_PINS
                if pin["status"] in statuses
                and query.get("cid", pin["pin"]["cid"]) == pin["pin"]["cid"]
                and query.get("name", "") in pin["pin"]["name"]
            ]
            return self.reply(200, {"count": len(results), "results": results})
        pin = next((p for p in PSA_PINS if path == f"/pins/{p['requestid']}"), None)
        if pin is None:
            return self.reply(404, {"error": {"reason": "NOT_FOUND"}})
        if self.command == "DELETE":
            PSA_PINS.remove(pin)
            record(f"psa unpin {pin['pin']['name']} {pin['pin']['cid']}")
            return self.reply(202)
        if not knob("psa-stuck"):
            pin["status"] = "pinned"
        return self.reply(200, shown(pin))

    # --- Gateway ---
    def gateway(self, path):
        root, _, name = path.partition("/")
        held = set(KUBO_PINS) | {p["pin"]["cid"] for p in PSA_PINS if p["status"] == "pinned"}
        child = directory_links(BLOCKS[root]).get(name) if root in held and root in BLOCKS else None
        if knob("gateway-down") or child not in BLOCKS:
            return self.reply(503 if knob("gateway-down") else 404)
        data = BLOCKS[child]
        headers = [] if knob("gateway-no-cors") else [("Access-Control-Allow-Origin", "*")]
        requested = self.headers.get("Range")
        if not requested or knob("gateway-200"):
            return self.reply(200, data, headers)
        start, end = (int(bound) for bound in requested[len("bytes=") :].split("-"))
        total = len(data) + 1 if knob("gateway-bad-range") else len(data)
        headers.append(("Content-Range", f"bytes {start}-{end}/{total}"))
        return self.reply(206, data[start : end + 1], headers)


def main():
    (STATE / "knobs").mkdir(parents=True, exist_ok=True)
    for name in ("log", "requests"):
        (STATE / name).touch()
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    print(server.server_address[1], flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
