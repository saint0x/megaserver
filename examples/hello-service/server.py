from http.server import BaseHTTPRequestHandler, HTTPServer
import os
import time


PORT = int(os.environ.get("PORT", "18080"))


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/health":
            body = b"ok\n"
            self.send_response(200)
        else:
            body = (
                f"hello from {os.environ.get('MEGASERVER_SERVICE', 'unknown')} at {int(time.time())}\n"
            ).encode()
            self.send_response(200)
        self.send_header("Content-Type", "text/plain; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, fmt, *args):
        message = "%s - - [%s] %s\n" % (
            self.address_string(),
            self.log_date_time_string(),
            fmt % args,
        )
        print(message, end="", flush=True)


if __name__ == "__main__":
    HTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
