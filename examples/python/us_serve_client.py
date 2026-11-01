"""Minimal client for `usc serve` (JSON lines over stdin/stdout).

Python 3.8+, standard library only. Usage:

    from us_serve_client import UsClient
    with UsClient(r"C:\\tools\\unthinkable-search\\usc.exe", live=False) as us:
        r = us.search("*.pdf", sort="mtime", desc=True, limit=20, fields=["full", "size", "mtime"])
        print(r["total"], "results in", r["ms"], "ms")
        for it in r["items"]:
            print(it["full"], it["size"], it["mtime"])

Protocol reference: docs/INTEGRATION.md.
"""
import itertools
import json
import subprocess


class UsError(RuntimeError):
    def __init__(self, reply):
        super().__init__(f"{reply.get('error')}: {reply.get('message')}")
        self.reply = reply


class UsClient:
    def __init__(self, usc_path="usc.exe", live=True, data_dir=None):
        args = [usc_path, "serve", "--live", "auto" if live else "off"]
        if data_dir:
            args += ["--data", data_dir]
        self._p = subprocess.Popen(
            args,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            encoding="utf-8",
            bufsize=1,  # line buffered
        )
        self._ids = itertools.count(1)
        self.events = []  # {"event": "index" | "status", ...} after subscribe()
        hello = json.loads(self._p.stdout.readline())
        if hello.get("event") != "hello" or hello.get("protocol") != 1:
            raise RuntimeError(f"unexpected greeting: {hello}")
        self.version = hello["version"]

    def request(self, **req):
        req.setdefault("id", next(self._ids))
        self._p.stdin.write(json.dumps(req, ensure_ascii=False) + "\n")
        self._p.stdin.flush()
        while True:
            line = self._p.stdout.readline()
            if not line:
                raise RuntimeError("usc serve exited")
            msg = json.loads(line)
            if "event" in msg:  # asynchronous notification, not our reply
                self.events.append(msg)
                continue
            if msg.get("id") != req["id"]:
                continue
            if not msg.get("ok"):
                raise UsError(msg)
            return msg

    def search(self, query, **opts):
        """opts: offset, limit (0 = all), sort (name|path|size|mtime|ctime),
        desc, fields, time (iso|filetime|unix_ms), wait_ms."""
        return self.request(cmd="search", query=query, **opts)

    def info(self):
        return self.request(cmd="info")

    def subscribe(self):
        """Index/status events are collected in self.events from now on."""
        return self.request(cmd="subscribe")

    def close(self):
        try:
            self.request(cmd="quit")
        except Exception:
            pass
        self._p.wait(timeout=15)

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()


if __name__ == "__main__":
    import sys

    usc = sys.argv[1] if len(sys.argv) > 1 else "usc.exe"
    query = sys.argv[2] if len(sys.argv) > 2 else "notepad.exe"
    with UsClient(usc, live=False) as us:
        r = us.search(query, limit=10, fields=["full", "size", "mtime"])
        print(f"{r['total']} results in {r['ms']} ms")
        for it in r["items"]:
            print(f"{it['size']:>14,}  {it['mtime']}  {it['full']}")
