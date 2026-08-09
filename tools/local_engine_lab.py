#!/usr/bin/env python3
"""Exercise BigTiny's in-process llama.cpp engine end to end.

Phase 2a (docs/ANDROID.md §3) added `LocalEngine`, `LocalProvider`,
`LocalSummarizer` and `POST /api/embeddings`, but nothing in the Kitty UI
reaches them yet. This drives the daemon's HTTP API directly so the engine can
be exercised — and compared against Ollama — before deciding whether it earns
replacing the managed Ollama process in Phase 2b.

Stdlib only, deliberately: no venv, no pip, runs from a clean checkout.

    python tools/local_engine_lab.py --build          # build, test, tear down
    python tools/local_engine_lab.py --ab             # ...and compare vs Ollama
    python tools/local_engine_lab.py --attach http://127.0.0.1:8080

Exit code is 0 only if every check passed.
"""

from __future__ import annotations

import argparse
import json
import math
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from dataclasses import dataclass, field
from pathlib import Path

# Windows consoles default to cp1252, which cannot encode the characters that
# show up in model output (and in any prose we print). Without this a
# successful run dies in `print` with a UnicodeEncodeError — the checks pass
# and the tool still reports failure. `errors="replace"` keeps that impossible.
for _stream in (sys.stdout, sys.stderr):
    try:
        _stream.reconfigure(encoding="utf-8", errors="replace")
    except (AttributeError, OSError):
        pass

REPO = Path(__file__).resolve().parent.parent
DAEMON_CRATE = REPO / "plugins" / "bigtiny_rust"

# Must match `bigtiny_rust::LOCAL_PROVIDER_ID`. A session pins itself to the
# in-process engine by sending this as `provider` at creation.
LOCAL_PROVIDER_ID = "local"

DEFAULT_MODELS_DIR = Path(
    os.environ.get("LOCALAPPDATA", Path.home() / ".local" / "share")
) / "Kitty" / "models"

# Pinned defaults from docs/ANDROID.md §9.
DEFAULT_CHAT_GGUF = "LFM2.5-1.2B-Instruct-Q4_K_M.gguf"
DEFAULT_EMBED_GGUF = "Qwen3-Embedding-0.6B-q4_k_m.gguf"

OLLAMA_BASE = "http://127.0.0.1:11434"

# Reused from examples/local_embed_spike.rs so the harness and the Rust probe
# agree on what "semantically ordered" means.
EMBED_A = "The cat sat on the warm windowsill."
EMBED_B = "A kitten napped in the sunny window."
EMBED_C = "Quarterly amortisation of deferred tax assets."

CHAT_PROMPT = "In one sentence, what is a cat?"


# --------------------------------------------------------------------------
# tiny result/reporting layer
# --------------------------------------------------------------------------

PASS, FAIL, SKIP, INFO = "PASS", "FAIL", "SKIP", "INFO"


@dataclass
class Report:
    rows: list[tuple[str, str, str]] = field(default_factory=list)

    def add(self, status: str, name: str, detail: str = "") -> None:
        self.rows.append((status, name, detail))
        colour = {PASS: "\033[32m", FAIL: "\033[31m", SKIP: "\033[33m", INFO: "\033[36m"}
        reset = "\033[0m" if _colour_ok() else ""
        tint = colour.get(status, "") if _colour_ok() else ""
        print(f"  {tint}{status:<4}{reset} {name}" + (f"  - {detail}" if detail else ""))

    def check(self, name: str, ok: bool, detail: str = "") -> bool:
        self.add(PASS if ok else FAIL, name, detail)
        return ok

    @property
    def failed(self) -> int:
        return sum(1 for s, _, _ in self.rows if s == FAIL)

    def summary(self) -> None:
        counts = {k: sum(1 for s, _, _ in self.rows if s == k) for k in (PASS, FAIL, SKIP)}
        print()
        print(f"  {counts[PASS]} passed, {counts[FAIL]} failed, {counts[SKIP]} skipped")
        if counts[FAIL]:
            print("\n  Failures:")
            for s, name, detail in self.rows:
                if s == FAIL:
                    print(f"    - {name}" + (f": {detail}" if detail else ""))


