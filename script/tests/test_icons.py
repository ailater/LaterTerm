import hashlib
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from PIL import Image

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "script" / "generate-icons.py"
SOURCE = ROOT / "assets" / "logo.png"
SOURCE_SHA256 = "53fc30e3f1ffdcb4dfa666e9fda4c2c9bda16445f504928586a7f383482250c8"
PNG_SIZES = (16, 24, 32, 48, 64, 128, 256, 512, 1024)
ICO_SIZES = (16, 24, 32, 48, 64, 128, 256)
ICNS_CHUNKS = ((b"icp4", 16), (b"icp5", 32), (b"icp6", 48), (b"ic07", 128), (b"ic08", 256), (b"ic09", 512), (b"ic10", 1024), (b"ic11", 32), (b"ic12", 64), (b"ic13", 256), (b"ic14", 512))


class IconGenerationTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.source = self.root / "logo.png"
        self.output = self.root / "icons"
        shutil.copyfile(SOURCE, self.source)

    def tearDown(self):
        self.tmp.cleanup()

    def run_generator(self, *args, check=True):
        return subprocess.run(
            [sys.executable, "-I", str(SCRIPT), "--source", str(self.source), "--output-dir", str(self.output), *map(str, args)],
            cwd=ROOT, text=True, capture_output=True, check=check,
        )

    def test_source_manifest_checksum_and_format(self):
        self.assertEqual(hashlib.sha256(self.source.read_bytes()).hexdigest(), SOURCE_SHA256)
        with Image.open(self.source) as image:
            self.assertEqual(image.format, "PNG")
            self.assertEqual((image.size, image.mode), ((500, 500), "RGBA"))

    def test_outputs_are_isolated_and_pixels_match_full_resize(self):
        self.run_generator()
        with Image.open(self.source) as source:
            expected = {s: source.resize((s, s), Image.Resampling.LANCZOS).tobytes() for s in PNG_SIZES}
        for size in PNG_SIZES:
            with Image.open(self.output / "png" / f"laterterm-{size}.png") as image:
                self.assertEqual(image.tobytes(), expected[size])

    def test_containers_validate_sizes_and_icns_chunks(self):
        self.run_generator()
        with Image.open(self.output / "laterterm.ico") as ico:
            self.assertEqual(sorted(ico.ico.sizes()), sorted((s, s) for s in ICO_SIZES))
        data = (self.output / "laterterm.icns").read_bytes()
        self.assertEqual(data[:4], b"icns")
        self.assertEqual(int.from_bytes(data[4:8], "big"), len(data))
        offset = 8
        for kind, size in ICNS_CHUNKS:
            self.assertEqual(data[offset:offset + 4], kind)
            length = int.from_bytes(data[offset + 4:offset + 8], "big")
            self.assertGreaterEqual(length, 8)
            payload = data[offset + 8:offset + length]
            self.assertEqual(length, len(payload) + 8)
            self.assertEqual(payload[:8], b"\x89PNG\r\n\x1a\n")
            with Image.open(__import__("io").BytesIO(payload)) as image:
                self.assertEqual(image.size, (size, size))
            offset += length
        self.assertEqual(offset, len(data))

    def test_missing_and_damaged_outputs_are_detected_without_writing(self):
        self.run_generator()
        target = self.output / "png" / "laterterm-16.png"
        target.unlink()
        result = self.run_generator("--check", check=False)
        self.assertNotEqual(result.returncode, 0)
        target.write_bytes(b"damaged")
        result = self.run_generator("--check", check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(target.read_bytes(), b"damaged")

    def test_check_and_generation_are_byte_stable(self):
        self.run_generator()
        before = {p: p.read_bytes() for p in self.output.rglob("*") if p.is_file()}
        self.assertEqual(self.run_generator("--check").returncode, 0)
        self.run_generator()
        self.assertEqual(before, {p: p.read_bytes() for p in self.output.rglob("*") if p.is_file()})

    def test_symlink_output_is_rejected(self):
        real = self.root / "real"
        real.mkdir()
        self.output.symlink_to(real, target_is_directory=True)
        result = self.run_generator(check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(list(real.iterdir()), [])


if __name__ == "__main__":
    unittest.main()
