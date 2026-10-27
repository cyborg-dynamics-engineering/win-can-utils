"""Windows CAN protocol v1 using only Python's standard library.

python examples/service_client.py can0
python examples/service_client.py can0 --send 123#010203
"""
import argparse
import json
import struct
import time

MAX_MESSAGE = 65536


def read_exact(pipe, count):
    result = bytearray()
    while len(result) < count:
        chunk = pipe.read(count - len(result))
        if not chunk:
            raise EOFError("CAN service disconnected")
        result.extend(chunk)
    return result


def request(pipe, request_id, op, **fields):
    data = json.dumps(dict(version=1, id=request_id, op=op, **fields)).encode("utf-8")
    if not 0 < len(data) <= MAX_MESSAGE:
        raise ValueError("Request is too large")
    packet = memoryview(struct.pack("<I", len(data)) + data)
    while packet:
        count = pipe.write(packet)
        if not count:
            raise EOFError("CAN service disconnected")
        packet = packet[count:]
    while True:
        length, = struct.unpack("<I", read_exact(pipe, 4))
        if not 0 < length <= MAX_MESSAGE:
            raise ValueError("Invalid server message length")
        message = json.loads(read_exact(pipe, length))
        if message["version"] != 1:
            raise ValueError("Unsupported server version")
        if message["kind"] == "frame":
            continue
        if message.get("id") != request_id:
            raise ValueError("Unexpected response ID")
        if message["kind"] == "error":
            raise RuntimeError(f'{message["code"]}: {message["message"]}')
        return message["reply"]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("interface")
    parser.add_argument("--send", help="Classic data frame, e.g. 123#010203")
    args = parser.parse_args()
    name = args.interface
    if not 1 <= len(name) <= 64 or not all(c.isascii() and (c.isalnum() or c in "_-") for c in name):
        parser.error("Invalid interface name")
    deadline = time.monotonic() + 3
    while True:
        try:
            pipe = open(rf"\\.\pipe\wincan_{name}_v1", "r+b", buffering=0)
            break
        except OSError as error:
            if getattr(error, "winerror", None) not in (2, 231) or time.monotonic() >= deadline:
                raise
            time.sleep(0.01)
    with pipe:
        request(pipe, 1, "hello", subscription={"receive": False})
        if args.send:
            identifier, payload = args.send.split("#", 1)
            data = list(bytes.fromhex(payload))
            print(json.dumps(request(pipe, 2, "send", frame=dict(
                id=int(identifier, 16), extended=len(identifier) == 8,
                dlc=len(data), data=data)), indent=2))
        print(json.dumps(request(pipe, 3, "status"), indent=2))


if __name__ == "__main__":
    main()