def _colour_ok() -> bool:
    return sys.stdout.isatty() and os.environ.get("NO_COLOR") is None


def section(title: str) -> None:
    print(f"\n\033[1m{title}\033[0m" if _colour_ok() else f"\n{title}")


# --------------------------------------------------------------------------
# HTTP (stdlib)
# --------------------------------------------------------------------------


def http_json(
    url: str, payload: dict | None = None, timeout: float = 120.0, method: str | None = None
) -> tuple[int, dict | None]:
    """Return (status, parsed-json-or-None). Never raises for HTTP errors —
    a 4xx/5xx body is frequently the thing under test."""
    data = json.dumps(payload).encode() if payload is not None else None
    req = urllib.request.Request(
        url,
        data=data,
        method=method or ("POST" if data else "GET"),
        headers={"Content-Type": "application/json"},
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            body = r.read().decode("utf-8", "replace")
            return r.status, (json.loads(body) if body.strip() else None)
    except urllib.error.HTTPError as e:
        body = e.read().decode("utf-8", "replace")
        try:
            return e.code, json.loads(body)
        except json.JSONDecodeError:
            return e.code, {"error": body[:400]}
    except (urllib.error.URLError, TimeoutError, socket.timeout) as e:
        return 0, {"error": str(e)}


def stream_sse(url: str, payload: dict, timeout: float = 300.0):
    """Yield (elapsed_seconds, event_dict) per `data:` frame.

    BigTiny emits `data: {json}\\n\\n` with no event names
    (`server/events.rs::serialize_sse`), so this only needs to handle that one
    shape.
    """
    data = json.dumps(payload).encode()
    req = urllib.request.Request(
        url, data=data, method="POST", headers={"Content-Type": "application/json"}
    )
    start = time.perf_counter()
    with urllib.request.urlopen(req, timeout=timeout) as r:
        buf = ""
        for raw in r:
            buf += raw.decode("utf-8", "replace")
            while "\n\n" in buf:
                frame, buf = buf.split("\n\n", 1)
                for line in frame.splitlines():
                    if line.startswith("data:"):
                        chunk = line[5:].strip()
                        if chunk:
                            try:
                                yield time.perf_counter() - start, json.loads(chunk)
                            except json.JSONDecodeError:
                                pass


# --------------------------------------------------------------------------
# daemon lifecycle
# --------------------------------------------------------------------------


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def build_daemon(rep: Report) -> Path | None:
    """cargo build --release --features local-engine.

    Sets CMAKE_GENERATOR and LIBCLANG_PATH because `llama-cpp-sys-2` needs
    both on Windows and fails with errors that look like crate bugs rather
    than missing tools (docs/ANDROID.md §11).
    """
    env = dict(os.environ)
    env.setdefault("CMAKE_GENERATOR", "Ninja")
    if "LIBCLANG_PATH" not in env:
        guess = Path(env.get("LOCALAPPDATA", "")) / "kitty-buildtools" / "libclang"
        if (guess / "libclang.dll").exists():
            env["LIBCLANG_PATH"] = str(guess)

    # A daemon left running from an earlier run holds an exclusive lock on the
    # exe, and cargo fails with a bare "Access is denied (os error 5)" that
    # looks nothing like its actual cause. Say so instead.
    stale = daemon_path()
    if stale and _is_locked(stale):
        rep.add(
            FAIL,
            "build daemon",
            f"{stale.name} is locked by a running process - stop it first "
            f"(e.g. taskkill /IM {stale.name} /F)",
        )
        return None

    print("  building (first build compiles llama.cpp - several minutes)...")
    t0 = time.perf_counter()
    proc = subprocess.run(
        ["cargo", "build", "--release", "--features", "local-engine", "--bin", "bigtiny-daemon"],
        cwd=DAEMON_CRATE,
        env=env,
        capture_output=True,
        text=True,
    )
    if proc.returncode != 0:
        tail = "\n".join((proc.stderr or "").strip().splitlines()[-15:])
        rep.add(FAIL, "build daemon", f"cargo exited {proc.returncode}\n{tail}")
        return None
    rep.add(PASS, "build daemon", f"{time.perf_counter() - t0:.0f}s")
    return daemon_path()


def _is_locked(p: Path) -> bool:
    """True if the file can't be opened for writing — on Windows that means a
    running process holds it. Renaming to itself is the cheap portable probe."""
    try:
        os.rename(p, p)
        return False
    except OSError:
        return True


def daemon_path() -> Path | None:
    for name in ("bigtiny-daemon.exe", "bigtiny-daemon"):
        p = DAEMON_CRATE / "target" / "release" / name
        if p.exists():
            return p
    return None


def write_config(tmp: Path, chat: Path | None, embed: Path | None, enabled: bool) -> Path:
    """Minimal YAML — only the `[local]` block matters here.

    Hand-rolled rather than imported: PyYAML isn't stdlib, and this shape is
    three keys deep.
    """
    def esc(p: Path | None) -> str:
        return json.dumps(str(p)) if p else '""'

    cfg = tmp / "bigtiny.yaml"
    cfg.write_text(
        "local:\n"
        f"  enabled: {'true' if enabled else 'false'}\n"
        f"  model_path: {esc(chat)}\n"
        f"  embed_model_path: {esc(embed)}\n"
        "  n_ctx: 2048\n"
        "  embed_n_ctx: 512\n"
        "  embed_pooling: last\n",
        encoding="utf-8",
    )
    return cfg


class Daemon:
    """Spawns the daemon against a throwaway data dir and always cleans up.

    The isolated `BIGTINY_DATA_DIR` matters: without it a test run would write
    sessions and a DB into `~/.bigtiny` alongside real usage.
    """

    def __init__(self, exe: Path, cfg: Path, tmp: Path):
        self.exe, self.cfg, self.tmp = exe, cfg, tmp
        self.port = free_port()
        self.base = f"http://127.0.0.1:{self.port}"
        self.proc: subprocess.Popen | None = None
        self.log = tmp / "daemon.log"

    def __enter__(self) -> "Daemon":
        env = dict(os.environ)
        env["BIGTINY_DATA_DIR"] = str(self.tmp / "data")
        # No secret => auth middleware no-ops (server/middleware.rs), which is
        # what we want for a loopback throwaway instance.
        env.pop("BIGTINY_SECRET", None)
        self._fh = open(self.log, "w", encoding="utf-8")
        self.proc = subprocess.Popen(
            [str(self.exe), "--host", "127.0.0.1", "--port", str(self.port),
             "--config", str(self.cfg)],
            stdout=self._fh, stderr=subprocess.STDOUT, env=env,
        )
        return self

    def wait_ready(self, timeout: float = 60.0) -> bool:
        deadline = time.time() + timeout
        while time.time() < deadline:
            if self.proc and self.proc.poll() is not None:
                return False  # died during startup; caller prints the log
            status, _ = http_json(f"{self.base}/api/health", timeout=2.0)
            if status == 200:
                return True
            time.sleep(0.25)
        return False

    def log_text(self) -> str:
        try:
            return self.log.read_text(encoding="utf-8", errors="replace")
        except OSError:
            return ""

    def __exit__(self, *exc) -> None:
        if self.proc and self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.proc.kill()
        try:
            self._fh.close()
        except Exception:
            pass


# --------------------------------------------------------------------------
# checks
# --------------------------------------------------------------------------


def cosine(a: list[float], b: list[float]) -> float:
    return sum(x * y for x, y in zip(a, b))


def check_embeddings(base: str, rep: Report) -> None:
    section("Embeddings  (POST /api/embeddings)")

    status, body = http_json(f"{base}/api/embeddings", {"model": "e", "prompt": EMBED_A})
    if status != 200 or not isinstance(body, dict) or "embedding" not in body:
        detail = (body or {}).get("error", f"HTTP {status}")
        rep.add(FAIL, "returns an embedding", str(detail))
        # Everything downstream needs a vector; bail rather than cascade.
        return
    va = body["embedding"]
    rep.check("returns an embedding", isinstance(va, list) and len(va) > 0, f"{len(va)} dims")

    finite = all(isinstance(x, (int, float)) and math.isfinite(x) for x in va)
    rep.check("all values finite", finite)
    rep.check("not all zero", any(abs(x) > 1e-6 for x in va))

    norm = math.sqrt(sum(x * x for x in va))
    rep.check("L2-normalised", abs(norm - 1.0) < 1e-3, f"|v| = {norm:.6f}")

    _, bb = http_json(f"{base}/api/embeddings", {"prompt": EMBED_B})
    _, bc = http_json(f"{base}/api/embeddings", {"prompt": EMBED_C})
    if isinstance(bb, dict) and isinstance(bc, dict):
        rel = cosine(va, bb["embedding"])
        unrel = cosine(va, bc["embedding"])
        # The check that catches a backend returning correctly-shaped garbage:
        # constant vectors would pass every assertion above.
        rep.check(
            "semantically ordered", rel > unrel, f"related {rel:.4f} > unrelated {unrel:.4f}"
        )

    _, again = http_json(f"{base}/api/embeddings", {"prompt": EMBED_A})
    if isinstance(again, dict):
        rep.check("deterministic", again["embedding"] == va, "same input -> same vector")

    status, _ = http_json(f"{base}/api/embeddings", {"model": "e"})
    rep.check("missing prompt -> 400", status == 400, f"got {status}")


def check_chat(base: str, rep: Report, sse_timeout: float = 180.0) -> dict | None:
    """Drive a turn through LocalProvider. Returns timing info on success."""
    section(f"Chat  (session pinned to provider={LOCAL_PROVIDER_ID!r})")

    status, body = http_json(
        f"{base}/api/chat/",
        {"name": "local-engine-lab", "provider": LOCAL_PROVIDER_ID},
    )
    sid = (body or {}).get("session_id") if isinstance(body, dict) else None
    if status != 200 or not sid:
        rep.add(FAIL, "create session", f"HTTP {status}: {body}")
        return None
    rep.add(PASS, "create session", sid[:8])

    text, ttft, events, err = "", None, [], None
    t0 = time.perf_counter()
    try:
        for elapsed, ev in stream_sse(
            f"{base}/api/chat/{sid}/send", {"message": CHAT_PROMPT}, timeout=sse_timeout
        ):
            events.append(ev)
            kind = ev.get("type")
            if kind == "llm_delta" and ev.get("content"):
                if ttft is None:
                    ttft = elapsed
                text += ev["content"]
            elif kind in ("error", "provider_error"):
                err = ev.get("content") or ev.get("error") or str(ev)
            # Stop at the terminal event rather than reading to EOF. This is
            # what a real client does (Kitty included), and it keeps the
            # harness honest about latency: the daemon may legitimately hold
            # the connection open after the turn for a follow-up turn on the
            # same stream.
            if kind in ("llm_stop", "error", "provider_error") or ev.get("is_last"):
                break
    except TimeoutError:
        rep.add(FAIL, "stream a turn", f"no terminal event within {sse_timeout:.0f}s")
        rep.add(
            INFO,
            "hint",
            "re-run with --keep and read daemon.log; "
            "`local generation: done` means the engine finished and the stall is downstream.",
        )
        return None
    except Exception as e:  # noqa: BLE001 - surface transport failures as a check
        rep.add(FAIL, "stream a turn", f"{type(e).__name__}: {e}")
        return None
    total = time.perf_counter() - t0

    if err:
        rep.add(FAIL, "stream a turn", err)
        return None

    # The specific regression guard: without the chat template an instruct
    # model emits EOS immediately, producing zero content deltas — which looks
    # exactly like a broken build (docs/ANDROID.md §9).
    if not rep.check("produced text", bool(text.strip()), f"{len(text)} chars"):
        rep.add(INFO, "hint", "zero output is the signature of a missing chat template")
        return None

    print(f"\n      \033[2m{text.strip()[:300]}\033[0m\n" if _colour_ok()
          else f"\n      {text.strip()[:300]}\n")

    stop = next((e for e in reversed(events) if e.get("type") == "llm_stop"), None)
    rep.check("terminal llm_stop event", stop is not None)

    timing = next((e for e in reversed(events) if e.get("type") == "llm_timing"), None)
    out_tokens = None
    if timing:
        out_tokens = timing.get("total_tokens") or (timing.get("timing") or {}).get("total_tokens")
    if out_tokens is None:
        # Fall back to a rough estimate so the benchmark still reports
        # something, but say that it's an estimate.
        out_tokens = max(1, len(text) // 4)
        rep.add(INFO, "token count", f"~{out_tokens} (estimated from text length)")
    else:
        rep.check("reported token count", out_tokens > 0, str(out_tokens))

    # Wall-clock throughput folds in the model load (first request off disk),
    # so also report the daemon's own generation-only figure. The latter is the
    # number to compare against another engine; the former is what a user feels
    # on a cold start.
    gen_ms = (timing or {}).get("generation_ms")
    tps = out_tokens / total if total > 0 else 0.0
    gen_tps = (out_tokens / (gen_ms / 1000.0)) if gen_ms else None
    detail = f"TTFT {ttft:.2f}s, {tps:.1f} tok/s wall, {total:.2f}s total"
    if gen_tps:
        detail += f"  |  {gen_tps:.1f} tok/s generating ({gen_ms / 1000.0:.2f}s)"
    rep.add(INFO, "throughput", detail)
    return {
        "ttft": ttft,
        "tok_s": tps,
        "gen_tok_s": gen_tps,
        "total": total,
        "text": text.strip(),
    }


# --------------------------------------------------------------------------
# Ollama A/B
# --------------------------------------------------------------------------


def ollama_models() -> list[str]:
    status, body = http_json(f"{OLLAMA_BASE}/api/tags", timeout=3.0)
    if status != 200 or not isinstance(body, dict):
        return []
    return [m.get("name", "") for m in body.get("models", [])]


def check_ollama_ab(rep: Report, local: dict | None, model: str | None, chat_gguf: Path) -> None:
    section("A/B vs Ollama")

    status, _ = http_json(f"{OLLAMA_BASE}/api/version", timeout=3.0)
    if status != 200:
        rep.add(SKIP, "Ollama comparison", "Ollama not reachable on :11434")
        return
    tags = ollama_models()
    if not tags:
        rep.add(SKIP, "Ollama comparison", "no models installed")
        return
    if model is None:
        # Try to match the local GGUF so the comparison is engine-vs-engine.
        stem = chat_gguf.stem.lower().split("-")[0]
        model = next((t for t in tags if stem and stem in t.lower()), tags[0])

    same_family = chat_gguf.stem.lower().split("-")[0] in model.lower()
    if not same_family:
        # Say this loudly. Comparing different models measures the models, not
        # the engines, and that distinction decides Phase 2b.
        rep.add(
            INFO,
            "CAVEAT",
            f"Ollama is serving {model!r}, which is NOT the local GGUF "
            f"({chat_gguf.name}). These numbers compare stacks, not engines. "
            f"Pull the same model for a fair test.",
        )

    # Warm the model first. Ollama loads on demand, and a cold load is tens of
    # seconds to minutes — timing it as "time to first token" would report a
    # disk read as an engine characteristic.
    print("  warming Ollama (cold model load is not part of the measurement)...")
    warm_status, _ = http_json(
        f"{OLLAMA_BASE}/api/chat",
        {"model": model, "messages": [{"role": "user", "content": "hi"}],
         "stream": False, "options": {"num_predict": 1}},
        timeout=600.0,
    )
    if warm_status != 200:
        rep.add(SKIP, "Ollama comparison", f"warmup failed (HTTP {warm_status})")
        return

    t0 = time.perf_counter()
    ttft, text, tokens, eval_ns, thinking_chars = None, "", 0, 0, 0
    try:
        data = json.dumps(
            {"model": model, "messages": [{"role": "user", "content": CHAT_PROMPT}], "stream": True}
        ).encode()
        req = urllib.request.Request(
            f"{OLLAMA_BASE}/api/chat", data=data, headers={"Content-Type": "application/json"}
        )
        with urllib.request.urlopen(req, timeout=300) as r:
            for line in r:  # NDJSON, one object per line
                s = line.decode("utf-8", "replace").strip()
                if not s:
                    continue
                obj = json.loads(s)
                msg = obj.get("message") or {}
                piece = msg.get("content", "")
                # A reasoning model streams its scratchpad in `thinking`, not
                # `content`. Counting only `content` reported the time to the
                # first *answer* token as TTFT — 200s+ on a model that thought
                # first — which reads as a stall rather than as reasoning.
                think = msg.get("thinking") or ""
                if think:
                    thinking_chars += len(think)
                if (piece or think) and ttft is None:
                    ttft = time.perf_counter() - t0
                text += piece
                if obj.get("done"):
                    tokens = obj.get("eval_count") or 0
                    eval_ns = obj.get("eval_duration") or 0
    except Exception as e:  # noqa: BLE001
        rep.add(SKIP, "Ollama comparison", f"{type(e).__name__}: {e}")
        return
    total = time.perf_counter() - t0
    tokens = tokens or max(1, len(text) // 4)
    o_tps = tokens / total if total else 0.0
    # Ollama reports its own generation time; prefer it, for the same reason we
    # prefer the daemon's `generation_ms` on our side.
    o_gen_tps = (tokens / (eval_ns / 1e9)) if eval_ns else None

    detail = f"TTFT {ttft or 0:.2f}s, {o_tps:.1f} tok/s wall"
    if o_gen_tps:
        detail += f"  |  {o_gen_tps:.1f} tok/s generating"
    rep.add(INFO, f"ollama ({model})", detail)
    if thinking_chars:
        # Most of `eval_count` was then scratchpad, not answer. Throughput is
        # still comparable; total latency very much is not.
        rep.add(
            INFO,
            "note",
            f"{model} is a reasoning model - it emitted {thinking_chars} chars of "
            f"hidden thinking ({tokens} tokens total) before answering, so its "
            f"end-to-end latency is not comparable to a non-reasoning model.",
        )
    if local:
        l_detail = f"TTFT {local['ttft'] or 0:.2f}s, {local['tok_s']:.1f} tok/s wall"
        if local.get("gen_tok_s"):
            l_detail += f"  |  {local['gen_tok_s']:.1f} tok/s generating"
        rep.add(INFO, "local engine", l_detail)

        # Compare generating-only throughput when both sides reported it —
        # that's engine against engine. Wall-clock includes our cold model
        # load (Ollama's was warmed away above), so it would flatter Ollama.
        l_rate, o_rate = local.get("gen_tok_s"), o_gen_tps
        basis = "generating"
        if not (l_rate and o_rate):
            l_rate, o_rate, basis = local["tok_s"], o_tps, "wall-clock"
        faster = "local" if l_rate > o_rate else "ollama"
        ratio = max(l_rate, o_rate) / max(1e-9, min(l_rate, o_rate))
        rep.add(INFO, "verdict", f"{faster} is {ratio:.2f}x faster ({basis} throughput)")
        print()
        print(f"      local : {local['text'][:200]}")
        print(f"      ollama: {text.strip()[:200]}")


# --------------------------------------------------------------------------
# main
# --------------------------------------------------------------------------


def resolve_gguf(explicit: str | None, models_dir: Path, default_name: str) -> Path | None:
    if explicit:
        p = Path(explicit)
        return p if p.is_file() else None
    p = models_dir / default_name
    return p if p.is_file() else None


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--build", action="store_true", help="cargo build the daemon with --features local-engine first")
    ap.add_argument("--attach", metavar="URL", help="use an already-running daemon instead of spawning one")
    ap.add_argument("--models-dir", default=str(DEFAULT_MODELS_DIR))
    ap.add_argument("--chat-gguf", help=f"default: <models-dir>/{DEFAULT_CHAT_GGUF}")
    ap.add_argument("--embed-gguf", help=f"default: <models-dir>/{DEFAULT_EMBED_GGUF}")
    ap.add_argument("--ab", action="store_true", help="also benchmark against a local Ollama")
    ap.add_argument("--ollama-model", help="model tag for --ab (default: best match, else first)")
    ap.add_argument("--keep", action="store_true", help="keep the temp data dir and daemon log")
    args = ap.parse_args()

    rep = Report()
    print("\033[1mBigTiny local-engine lab\033[0m" if _colour_ok() else "BigTiny local-engine lab")

    if args.attach:
        base = args.attach.rstrip("/")
        section("Daemon")
        status, _ = http_json(f"{base}/api/health", timeout=5.0)
        if not rep.check("health reachable", status == 200, base):
            rep.summary()
            return 1
        chat_gguf = resolve_gguf(args.chat_gguf, Path(args.models_dir), DEFAULT_CHAT_GGUF)
        check_embeddings(base, rep)
        local = check_chat(base, rep)
        if args.ab:
            check_ollama_ab(rep, local, args.ollama_model, chat_gguf or Path(DEFAULT_CHAT_GGUF))
        rep.summary()
        return 1 if rep.failed else 0

    section("Preflight")
    models_dir = Path(args.models_dir)
    chat_gguf = resolve_gguf(args.chat_gguf, models_dir, DEFAULT_CHAT_GGUF)
    embed_gguf = resolve_gguf(args.embed_gguf, models_dir, DEFAULT_EMBED_GGUF)

    def _missing(explicit: str | None, default_name: str) -> str:
        # Name the path actually looked at. Saying "not found in <models-dir>"
        # when the user passed an explicit path sends them to the wrong place.
        return f"not found: {explicit}" if explicit else f"not found: {models_dir / default_name}"

    rep.check("chat GGUF", chat_gguf is not None,
              str(chat_gguf) if chat_gguf else _missing(args.chat_gguf, DEFAULT_CHAT_GGUF))
    rep.check("embed GGUF", embed_gguf is not None,
              str(embed_gguf) if embed_gguf else _missing(args.embed_gguf, DEFAULT_EMBED_GGUF))
    if not chat_gguf or not embed_gguf:
        rep.add(INFO, "hint", "pass --chat-gguf/--embed-gguf, or see docs/ANDROID.md section 9 for the pinned models")
        rep.summary()
        return 1

    exe = build_daemon(rep) if args.build else daemon_path()
    if exe is None:
        rep.add(FAIL, "locate daemon", "no release binary; run with --build")
        rep.summary()
        return 1
    if not args.build:
        rep.add(INFO, "daemon", str(exe))

    tmp = Path(tempfile.mkdtemp(prefix="kitty-local-lab-"))
    try:
        cfg = write_config(tmp, chat_gguf, embed_gguf, enabled=True)
        section("Daemon")
        with Daemon(exe, cfg, tmp) as d:
            if not rep.check("started and healthy", d.wait_ready(), d.base):
                tail = "\n".join(d.log_text().strip().splitlines()[-20:])
                print(f"\n  daemon log tail:\n{tail}\n")
                rep.summary()
                return 1

            log = d.log_text()
            if "local engine registered" in log:
                registered = True
                rep.add(PASS, "local engine registered", "")
            elif "[local].enabled = false" in log:
                registered = False
                rep.add(FAIL, "local engine registered", "daemon says [local].enabled = false")
            else:
                # The overwhelmingly likely cause, and otherwise a confusing
                # cascade of 503s three checks later.
                registered = False
                rep.add(
                    FAIL,
                    "local engine registered",
                    "no registration log - binary probably built without --features local-engine",
                )

            check_embeddings(d.base, rep)

            local = None
            if registered:
                local = check_chat(d.base, rep)
            else:
                # Not just tidiness: an unregistered provider makes the agent
                # loop hold the SSE connection open with nothing to send, so
                # attempting the turn hangs until the socket timeout rather
                # than failing.
                section("Chat")
                rep.add(SKIP, "chat turn", "local provider not registered - would hang, not fail")

            if args.ab:
                check_ollama_ab(rep, local, args.ollama_model, chat_gguf)

            if args.keep:
                rep.add(INFO, "kept", str(tmp))
    finally:
        if not args.keep:
            shutil.rmtree(tmp, ignore_errors=True)

    rep.summary()
    return 1 if rep.failed else 0


if __name__ == "__main__":
    sys.exit(main())
