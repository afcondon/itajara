"""Minimal WebSocket client for the itajara daemon on :23028.

Enough to read a snapshot and send commands; no dependencies.
"""
import json, socket, base64, os, struct, time

PORT = 23028


class Rig:
    def __init__(self, port=PORT):
        self.s = socket.create_connection(("127.0.0.1", port), 3)
        key = base64.b64encode(os.urandom(16)).decode()
        self.s.send((
            "GET / HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\n"
            "Connection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        ).encode())
        time.sleep(0.3)
        data = self.s.recv(65536)
        self.buf = data.split(b"\r\n\r\n", 1)[1] if b"\r\n\r\n" in data else b""

    def send(self, text):
        payload = text.encode()
        mask = os.urandom(4)
        masked = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
        n = len(payload)
        if n < 126:
            hdr = struct.pack("!BB", 0x81, 0x80 | n)
        else:
            hdr = struct.pack("!BBH", 0x81, 0x80 | 126, n)
        self.s.send(hdr + mask + masked)

    def _frames(self):
        out, buf, i = [], self.buf, 0
        while i + 2 <= len(buf):
            b2 = buf[i + 1]
            i += 2
            ln = b2 & 0x7F
            if ln == 126:
                ln = struct.unpack(">H", buf[i:i + 2])[0]; i += 2
            elif ln == 127:
                ln = struct.unpack(">Q", buf[i:i + 8])[0]; i += 8
            if i + ln > len(buf):
                i -= 2
                break
            out.append(buf[i:i + ln]); i += ln
        self.buf = buf[i:]
        return out

    def snapshot(self, wait=0.5):
        time.sleep(wait)
        self.s.setblocking(False)
        try:
            while True:
                chunk = self.s.recv(1 << 20)
                if not chunk:
                    break
                self.buf += chunk
        except BlockingIOError:
            pass
        self.s.setblocking(True)
        latest = None
        for f in self._frames():
            try:
                latest = json.loads(f)
            except Exception:
                pass
        return latest

    def close(self):
        self.s.close()


WRITING = ("recordingFirst", "overdubbing", "multiplying")


def show(snap, title):
    print(f"\n{title}")
    print(f"  audio alive {snap['audioAlive']}  device lost {snap['deviceLost']}"
          f"  reopens {snap['reopens']}  sr {snap['sampleRate']}")
    print(f"  link: tempo {snap['linkTempo']:.1f}  anchors {snap['linkAnchors']}"
          f"  rejected {snap['linkRejected']}")
    print(f"  ack: {snap['ack']!r} (seq {snap['ackSeq']})")
    for lp in snap["loops"]:
        flags = [k for k in ("muted", "reverse", "pendulum", "oneShot",
                             "levelArm", "armed", "quant", "skipping")
                 if lp[k]]
        extra = []
        if lp["speed"] != 1.0:
            extra.append(f"speed {lp['speed']}")
        if lp["pan"] != 64:
            extra.append(f"pan {lp['pan']}")
        if lp["chance"] != 1.0:
            extra.append(f"chance {lp['chance']}")
        if lp["fadeMs"]:
            extra.append(f"fade {lp['fadeMs']}")
        if lp["decayDb"]:
            extra.append(f"decay {lp['decayDb']}")
        mark = "  <== WRITING" if lp["state"] in WRITING else ""
        print(f"  loop {lp['index']}  {lp['state']:<14} layers {lp['layers']}"
              f"  {lp['loopSecs']:.2f}s  {' '.join(flags + extra)}{mark}")
