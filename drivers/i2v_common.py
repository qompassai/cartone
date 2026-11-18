#!/usr/bin/env python3
"""Shared helpers for the img2video-pipeline provider CLIs.

Contract:
- API keys come from environment variables, or from a `pass` entry whose
  path is named by a second environment variable. Keys are never printed,
  never written to disk by this module, and a missing key is a hard
  failure (ConfigError) with instructions — the CLIs fail closed.
- All HTTP is bounded: per-request timeout, a shared retry budget with
  backoff, and hard caps on response and download sizes.
- Only the standard library plus `requests` is used.

Failure behavior: every expected failure raises a subclass of I2VError;
callers convert those to a nonzero exit code with the message on stderr.
"""

from __future__ import annotations

import json
import os
import random
import subprocess
import sys
import time
from pathlib import Path
from typing import Any

import requests

MAX_DOWNLOAD_BYTES = 2_147_483_648  # 2 GiB hard cap on any single download
MAX_IMAGE_BYTES = 20_971_520  # 20 MiB cap for a still image input
MAX_POLL_TIMEOUT_S = 3_600  # 1 h ceiling on any provider poll loop
MAX_RESPONSE_BYTES = 67_108_864  # 64 MiB cap on a JSON response body
MAX_RETRIES = 3  # retries after the first attempt, per request
PASS_TIMEOUT_S = 15
REQUEST_TIMEOUT_S = 60

_RETRYABLE_STATUS = frozenset({408, 425, 429, 500, 502, 503, 504})


class I2VError(Exception):
    """Base class for expected, user-facing failures."""


class ConfigError(I2VError):
    """Configuration is missing or unusable (e.g. no API key)."""


class HttpError(I2VError):
    """A provider request failed after the bounded retry budget."""


class ProviderError(I2VError):
    """The provider answered, but the job or payload is unusable."""


class ValidationError(I2VError):
    """Caller input violates a stated bound or contract."""


def resolve_api_key(env_name: str, pass_path_env_name: str) -> str:
    """Return the API key for a provider, or raise ConfigError.

    Resolution order: the environment variable `env_name`; otherwise the
    `pass` entry whose path is stored in `pass_path_env_name`. The key
    value is never logged. Raises ConfigError naming both routes.
    """
    value = os.environ.get(env_name, "").strip()
    if value:
        return value
    pass_path = os.environ.get(pass_path_env_name, "").strip()
    if pass_path:
        try:
            proc = subprocess.run(
                ["pass", "show", pass_path],
                shell=False,
                capture_output=True,
                text=True,
                timeout=PASS_TIMEOUT_S,
                check=False,
            )
        except (OSError, subprocess.TimeoutExpired) as exc:
            raise ConfigError(f"could not read pass entry {pass_path!r}: {exc}") from exc
        key = proc.stdout.strip()
        if proc.returncode == 0 and key:
            return key
        raise ConfigError(
            f"pass entry {pass_path!r} is unavailable (locked agent or missing "
            "entry); export the key in the environment instead"
        )
    raise ConfigError(
        f"API key not configured: set ${env_name}, or set "
        f"${pass_path_env_name} to a `pass` entry path that holds the key"
    )


def resolve_key(env_name: str, pass_path_env_name: str, *, dry_run: bool) -> str:
    """resolve_api_key, with a dry-run escape hatch.

    Real runs fail closed (ConfigError) when no key is configured. A dry
    run instead returns a visible placeholder so the would-be request can
    be inspected before any account exists; the substitution is announced
    on stderr so it can never pass silently.
    """
    try:
        return resolve_api_key(env_name, pass_path_env_name)
    except ConfigError:
        if not dry_run:
            raise
        print(
            f"note: ${env_name} is not configured; a real run would fail "
            "closed here. Dry run continues with a placeholder.",
            file=sys.stderr,
        )
        return "NOT-CONFIGURED"


def redact_headers(headers: dict[str, str]) -> dict[str, str]:
    """Return a copy of headers with credential values replaced."""
    redacted: dict[str, str] = {}
    for name, value in headers.items():
        lowered = name.lower()
        if lowered in {"authorization", "x-api-key"} or "key" in lowered:
            scheme, _, _ = value.partition(" ")
            redacted[name] = f"{scheme} <redacted>" if scheme else "<redacted>"
        else:
            redacted[name] = value
    return redacted


def print_dry_run(
    provider: str,
    method: str,
    url: str,
    headers: dict[str, str],
    body: Any,
    extra_lines: tuple[str, ...] = (),
) -> None:
    """Print the exact request a real run would send (key redacted)."""
    print(f"DRY RUN — {provider}: no request will be sent")
    print(f"{method} {url}")
    print("headers:", json.dumps(redact_headers(headers), indent=2, sort_keys=True))
    print("body:", json.dumps(body, indent=2, sort_keys=True, default=str)[:4_000])
    for line in extra_lines:
        print(line)


