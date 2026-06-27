#!/usr/bin/env python3
"""
stdio JSON embedding server backed by fastembed.

Protocol (NDJSON, one JSON object per line):
  request:  {"id": <any>, "texts": ["..."], "kind": "query"|"document", "truncateDim": <int|null>}
  response: {"id": <any>, "dim": <int>, "embeddings": [[float, ...], ...]}
  error:    {"id": <any>, "error": "..."}

Lifecycle:
  - Model is loaded lazily on first request (slow first call, fast thereafter).
  - Process exits when stdin closes.
  - Parent (Bun) is expected to spawn a fresh process per call to avoid
    reusing the ONNX session (fastembed has a bug where the second
    `model.embed()` call in the same process hangs on jina-v3).
"""
from __future__ import annotations

import json
import math
import os
import sys
import time
from typing import Any

MODEL_NAME = os.environ.get("EMBED_MODEL", "jinaai/jina-embeddings-v3")
CACHE_DIR = os.environ.get(
    "EMBED_CACHE", os.path.expanduser("~/.cache/fastembed")
)
TASK = os.environ.get("EMBED_TASK", "retrieval")
DEBUG = bool(os.environ.get("EMBED_DEBUG"))

_model: Any = None


def log(msg: str) -> None:
    if DEBUG:
        sys.stderr.write(f"[embed.py] {msg}\n")
        sys.stderr.flush()


def l2norm(vec: list[float]) -> float:
    return math.sqrt(sum(x * x for x in vec))


def normalize(vec: list[float]) -> list[float]:
    n = l2norm(vec)
    if n == 0:
        return vec
    if abs(n - 1.0) <= 0.05:
        return vec
    return [x / n for x in vec]


def get_model() -> Any:
    global _model
    if _model is None:
        log(f"loading {MODEL_NAME} from {CACHE_DIR}")
        t0 = time.time()
        os.makedirs(CACHE_DIR, exist_ok=True)
        from fastembed import TextEmbedding

        _model = TextEmbedding(model_name=MODEL_NAME, cache_dir=CACHE_DIR)
        log(f"loaded in {time.time() - t0:.1f}s")
    return _model


def handle(req: dict[str, Any]) -> dict[str, Any]:
    rid = req.get("id")
    texts = req.get("texts") or []
    truncate_dim = req.get("truncateDim")
    if not isinstance(texts, list) or not texts:
        return {"id": rid, "error": "texts must be non-empty list"}
    if not all(isinstance(t, str) for t in texts):
        return {"id": rid, "error": "all texts must be strings"}
    if texts == [""]:
        return {"id": rid, "error": "texts must not contain empty string"}

    model = get_model()

    try:
        embeddings_iter = model.embed(texts, task=TASK)
        raw = [list(map(float, v)) for v in embeddings_iter]
    except TypeError:
        embeddings_iter = model.embed(texts)
        raw = [list(map(float, v)) for v in embeddings_iter]

    if not raw:
        return {"id": rid, "dim": 0, "embeddings": []}

    if truncate_dim and isinstance(truncate_dim, int) and truncate_dim > 0:
        target = min(truncate_dim, len(raw[0]))
        out: list[list[float]] = []
        for v in raw:
            t = v[:target]
            n = l2norm(t)
            if n > 0:
                t = [x / n for x in t]
            out.append(t)
        raw = out
    else:
        raw = [normalize(v) for v in raw]

    dim = len(raw[0]) if raw else 0
    return {"id": rid, "dim": dim, "embeddings": raw}


def main() -> None:
    log(f"ready, model={MODEL_NAME}, cache={CACHE_DIR}")
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            req = json.loads(line)
        except json.JSONDecodeError as e:
            resp: dict[str, Any] = {"id": None, "error": f"invalid json: {e}"}
        else:
            try:
                resp = handle(req)
            except Exception as e:  # noqa: BLE001
                resp = {"id": req.get("id"), "error": f"{type(e).__name__}: {e}"}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()


if __name__ == "__main__":
    main()
