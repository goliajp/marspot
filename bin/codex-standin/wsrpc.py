"""JSON-RPC over a WebSocket on a unix socket, both ends: what a Codex
app-server control socket speaks.  Text frames only; a client masks
what it sends, a server does not."""
import base64
import hashlib
import json
import os
import socket
import struct
import threading

GUID = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11"


class Conn:
    def __init__(self, sock, masked):
        self.s = sock
        self.masked = masked
        self.buf = b""
        self.lock = threading.Lock()

    def _need(self, k):
        while len(self.buf) < k:
            got = self.s.recv(65536)
            if not got:
                raise EOFError
            self.buf += got
        out, self.buf = self.buf[:k], self.buf[k:]
        return out

    def recv(self):
        """The next JSON message; EOFError once the peer is gone."""
        while True:
            b0, b1 = self._need(2)
            k = b1 & 0x7F
            if k == 126:
                k = struct.unpack(">H", self._need(2))[0]
            elif k == 127:
                k = struct.unpack(">Q", self._need(8))[0]
            mask = self._need(4) if b1 & 0x80 else None
            p = self._need(k)
            if mask:
                p = bytes(c ^ mask[i % 4] for i, c in enumerate(p))
            op = b0 & 0x0F
            if op == 0x8:
                raise EOFError
            if op == 0x1:
                return json.loads(p)

    def send(self, obj):
        p = json.dumps(obj).encode()
        k = len(p)
        bit = 0x80 if self.masked else 0
        if k < 126:
            h = bytes([0x81, bit | k])
        elif k <= 0xFFFF:
            h = bytes([0x81, bit | 126]) + struct.pack(">H", k)
        else:
            h = bytes([0x81, bit | 127]) + struct.pack(">Q", k)
        if self.masked:
            m = os.urandom(4)
            h += m
            p = bytes(c ^ m[i % 4] for i, c in enumerate(p))
        with self.lock:
            self.s.sendall(h + p)

    def close(self):
        try:
            self.s.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass
        self.s.close()


def at_short(path, fn):
    """fn(path), or fn(basename) from inside its directory when the path
    is past what sun_path holds (104 bytes)."""
    if len(path.encode()) < 100:
        return fn(path)
    back = os.open(".", os.O_RDONLY)
    os.chdir(os.path.dirname(path))
    try:
        return fn(os.path.basename(path))
    finally:
        os.fchdir(back)
        os.close(back)


def connect(path, timeout=None):
    s = socket.socket(socket.AF_UNIX)
    s.settimeout(timeout)
    try:
        at_short(os.path.realpath(path), s.connect)
    except OSError:
        s.close()
        raise
    key = base64.b64encode(os.urandom(16))
    s.sendall(b"GET / HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
              b"Sec-WebSocket-Key: " + key + b"\r\nSec-WebSocket-Version: 13\r\n\r\n")
    c = Conn(s, masked=True)
    head = b""
    while b"\r\n\r\n" not in c.buf:
        got = s.recv(4096)
        if not got:
            raise EOFError
        c.buf += got
    head, c.buf = c.buf.split(b"\r\n\r\n", 1)
    if not head.startswith(b"HTTP/1.1 101"):
        raise ConnectionError(head[:80])
    return c


def accept(sock):
    """The server side of the handshake on an accepted socket."""
    c = Conn(sock, masked=False)
    while b"\r\n\r\n" not in c.buf:
        got = sock.recv(4096)
        if not got:
            raise EOFError
        c.buf += got
    head, c.buf = c.buf.split(b"\r\n\r\n", 1)
    key = b""
    for line in head.split(b"\r\n"):
        if line.lower().startswith(b"sec-websocket-key:"):
            key = line.split(b":", 1)[1].strip()
    acc = base64.b64encode(hashlib.sha1(key + GUID).digest())
    sock.sendall(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
                 b"Sec-WebSocket-Accept: " + acc + b"\r\n\r\n")
    return c


class Client:
    """Calls with answers matched by id; notifications go to `on_note`
    from a reader thread."""

    def __init__(self, path, name, on_note=None, timeout=None):
        self.c = connect(path, timeout)
        self.n = 0
        self.waiting = {}
        self.cv = threading.Condition()
        self.on_note = on_note
        self.closed = False
        threading.Thread(target=self._read, daemon=True).start()
        self.call("initialize", {"clientInfo": {"name": name, "version": "0"},
                                 "capabilities": {"experimentalApi": True}})
        self.c.send({"jsonrpc": "2.0", "method": "initialized"})

    def _read(self):
        try:
            while True:
                m = self.c.recv()
                if "id" in m and "method" not in m:
                    with self.cv:
                        self.waiting[m["id"]] = m
                        self.cv.notify_all()
                elif self.on_note:
                    self.on_note(m)
        except (EOFError, OSError, ValueError):
            with self.cv:
                self.closed = True
                self.cv.notify_all()
            if self.on_note:
                self.on_note(None)

    def call(self, method, params, timeout=30):
        """The whole answer: `result` or `error`."""
        with self.cv:
            self.n += 1
            n = self.n
        self.c.send({"jsonrpc": "2.0", "id": n, "method": method, "params": params})
        with self.cv:
            ok = self.cv.wait_for(lambda: n in self.waiting or self.closed, timeout)
            if n in self.waiting:
                return self.waiting.pop(n)
        raise TimeoutError(method) if not ok else EOFError(method)