class HttpClient:
    """Bounded HTTP client: retries with backoff on transient failures.

    A retry happens only for retryable statuses and transport errors, at
    most MAX_RETRIES times, with exponential backoff and jitter drawn
    from a seeded generator (deterministic per process run by default).
    """

    def __init__(self, *, jitter_seed: int = 1234) -> None:
        self._session = requests.Session()
        self._rng = random.Random(jitter_seed)

    def request_json(
        self,
        method: str,
        url: str,
        *,
        headers: dict[str, str],
        body: Any = None,
    ) -> Any:
        """Send a request and return the decoded JSON payload.

        Raises HttpError on transport failure or a non-2xx status after
        retries, and ProviderError if the body is not bounded JSON.
        """
        last_error: Exception | None = None
        for attempt in range(MAX_RETRIES + 1):
            try:
                response = self._session.request(
                    method,
                    url,
                    headers=headers,
                    json=body,
                    timeout=REQUEST_TIMEOUT_S,
                )
            except requests.RequestException as exc:
                last_error = exc
                self._backoff(attempt)
                continue
            if response.status_code in _RETRYABLE_STATUS and attempt < MAX_RETRIES:
                self._backoff(attempt)
                continue
            if not 200 <= response.status_code < 300:
                snippet = response.text[:500]
                raise HttpError(f"{method} {url} -> HTTP {response.status_code}: {snippet}")
            raw = response.content
            if len(raw) > MAX_RESPONSE_BYTES:
                raise ProviderError(f"response from {url} exceeds size cap")
            try:
                return json.loads(raw.decode("utf-8"))
            except (ValueError, UnicodeDecodeError) as exc:
                raise ProviderError(f"non-JSON response from {url}") from exc
        raise HttpError(f"{method} {url} failed after retries: {last_error}")

    def download(self, url: str, dest: Path, *, headers: dict[str, str]) -> int:
        """Stream `url` to `dest`; return bytes written. Bounded size."""
        with self._session.get(
            url, headers=headers, timeout=REQUEST_TIMEOUT_S, stream=True
        ) as response:
            if not 200 <= response.status_code < 300:
                raise HttpError(f"GET {url} -> HTTP {response.status_code}")
            written = 0
            with dest.open("wb") as handle:
                for chunk in response.iter_content(chunk_size=1 << 16):
                    written += len(chunk)
                    if written > MAX_DOWNLOAD_BYTES:
                        raise ProviderError(f"download from {url} exceeds size cap")
                    handle.write(chunk)
        return written

    def _backoff(self, attempt: int) -> None:
        delay_s = min(2.0**attempt, 16.0) + self._rng.uniform(0.0, 0.5)
        time.sleep(delay_s)


def encode_image_data_uri(path: Path) -> str:
    """Encode a local image file as a bounded base64 data URI."""
    if not path.is_file():
        raise ValidationError(f"image file not found: {path}")
    size = path.stat().st_size
    if size > MAX_IMAGE_BYTES:
        raise ValidationError(f"image exceeds {MAX_IMAGE_BYTES} bytes: {path}")
    suffix = path.suffix.lower()
    media_type = {
        ".jpg": "image/jpeg",
        ".jpeg": "image/jpeg",
        ".png": "image/png",
        ".webp": "image/webp",
    }.get(suffix)
    if media_type is None:
        raise ValidationError(f"unsupported image type: {suffix}")
    import base64

    encoded = base64.b64encode(path.read_bytes()).decode("ascii")
    return f"data:{media_type};base64,{encoded}"


def encode_image_base64(path: Path) -> str:
    """Encode a local image file as raw base64 (no data: prefix)."""
    uri = encode_image_data_uri(path)
    return uri.split(",", 1)[1]


def poll_until(
    description: str,
    *,
    timeout_s: int,
    interval_s: int,
    check: Any,
) -> Any:
    """Call `check()` until it returns a non-None result or time runs out.

    `check` returns None to keep waiting, or a result to stop. Raises
    ProviderError when the deadline passes. Timeout is clamped to
    MAX_POLL_TIMEOUT_S.
    """
    if timeout_s <= 0 or interval_s <= 0:
        raise ValidationError("poll timeout and interval must be positive")
    deadline = time.monotonic() + min(timeout_s, MAX_POLL_TIMEOUT_S)
    while True:
        result = check()
        if result is not None:
            return result
        if time.monotonic() >= deadline:
            raise ProviderError(f"timed out waiting for {description}")
        print(f"… waiting for {description}", file=sys.stderr)
        time.sleep(interval_s)


def fail(message: str) -> int:
    """Print an expected failure and return a nonzero exit code."""
    print(f"error: {message}", file=sys.stderr)
    return 1
