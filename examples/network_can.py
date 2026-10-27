"""CAN service v1 over a dedicated Windows adapter's UDP endpoint.

python examples/network_can.py --status
python examples/network_can.py --send 123#010203 --receive-own-messages
python examples/network_can.py --dump --filter 123:800007FF

UDP sends are never automatically retried: a lost reply cannot undo a CAN send.
"""
import argparse
import collections
import json
import socket
import time


class Client:
    def __init__(self, local, peer, port, subscription):
        self.socket = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.socket.bind((local, 0))
        self.socket.connect((peer, port))
        self.socket.settimeout(3)
        self.next_id = 0
        self.events = collections.deque(maxlen=1024)
        self.dropped = 0
        hello = self.request("hello", subscription=subscription)
        self.client_id = hello["client_id"]

    def receive(self):
        message = json.loads(self.socket.recv(65535))
        if message.get("version") != 1:
            raise ValueError("Unsupported CAN protocol version")
        return message

    def request(self, op, **fields):
        self.next_id += 1
        request_id = self.next_id
        payload = json.dumps(dict(version=1, id=request_id, op=op, **fields)).encode()
        if len(payload) > 65507:
            raise ValueError("UDP request is too large")
        self.socket.send(payload)
        while True:
            message = self.receive()
            if message["kind"] == "frame":
                if len(self.events) == self.events.maxlen:
                    self.dropped += 1
                self.events.append(message["event"])
                continue
            if message.get("id") != request_id:
                raise ValueError("Unexpected response ID; do not reuse a UDP endpoint for simultaneous clients")
            if message["kind"] == "error":
                raise RuntimeError(f'{message["code"]}: {message["message"]}')
            return message["reply"]

    def event(self):
        if self.events:
            return self.events.popleft()
        message = self.receive()
        if message["kind"] == "error":
            raise RuntimeError(f'{message["code"]}: {message["message"]}')
        if message["kind"] != "frame":
            raise ValueError("Expected a frame event")
        return message["event"]


def frame(text):
    identifier, payload = text.split("#", 1)
    result = dict(id=int(identifier, 16), extended=len(identifier) == 8)
    if payload.upper().startswith("R"):
        dlc = int(payload[1:] or "0")
        result.update(rtr=True, dlc=dlc, data=[])
    else:
        data = list(bytes.fromhex(payload))
        dlc = len(data)
        result.update(dlc=dlc, data=data)
    if not 0 <= dlc <= 8:
        raise ValueError("Only classic CAN payloads are supported")
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--local", default="198.18.0.1")
    parser.add_argument("--peer", default="198.18.0.2")
    parser.add_argument("--port", type=int, default=29536)
    parser.add_argument("--status", action="store_true")
    parser.add_argument("--send")
    parser.add_argument("--dump", action="store_true")
    parser.add_argument("--count", type=int)
    parser.add_argument("--filter", action="append", default=[])
    parser.add_argument("--receive-own-messages", action="store_true")
    args = parser.parse_args()
    filters = []
    for item in args.filter:
        identifier, mask = item.split(":", 1)
        filters.append(dict(id=int(identifier, 16), mask=int(mask, 16)))
    client = Client(args.local, args.peer, args.port, dict(
        receive=args.dump or args.receive_own_messages,
        receive_own_messages=args.receive_own_messages, filters=filters))
    try:
        if args.send:
            print(json.dumps(client.request("send", frame=frame(args.send))))
            if args.receive_own_messages and not args.dump:
                while True:
                    event = client.event()
                    if event["source_client_id"] == client.client_id:
                        print(json.dumps(event))
                        break
        if args.status or not (args.send or args.dump):
            print(json.dumps(client.request("status"), indent=2))
        received = 0
        keepalive = time.monotonic()
        while args.dump and (args.count is None or received < args.count):
            if time.monotonic() - keepalive >= 20:
                client.request("status")
                keepalive = time.monotonic()
            try:
                print(json.dumps(client.event()), flush=True)
                received += 1
            except socket.timeout:
                continue
    except KeyboardInterrupt:
        pass
    finally:
        client.socket.close()


if __name__ == "__main__":
    main()
