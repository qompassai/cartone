#!/usr/bin/env python3
"""Cartone wai route driver — SDXL text-to-image through the estate
ComfyUI lane on primo.

This is the standing driver behind cartone's `wai` route. The recipe
is the asset factory's own, taken from
~/.local/share/pax/asset-factory/af.py (wf_sdxl + comfy_queue):
checkpoint waiIllustriousSDXL_v170.safetensors, KSampler
euler_ancestral / normal, CFG 6.0, denoise 1.0, plain txt2img (no
IP-Adapter). The factory's ComfyUI serves on 127.0.0.1:8191
(af.py's COMFY constant; the reap-idle-comfy contract pins the
process shape ~/ComfyUI/.venv/bin/python main.py, port 8191).

If no server answers on 8191, the driver starts one the estate way
(ComfyUI venv python, main.py, pinned by GPU UUID via
CUDA_VISIBLE_DEVICES — the run-wan-prima-a7.sh convention; the pin
names primo's RTX 4070, whose 8 GB holds the SDXL lane, while the
A6000 carries the standing rose server) and leaves it running; the
estate's reap-idle-comfy timer stops an idle server, which is the
designed lifecycle. A server the driver did not start is never
stopped or reconfigured.

Stdlib only. Bounded everywhere: HTTP timeouts, a queue deadline,
a capped download. Exit codes: 0 ok, 2 server start failure,
3 ComfyUI execution error, 4 queue timeout, 5 bad output.
"""

import argparse
import json
import os
import subprocess
import sys
import time
import urllib.parse
import urllib.request

COMFY = "http://127.0.0.1:8191"
COMFY_DIR = os.path.expanduser("~/ComfyUI")
COMFY_PY = os.path.join(COMFY_DIR, ".venv", "bin", "python")
CKPT = "waiIllustriousSDXL_v170.safetensors"
DATA_DIR = os.environ.get(
    "CARTONE_DATA_DIR", os.path.expanduser("~/.local/share/cartone"))
MAX_DOWNLOAD = 64 * 1024 * 1024
START_DEADLINE_S = 300


def http_json(path, payload=None, timeout=30):
    data = None
    headers = {}
    if payload is not None:
        data = json.dumps(payload).encode()
        headers["Content-Type"] = "application/json"
    req = urllib.request.Request(COMFY + path, data=data, headers=headers)
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        return json.loads(resp.read().decode())


def server_up():
    try:
        http_json("/system_stats", timeout=3)
        return True
    except Exception:
        return False


def ensure_server():
    if server_up():
        return
    log_dir = os.path.join(DATA_DIR, "logs")
    os.makedirs(log_dir, exist_ok=True)
    log = open(os.path.join(log_dir, "comfyui-autolaunch.log"), "ab")
    env = dict(os.environ)
    # RTX 4070 by UUID (nvidia-smi index order is not CUDA's order;
    # an index pin landed this server on the wrong card in testing
    # only by luck of enumeration — the UUID cannot drift).
    env["CUDA_VISIBLE_DEVICES"] = "GPU-3281fc60-e4c2-da28-9d27-da51a585c7b9"
    subprocess.Popen(
        [COMFY_PY, "main.py", "--listen", "127.0.0.1", "--port", "8191"],
        cwd=COMFY_DIR, env=env, stdout=log, stderr=log,
        start_new_session=True)
    deadline = time.time() + START_DEADLINE_S
    while time.time() < deadline:
        if server_up():
            return
        time.sleep(2)
    raise SystemExit("comfyui did not answer on 8191 within %ds"
                     % START_DEADLINE_S)


def workflow(prompt, negative, seed, width, height, steps, cfg):
    """af.py wf_sdxl without the IP-Adapter branch: nodes 1-7."""
    return {
        "1": {"class_type": "CheckpointLoaderSimple",
              "inputs": {"ckpt_name": CKPT}},
        "2": {"class_type": "CLIPTextEncode",
              "inputs": {"clip": ["1", 1], "text": prompt}},
        "3": {"class_type": "CLIPTextEncode",
              "inputs": {"clip": ["1", 1], "text": negative}},
        "4": {"class_type": "EmptyLatentImage",
              "inputs": {"width": width, "height": height, "batch_size": 1}},
        "5": {"class_type": "KSampler", "inputs": {
            "model": ["1", 0], "positive": ["2", 0], "negative": ["3", 0],
            "latent_image": ["4", 0], "seed": seed, "steps": steps,
            "cfg": cfg, "sampler_name": "euler_ancestral",
            "scheduler": "normal", "denoise": 1.0}},
        "6": {"class_type": "VAEDecode",
              "inputs": {"samples": ["5", 0], "vae": ["1", 2]}},
        "7": {"class_type": "SaveImage",
              "inputs": {"images": ["6", 0], "filename_prefix": "cartone/img"}},
    }


def queue_and_fetch(wf, out_path, timeout_s):
    """af.py comfy_queue's poll loop; the produced image is fetched
    over /view instead of the factory's output-dir convention, so
    the driver does not care where the server keeps its outputs."""
    pid = http_json("/prompt", {"prompt": wf}, timeout=60)["prompt_id"]
    deadline = time.time() + timeout_s
    while time.time() < deadline:
        time.sleep(3)
        hist = http_json("/history/" + pid, timeout=30)
        if pid not in hist:
            continue
        entry = hist[pid]
        status = entry.get("status", {})
        if status.get("status_str") == "error":
            print("comfy execution error: %s"
                  % json.dumps(status)[:600], file=sys.stderr)
            return 3
        for _nid, outputs in entry.get("outputs", {}).items():
            for image in outputs.get("images", []):
                query = urllib.parse.urlencode({
                    "filename": image["filename"],
                    "subfolder": image.get("subfolder", ""),
                    "type": image.get("type", "output")})
                req = urllib.request.Request(COMFY + "/view?" + query)
                with urllib.request.urlopen(req, timeout=60) as resp:
                    blob = resp.read(MAX_DOWNLOAD + 1)
                if len(blob) > MAX_DOWNLOAD:
                    print("output exceeds download cap", file=sys.stderr)
                    return 5
                if not blob.startswith(b"\x89PNG\r\n\x1a\n"):
                    print("output is not a PNG", file=sys.stderr)
                    return 5
                os.makedirs(os.path.dirname(out_path), exist_ok=True)
                with open(out_path, "wb") as fh:
                    fh.write(blob)
                print("wrote %s (%d bytes)" % (out_path, len(blob)))
                return 0
    print("comfy prompt %s exceeded %ds" % (pid, timeout_s), file=sys.stderr)
    return 4


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--prompt", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--negative", default="worst quality, low quality, "
                    "blurry, distorted, text, watermark")
    ap.add_argument("--seed", type=int, default=42)
    ap.add_argument("--width", type=int, default=1024)
    ap.add_argument("--height", type=int, default=1024)
    ap.add_argument("--steps", type=int, default=28)
    ap.add_argument("--cfg", type=float, default=6.0)
    ap.add_argument("--timeout", type=int, default=600)
    args = ap.parse_args()

    try:
        ensure_server()
    except SystemExit as exc:
        print(str(exc), file=sys.stderr)
        return 2
    wf = workflow(args.prompt, args.negative, args.seed,
                  args.width, args.height, args.steps, args.cfg)
    return queue_and_fetch(wf, args.out, args.timeout)


if __name__ == "__main__":
    sys.exit(main())
