#!/usr/bin/env python3
"""Demo traffic for a gateway running examples/turbojet-gateway.toml, to see the console with
orders in it: a NASDAQ venue on :9877 that answers orders by symbol, and a FUNDA client on :9876
that sends a handful. Each stays up, sending heartbeats, until stopped.
  scripts/fix-demo.py venue    # first: the gateway's initiator logs on to it
  scripts/fix-demo.py client   # then: eight orders, one canceled, two rejected by the gateway
The venue answers AAPL with a fill, MSFT with a partial fill, AMZN with two fills, and leaves
the rest working. Minimal FIX 4.4, Python's standard library only: no resends, and sequence
numbers from 1, so restart the gateway (whose example stores are in memory) between runs."""
import itertools
import socket
import sys
import threading
import time
from datetime import datetime, timezone

SOH = "\x01"
GATEWAY = "TURBOJETGATEWAY"


def now():
    return datetime.now(timezone.utc).strftime("%Y%m%d-%H:%M:%S.%f")[:-3]


class Session:
    """One FIX session over a connected socket: frames, numbers and heartbeats."""

    def __init__(self, sock, sender, target, on_message):
        self.sock, self.sender, self.target, self.on_message = sock, sender, target, on_message
        self.seq = itertools.count(1)
        self.lock = threading.Lock()
        self.orders = {}  # the venue's, by ClOrdID
        self.logged_on = threading.Event()
        threading.Thread(target=self.read, daemon=True).start()
        threading.Thread(target=self.beat, daemon=True).start()

    def send(self, msg_type, fields):
        with self.lock:
            body = f"35={msg_type}{SOH}49={self.sender}{SOH}56={self.target}{SOH}34={next(self.seq)}{SOH}52={now()}{SOH}"
            body += "".join(f"{tag}={value}{SOH}" for tag, value in fields)
            head = f"8=FIX.4.4{SOH}9={len(body)}{SOH}"
            check = sum((head + body).encode()) % 256
            self.sock.sendall(f"{head}{body}10={check:03}{SOH}".encode())

    def read(self):
        buffer = b""
        while chunk := self.sock.recv(65536):
            buffer += chunk
            while (end := buffer.find(b"\x0110=")) >= 0 and (stop := buffer.find(b"\x01", end + 4)) >= 0:
                raw, buffer = buffer[: stop + 1].decode(), buffer[stop + 1 :]
                fields = dict(f.split("=", 1) for f in raw.strip(SOH).split(SOH))
                if fields["35"] == "1":
                    self.send("0", [(112, fields["112"])])
                elif fields["35"] not in ("0", "5"):
                    self.on_message(self, fields)
        print(f"{self.sender}: disconnected", flush=True)

    def beat(self):
        while True:
            time.sleep(10)
            self.send("0", [])


# The venue's answer to each symbol's order: fills after the ack (quantities).
FILLS = {"AAPL": [500], "MSFT": [400], "AMZN": [300, 500]}
venue_ids = itertools.count(1)


def report(session, order, exec_type, status, cum, last=0, cl_ord_id=None, orig=None):
    qty, px = float(order["38"]), order["44"]
    fields = [(37, order["venue_id"]), (11, cl_ord_id or order["11"])]
    if orig:
        fields.append((41, orig))
    fields += [(17, f"X{next(venue_ids)}"), (150, exec_type), (39, status), (55, order["55"]),
               (54, order["54"]), (38, order["38"]), (44, px)]
    if last:
        fields += [(32, last), (31, px)]
    fields += [(151, 0 if status in "24" else qty - cum), (14, cum), (6, px if cum else 0)]
    session.send("8", fields)


def venue_message(session, msg):
    if msg["35"] == "A":
        session.send("A", [(98, 0), (108, msg["108"])])
        print("venue: logged on", flush=True)
    elif msg["35"] == "D":
        msg["venue_id"] = f"NQ{next(venue_ids)}"
        session.orders[msg["11"]] = msg
        report(session, msg, "0", "0", 0)
        cum = 0
        for last in FILLS.get(msg["55"], []):
            time.sleep(0.3)
            cum += last
            report(session, msg, "F", "2" if cum >= float(msg["38"]) else "1", cum, last)
    elif msg["35"] == "F":
        order = session.orders[msg["41"]]
        report(session, order, "4", "4", 0, cl_ord_id=msg["11"], orig=msg["41"])


def venue():
    server = socket.create_server(("127.0.0.1", 9877))
    sock, _ = server.accept()
    Session(sock, "NASDAQ", GATEWAY, venue_message)


ORDERS = [  # ClOrdID, side (1 buy, 2 sell), symbol, quantity, price, ExDestination
    ("C1", 1, "AAPL", 500, "231.50", "XNAS"),
    ("C2", 2, "MSFT", 1200, "418.20", "XNAS"),
    ("C3", 1, "NVDA", 2000, "135.10", "XNAS"),
    ("C4", 1, "AMZN", 800, "187.40", "XNAS"),
    ("C5", 2, "TSLA", 300, "252.00", "XNAS"),  # canceled after its ack
    ("C6", 1, "META", 1000, "590.00", "XNAS"),
    ("C7", 1, "GOOG", 400, "165.30", "XLON"),  # no route: rejected
    ("C8", 1, "AAPL", 20000, "231.60", "XNAS"),  # over max_order_qty: rejected
]


def client_message(session, msg):
    if msg["35"] == "A":
        session.logged_on.set()
    elif msg["35"] == "8":
        print(f"client: {msg.get('11')} ExecType {msg['150']} OrdStatus {msg['39']} {msg.get('58', '')}", flush=True)


def client():
    session = Session(socket.create_connection(("127.0.0.1", 9876)), "FUNDA", GATEWAY, client_message)
    session.send("A", [(98, 0), (108, 30)])
    if not session.logged_on.wait(10):
        sys.exit("client: no Logon from the gateway")
    for cl_ord_id, side, symbol, qty, px, ex_dest in ORDERS:
        session.send("D", [(11, cl_ord_id), (55, symbol), (54, side), (60, now()), (38, qty), (40, 2),
                           (44, px), (59, 0), (100, ex_dest)])
        time.sleep(0.5)
    time.sleep(1)
    session.send("F", [(11, "C5-X"), (41, "C5"), (55, "TSLA"), (54, 2), (60, now()), (38, 300)])


if sys.argv[1:] == ["venue"]:
    venue()
elif sys.argv[1:] == ["client"]:
    client()
else:
    sys.exit("usage: scripts/fix-demo.py venue|client")
while True:
    time.sleep(60)
