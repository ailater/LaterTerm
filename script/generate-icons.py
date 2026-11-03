#!/usr/bin/env python3
"""生成 LaterTerm 图标资源。"""
from __future__ import annotations

import argparse
import io
import sys
from pathlib import Path

from PIL import Image

ROOT = Path(__file__).resolve().parents[1]
DEFAULT_SOURCE = ROOT / "assets" / "logo.png"
PNG_SIZES = (16, 24, 32, 48, 64, 128, 256, 512, 1024)
ICO_SIZES = (16, 24, 32, 48, 64, 128, 256)
ICNS_SLOTS = (
    (b"icp4", 16), (b"icp5", 32), (b"icp6", 48),
    (b"ic07", 128), (b"ic08", 256), (b"ic09", 512), (b"ic10", 1024),
    (b"ic11", 32), (b"ic12", 64), (b"ic13", 256), (b"ic14", 512),
)


def png_bytes(image: Image.Image, size: int) -> bytes:
    resized = image.resize((size, size), Image.Resampling.LANCZOS)
    out = io.BytesIO()
    resized.save(out, format="PNG", optimize=False, compress_level=9)
    return out.getvalue()


def container_bytes(image: Image.Image, fmt: str, sizes: tuple[int, ...]) -> bytes:
    if fmt == "ICO":
        out = io.BytesIO()
        image.save(out, format="ICO", sizes=[(s, s) for s in sizes], optimize=False)
        return out.getvalue()
    # ICNS 标准 PNG slots；ic11-ic14 是对应的 Retina 2x 视图。
    blocks = []
    for kind, size in ICNS_SLOTS:
        payload = png_bytes(image, size)
        blocks.append(kind + (len(payload) + 8).to_bytes(4, "big") + payload)
    body = b"".join(blocks)
    return b"icns" + (len(body) + 8).to_bytes(4, "big") + body


def expected_outputs(source: Path, output_dir: Path) -> dict[Path, bytes]:
    with Image.open(source) as image:
        if image.format != "PNG" or image.size != (500, 500) or image.mode != "RGBA":
            raise ValueError("source must be a 500x500 RGBA PNG")
        image.load()
        outputs = {
            output_dir / "png" / f"laterterm-{s}.png": png_bytes(image, s)
            for s in PNG_SIZES
        }
        outputs[output_dir / "laterterm.ico"] = container_bytes(image, "ICO", ICO_SIZES)
        outputs[output_dir / "laterterm.icns"] = container_bytes(image, "ICNS", ())
        return outputs


def main() -> int:
    parser = argparse.ArgumentParser(description="生成 LaterTerm PNG、ICO、ICNS 图标")
    parser.add_argument("--source", type=Path, default=DEFAULT_SOURCE)
    parser.add_argument("--output-dir", type=Path, default=ROOT / "assets" / "icons")
    parser.add_argument("--check", action="store_true", help="只检查输出是否与当前源图一致")
    args = parser.parse_args()
    source = args.source.resolve()
    output_dir = args.output_dir.absolute()
    if output_dir.exists() and output_dir.is_symlink():
        parser.error(f"output directory must not be a symlink: {output_dir}")
    if not source.is_file():
        parser.error(f"source not found: {source}")
    try:
        outputs = expected_outputs(source, output_dir)
    except Exception as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2
    drift = []
    if args.check:
        for path, data in outputs.items():
            if not path.is_file() or path.read_bytes() != data:
                drift.append(path)
        if drift:
            print("drift detected:")
            print("\n".join(str(path) for path in drift))
            return 1
        print("icons are up to date")
        return 0
    for path, data in outputs.items():
        path.parent.mkdir(parents=True, exist_ok=True)
        if path.is_file() and path.read_bytes() == data:
            continue
        path.write_bytes(data)
        print(f"wrote {path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
