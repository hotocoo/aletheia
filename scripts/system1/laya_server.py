#!/usr/bin/env python3
"""System-1 sidecar for the `laya` backend, speaking the Aletheia decision wire (ADR-186).

    GET  /v1/models  -> {"data": [{"id": SERVE_ID}]}          (identity: a port is not a model)
    POST /v1/decide  {state, questions:[{type:choice, instructions, options:[{label,description}]}
                                        |{type:yesno, instructions}]}
                     -> {"answers": [{"label", "confidence"} | {"yes", "confidence"}]}

The backend is one implementation of that wire; the manifest's `backend` names which script serves
it, and nothing in Aletheia's Rust names this one. Binds 127.0.0.1 only; bodies are bounded.

    python3 scripts/system1/laya_server.py CHECKPOINT_DIR --serve-id ID [--port 8091] [--device cpu|mps|cuda]

Requires the backend's Python package: `pip install laya` (brings torch, transformers, safetensors).

CHECKPOINT_DIR is the directory holding `model.safetensors` (the manifest's file) and the
checkpoint's config; `aletheiad model status` prints where the registry found it.
"""
import argparse
import json
import os
import sys
import time
from http.server import BaseHTTPRequestHandler, HTTPServer

MAX_BODY = 256 * 1024  # a console request and its options are a few KB; anything bigger is hostile


def to_laya(q):
    if q.get("type") == "choice":
        opts = q.get("options") or []
        if not opts:
            raise ValueError("a choice needs options")
        return {
            "type": "choice",
            "instructions": str(q.get("instructions", "")),
            "criteria": {str(o["label"]): str(o.get("description") or "") or None for o in opts},
        }
    if q.get("type") == "yesno":
        return {"type": "noul", "instructions": str(q.get("instructions", ""))}
    raise ValueError("unknown question type %r" % q.get("type"))


def from_laya(a):
    if a["type"] == "choice":
        return {"label": a["choice"], "confidence": float(a["confidence"])}
    return {"yes": float(a["noul"]) >= 0.5, "confidence": float(a["confidence"])}


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("checkpoint")
    ap.add_argument("--serve-id", required=True)
    ap.add_argument("--port", type=int, default=8091)
    ap.add_argument("--device", default=os.environ.get("SYSTEM1_DEVICE"))
    a = ap.parse_args()

    import laya  # imported late so `--help` works without the backend installed

    t0 = time.time()
    agent = laya.load(a.checkpoint, device=a.device)
    print("[system1] %s loaded from %s on %s in %.1f s" % (a.serve_id, a.checkpoint, agent.device, time.time() - t0),
          flush=True)

    class Handler(BaseHTTPRequestHandler):
        def _send(self, code, obj):
            body = json.dumps(obj).encode()
            self.send_response(code)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self):
            if self.path in ("/v1/models", "/health"):
                return self._send(200, {"data": [{"id": a.serve_id}]})
            self._send(404, {"error": "not found"})

        def do_POST(self):
            if self.path != "/v1/decide":
                return self._send(404, {"error": "not found"})
            n = int(self.headers.get("Content-Length") or 0)
            if n <= 0 or n > MAX_BODY:
                return self._send(413, {"error": "body must be 1..%d bytes" % MAX_BODY})
            try:
                req = json.loads(self.rfile.read(n))
                state = str(req.get("state", ""))
                qs = {"q%d" % i: to_laya(q) for i, q in enumerate(req.get("questions") or [])}
                if not qs:
                    raise ValueError("no questions")
                res = agent.predict(state, qs)["answers"]
                self._send(200, {"answers": [from_laya(res["q%d" % i]) for i in range(len(qs))]})
            except (ValueError, KeyError, TypeError) as e:
                self._send(400, {"error": str(e)})

        def log_message(self, *_):
            pass

    srv = HTTPServer(("127.0.0.1", a.port), Handler)
    print("[system1] serving %s on 127.0.0.1:%d" % (a.serve_id, a.port), flush=True)
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        sys.exit(0)


if __name__ == "__main__":
    main()
