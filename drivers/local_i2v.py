#!/usr/bin/env python3
"""Local image-to-video with diffusers (Hugging Face route, $0 marginal).

Default model is Lightricks/LTX-Video (1.9B) — the pipeline's proven
smoke path on primo's RTX 4070 Laptop 8 GB: model CPU offload plus VAE
tiling, portrait input, 49 frames. Larger models (Wan 2.2 A14B,
HunyuanVideo, MiniMax H3) need the A6000 eGPU and ComfyUI/GGUF tooling;
see references/local-models.md for the per-GPU feasibility matrix
before choosing --model.

Requires torch + diffusers in the active interpreter; both are imported
lazily and a missing dependency is a clear ConfigError, not a traceback
deep inside a run. Weights download from Hugging Face on first use into
the standard HF cache — the download is the slow part, generation is
bounded by --steps and --frames.

Usage (on primo):
    local_i2v.py --image lattice_portrait.jpg \
        --prompt "she smiles, braids sway, slow push-in" --out clip.mp4
    local_i2v.py --image still.png --prompt "…" --dry-run

Exit codes: 0 success (or a successful dry run), 1 expected failure.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))
from i2v_common import (
    ConfigError,
    I2VError,
    ValidationError,
    fail,
)

DEFAULT_MODEL = "Lightricks/LTX-Video"
DEFAULT_NEGATIVE = "blurry, distorted, morphing, warping face, extra fingers, watermark, text"
MAX_DIMENSION_PX = 1_024
MAX_FRAMES = 257
MAX_STEPS = 100
MIN_DIMENSION_PX = 256


def validate(args: argparse.Namespace) -> dict[str, Any]:
    """Validate inputs and return the resolved run configuration."""
    image = Path(args.image)
    if not image.is_file():
        raise ValidationError(f"image file not found: {image}")
    for name in ("width", "height"):
        value = getattr(args, name)
        if not MIN_DIMENSION_PX <= value <= MAX_DIMENSION_PX or value % 32 != 0:
            raise ValidationError(
                f"--{name} must be a multiple of 32 in "
                f"{MIN_DIMENSION_PX}..{MAX_DIMENSION_PX}, got {value}"
            )
    if not (9 <= args.frames <= MAX_FRAMES) or (args.frames - 1) % 8 != 0:
        raise ValidationError(f"--frames must be 8k+1 in 9..{MAX_FRAMES}, got {args.frames}")
    if not 1 <= args.steps <= MAX_STEPS:
        raise ValidationError(f"--steps must be 1..{MAX_STEPS}, got {args.steps}")
    return {
        "model": args.model,
        "image": str(image),
        "prompt": args.prompt,
        "negative_prompt": args.negative_prompt,
        "width": args.width,
        "height": args.height,
        "num_frames": args.frames,
        "num_inference_steps": args.steps,
        "seed": args.seed,
        "fps": args.fps,
        "out": args.out,
        "offload_mode": args.offload_mode,
    }


def run_local(config: dict[str, Any]) -> int:
    """Execute the generation. Imports are lazy on purpose (see docstring)."""
    try:
        import torch
        from diffusers import LTXImageToVideoPipeline
        from diffusers.utils import export_to_video, load_image
    except ImportError as exc:
        raise ConfigError(
            "torch/diffusers not importable in this interpreter; on primo "
            "use ~/img2video-smoke/.venv/bin/python (diffusers 0.41 + "
            "torch 2.14 verified 2026-10-12)"
        ) from exc
    if not torch.cuda.is_available():
        raise ConfigError("no CUDA device visible; local generation needs the GPU")
    image = load_image(config["image"])
    pipe = LTXImageToVideoPipeline.from_pretrained(config["model"], torch_dtype=torch.bfloat16)
    # "sequential" streams layer-by-layer: the only mode that survives an
    # 8 GB card, because LTX's T5-XXL text encoder alone (~11 GB bf16)
    # exceeds it when moved wholesale (proven by the 2026-10-12 smoke:
    # model offload OOM'd inside encode_prompt). "model" moves whole
    # submodules (needs >12 GB). "none" keeps everything resident
    # (A6000-class headroom, fastest).
    if config["offload_mode"] == "sequential":
        pipe.enable_sequential_cpu_offload()
    elif config["offload_mode"] == "model":
        pipe.enable_model_cpu_offload()
    pipe.vae.enable_tiling()
    frames = pipe(
        image=image,
        prompt=config["prompt"],
        negative_prompt=config["negative_prompt"],
        width=config["width"],
        height=config["height"],
        num_frames=config["num_frames"],
        num_inference_steps=config["num_inference_steps"],
        generator=torch.Generator("cuda").manual_seed(config["seed"]),
    ).frames[0]
    export_to_video(frames, config["out"], fps=config["fps"])
    size = Path(config["out"]).stat().st_size
    print(f"wrote {config['out']} ({size} bytes, {len(frames)} frames)")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--image", required=True, help="source still (first frame)")
    parser.add_argument("--prompt", required=True)
    parser.add_argument("--negative-prompt", default=DEFAULT_NEGATIVE)
    parser.add_argument("--model", default=DEFAULT_MODEL)
    parser.add_argument("--width", type=int, default=512)
    parser.add_argument("--height", type=int, default=768)
    parser.add_argument("--frames", type=int, default=49)
    parser.add_argument("--steps", type=int, default=30)
    parser.add_argument("--seed", type=int, default=42)
    parser.add_argument("--fps", type=int, default=24)
    parser.add_argument("--out", default="local-clip.mp4")
    parser.add_argument(
        "--offload-mode",
        choices=("sequential", "model", "none"),
        default="sequential",
        help="sequential = layer streaming (8 GB cards); model = whole-module "
        "offload (>12 GB); none = fully resident (A6000)",
    )
    parser.add_argument(
        "--dry-run", action="store_true", help="validate and print the resolved config; run nothing"
    )
    args = parser.parse_args()
    try:
        config = validate(args)
        if args.dry_run:
            print("DRY RUN — local diffusers: nothing will be loaded or run")
            print(json.dumps(config, indent=2, sort_keys=True))
            return 0
        return run_local(config)
    except I2VError as exc:
        return fail(str(exc))


if __name__ == "__main__":
    sys.exit(main())
