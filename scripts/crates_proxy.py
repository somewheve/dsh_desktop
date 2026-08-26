#!/usr/bin/env python3
"""crates_proxy.py — 开发期 crates.io sparse-index + crate 下载的本地 HTTP 代理。

沙箱内 Windows schannel TLS 失败 (SEC_E_NO_CREDENTIALS)，但 Python OpenSSL 可出网。
本代理监听 127.0.0.1:<port>，把 cargo 的请求转发到 crates.io：
  GET /config.json | /index/config.json  -> 代理自产 config（dl 指向本代理）
  GET /index/<path>                      -> https://index.crates.io/<path>   (sparse index)
  GET /dl/<name>/<ver>/download          -> https://static.crates.io/crates/<name>/<ver>.crate

用法: python scripts/crates_proxy.py [port=8899]
注意: 本代理只用于开发期构建，不是 dsh-desktop 的运行期依赖。
"""
import http.server
import sys
import urllib.request
import urllib.error

INDEX_BASE = "https://index.crates.io"
DL_BASE = "https://static.crates.io/crates"


def fetch(url: str):
    req = urllib.request.Request(url, headers={"User-Agent": "cargo/1.99 (dsh-desktop dev proxy)"})
    try:
        with urllib.request.urlopen(req, timeout=120) as resp:
            return resp.status, resp.headers, resp.read()
    except urllib.error.HTTPError as e:
        return e.code, e.headers, e.read()
    except Exception as e:
        return 502, {}, str(e).encode("utf-8", "replace")


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):
        sys.stderr.write("[proxy] " + (fmt % args) + "\n")

    def _send(self, status, body, ctype="text/plain"):
        try:
            self.send_response(status)
            self.send_header("Content-Type", ctype)
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Connection", "keep-alive")
            self.end_headers()
            if self.command != "HEAD":
                self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError):
            pass  # client went away; nothing to do

    def _config(self):
        port = self.server.server_port
        cfg = ('{"dl": "http://127.0.0.1:%d/dl", "api": "http://127.0.0.1:%d"}' % (port, port)).encode()
        self._send(200, cfg, "application/json")

    def do_HEAD(self):
        self.do_GET()

    def do_GET(self):
        path = self.path
        try:
            if path in ("/config.json", "/index/config.json"):
                self._config()
                return
            if path.startswith("/index/"):
                upstream = INDEX_BASE + path[len("/index"):]
                status, headers, body = fetch(upstream)
                self._send(status, body, headers.get("Content-Type", "text/plain"))
                return
            if path.startswith("/dl/"):
                parts = path.split("/")
                if len(parts) == 5 and parts[1] == "dl" and parts[4] == "download":
                    name, ver = parts[2], parts[3]
                    upstream = f"{DL_BASE}/{name}/{name}-{ver}.crate"
                    status, headers, body = fetch(upstream)
                    self._send(status, body, "application/octet-stream")
                    return
                self._send(404, b"bad dl path")
                return
            self._send(404, b"not found")
        except Exception as e:  # never let a handler thread die silently
            sys.stderr.write("[proxy] ERROR on %s: %s\n" % (path, e))
            self._send(500, str(e).encode("utf-8", "replace"))


def main():
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8899
    server = http.server.ThreadingHTTPServer(("127.0.0.1", port), Handler)
    print(f"crates proxy listening on http://127.0.0.1:{port}", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()