#!/usr/bin/env python3
"""Convert an MP4 file to GIF, optionally trimming it to a given duration."""

import argparse
import shlex
import subprocess
import sys
from pathlib import Path


# Two-pass palette filter produces noticeably better GIFs than the default
# 256-color quantization. fps is capped to 15 because going higher inflates
# file size a lot while not improving perceived smoothness on GIF; pass
# `--fps` to override.
DEFAULT_FPS = 15


def build_filter(fps: int, max_width: int | None) -> str:
    parts = [f"fps={fps}"]
    if max_width is not None:
        parts.append(f"scale={max_width}:-1:flags=lanczos")
    parts.append("split[s0][s1];[s0]palettegen[p];[s1][p]paletteuse")
    return ",".join(parts)


def build_ffmpeg_command(
    input_path: Path,
    output_path: Path,
    duration: float | None,
    fps: int,
    max_width: int | None,
) -> list[str]:
    """Compose the ffmpeg invocation.

    `-t` is placed before `-i` so it limits how much of the input is decoded,
    which is faster and cleaner than trimming on the output side.
    """
    cmd: list[str] = ["ffmpeg", "-y"]
    if duration is not None:
        cmd += ["-t", str(duration)]
    cmd += [
        "-i",
        str(input_path),
        "-vf",
        build_filter(fps, max_width),
        str(output_path),
    ]
    return cmd


def convert(
    input_path: Path,
    duration: float | None,
    fps: int,
    max_width: int | None,
) -> Path:
    """Run ffmpeg to produce a GIF next to the input file. Returns output path."""
    if not input_path.is_file():
        raise FileNotFoundError(f"Input file does not exist: {input_path}")

    output_path = input_path.with_suffix(".gif")
    cmd = build_ffmpeg_command(input_path, output_path, duration, fps, max_width)

    print("Running:", " ".join(shlex.quote(part) for part in cmd))
    result = subprocess.run(cmd)
    if result.returncode != 0:
        raise RuntimeError(f"ffmpeg failed with exit code {result.returncode}")

    return output_path


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Convert an MP4 file to GIF, optionally trimming to a given duration.",
    )
    parser.add_argument(
        "mp4_path",
        type=Path,
        help="Path to the input MP4 file. The output GIF is written next to it with the same name.",
    )
    parser.add_argument(
        "--duration",
        type=float,
        default=None,
        help="Optional duration in seconds; the video is trimmed to this length before conversion.",
    )
    parser.add_argument(
        "--fps",
        type=int,
        default=DEFAULT_FPS,
        help=f"Frames per second of the output GIF (default: {DEFAULT_FPS}).",
    )
    parser.add_argument(
        "--max-width",
        type=int,
        default=None,
        help="If set, downscale the GIF so its width does not exceed this value; height is computed automatically.",
    )
    args = parser.parse_args()

    try:
        output_path = convert(args.mp4_path, args.duration, args.fps, args.max_width)
    except (FileNotFoundError, RuntimeError) as exc:
        print(f"Error: {exc}", file=sys.stderr)
        return 1

    print(f"Saved: {output_path}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
