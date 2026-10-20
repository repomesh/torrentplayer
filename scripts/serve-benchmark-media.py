#!/usr/bin/env python3
"""Serve one benchmark video on loopback, with HTTP byte ranges."""

import argparse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import urlsplit


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("file", type=Path)
    parser.add_argument("--port", type=int, default=46128)
    args = parser.parse_args()
    media = args.file.resolve(strict=True)

    class Handler(BaseHTTPRequestHandler):
        def do_HEAD(self):
            self.respond(False)

        def do_GET(self):
            self.respond(True)

        def respond(self, body):
            if urlsplit(self.path).path != f"/{media.name}":
                self.send_error(404)
                return
            size = media.stat().st_size
            start, end = 0, size-1
            requested = self.headers.get("Range")
            if requested:
                try:
                    unit, value = requested.split("=", 1)
                    first, last = value.split("-", 1)
                    if unit != "bytes" or "," in value:
                        raise ValueError()
                    if first:
                        start = int(first)
                        end = min(int(last), end) if last else end
                    else:
                        start = max(0, size-int(last))
                    if not 0 <= start <= end < size:
                        raise ValueError()
                except ValueError:
                    self.send_response(416)
                    self.send_header("Content-Range", f"bytes */{size}")
                    self.end_headers()
                    return
            self.send_response(206 if requested else 200)
            self.send_header("Content-Type", "video/mp4")
            self.send_header("Accept-Ranges", "bytes")
            self.send_header("Cache-Control", "no-store")
            self.send_header("Content-Length", str(end-start+1))
            if requested:
                self.send_header("Content-Range", f"bytes {start}-{end}/{size}")
            self.end_headers()
            if body:
                try:
                    with media.open("rb") as stream:
                        stream.seek(start)
                        remaining = end-start+1
                        while remaining:
                            chunk = stream.read(min(65536, remaining))
                            if not chunk:
                                break
                            self.wfile.write(chunk)
                            remaining -= len(chunk)
                except (BrokenPipeError, ConnectionResetError):
                    pass

        def log_message(self, *_):
            pass

    server = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    print(f"Serving {media.name} on loopback port {args.port}", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()


if __name__ == "__main__":
    main()
