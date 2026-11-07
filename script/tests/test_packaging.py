#!/usr/bin/env python3
"""Packaging contract tests; stdlib only."""
from pathlib import Path
import os
import shutil
import subprocess
import tempfile
import tomllib
import unittest
import plistlib

ROOT = Path(__file__).resolve().parents[2]

class PackagingTests(unittest.TestCase):
    def test_linux_desktop_contract(self):
        p = ROOT / "packaging/linux/laterterm.desktop"
        text = p.read_text()
        self.assertIn("Exec=laterterm", text)
        self.assertIn("Terminal=false", text)
        self.assertIn("Categories=System;TerminalEmulator;", text)
        self.assertIn("StartupWMClass=laterterm", text)
        self.assertIn("Icon=laterterm", text)

    def test_linux_installer_staging_and_existing_protection(self):
        script = ROOT / "script/install-linux-desktop.sh"
        with tempfile.TemporaryDirectory() as td:
            dest = Path(td) / "dest with spaces"
            binary = Path(td) / "bin with spaces/laterterm"
            binary.parent.mkdir()
            binary.write_text("#!/bin/sh\n")
            binary.chmod(0o755)
            r = subprocess.run([script, "--destdir", str(dest), "--binary", str(binary)], text=True, capture_output=True)
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertTrue((dest / "usr/share/applications/laterterm.desktop").exists())
            self.assertTrue((dest / "usr/share/icons/hicolor/256x256/apps/laterterm.png").exists())
            existing = dest / "usr/share/applications/laterterm.desktop"
            before = existing.read_bytes()
            r = subprocess.run([script, "--destdir", str(dest), "--binary", str(binary)], text=True, capture_output=True)
            self.assertNotEqual(r.returncode, 0)
            self.assertEqual(before, existing.read_bytes())

    def test_user_install_exec_quoting_and_xdg_location(self):
        with tempfile.TemporaryDirectory(prefix='laterterm package ') as directory:
            base = Path(directory)
            home = base / 'home with space $quote'
            home.mkdir()
            data = base / 'xdg data'
            binary = base / 'input binary'
            binary.write_text('#!/bin/sh\n')
            binary.chmod(0o755)
            env = os.environ.copy()
            env.update(HOME=str(home), XDG_DATA_HOME=str(data))
            result = subprocess.run([ROOT / 'script/install-linux-desktop.sh', '--binary', binary], env=env, cwd='/', text=True, capture_output=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            desktop = data / 'applications/laterterm.desktop'
            self.assertTrue(desktop.exists())
            line = next(line for line in desktop.read_text().splitlines() if line.startswith('Exec='))
            self.assertTrue(line.startswith('Exec="'))
            self.assertIn('\\\\$', line)
            for size in (16, 24, 32, 48, 64, 128, 256, 512, 1024):
                self.assertEqual((data / f'icons/hicolor/{size}x{size}/apps/laterterm.png').read_bytes(), (ROOT / f'assets/icons/png/laterterm-{size}.png').read_bytes())

    def test_macos_is_independent_of_working_directory_and_has_exact_metadata(self):
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            binary = base / 'native binary'
            binary.write_bytes(b'fixture')
            binary.chmod(0o755)
            result = subprocess.run([ROOT / 'script/package-macos.sh', '--binary', binary, '--output', base / 'output'], cwd='/', text=True, capture_output=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            app = base / 'output/LaterTerm.app'
            metadata = plistlib.loads((app / 'Contents/Info.plist').read_bytes())
            self.assertEqual(metadata['CFBundleExecutable'], 'laterterm')
            self.assertNotIn('@VERSION@', str(metadata))
            self.assertEqual((app / 'Contents/Resources/laterterm.icns').read_bytes(), (ROOT / 'assets/icons/laterterm.icns').read_bytes())

    def test_symlink_parent_is_rejected_without_outside_writes(self):
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            outside = base / 'outside'
            outside.mkdir()
            dest = base / 'dest'
            dest.mkdir()
            (dest / 'usr').symlink_to(outside, target_is_directory=True)
            binary = base / 'binary'
            binary.write_text('#!/bin/sh\n')
            binary.chmod(0o755)
            result = subprocess.run([ROOT / 'script/install-linux-desktop.sh', '--binary', binary, '--destdir', dest], text=True, capture_output=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(list(outside.iterdir()), [])

    def test_linux_percent_in_executable_path_is_rejected_before_writes(self):
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            home = base / 'home%literal'
            home.mkdir()
            binary = base / 'fixture'
            binary.write_text('#!/bin/sh\n')
            binary.chmod(0o755)
            env = os.environ.copy()
            env.update(HOME=str(home), XDG_DATA_HOME=str(base / 'data'))
            result = subprocess.run([ROOT / 'script/install-linux-desktop.sh', '--binary', binary], env=env, text=True, capture_output=True)
            self.assertNotEqual(result.returncode, 0, 'GIO 不能可靠解析 executable 中的百分号，须显式拒绝')
            self.assertFalse((home / '.local').exists())

    def test_file_in_data_parent_fails_before_any_install(self):
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            home = base / 'home'
            home.mkdir()
            data = base / 'not a directory'
            data.write_text('keep')
            binary = base / 'fixture'
            binary.write_text('#!/bin/sh\n')
            binary.chmod(0o755)
            env = os.environ.copy()
            env.update(HOME=str(home), XDG_DATA_HOME=str(data))
            result = subprocess.run([ROOT / 'script/install-linux-desktop.sh', '--binary', binary], env=env, text=True, capture_output=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertFalse((home / '.local').exists(), '预检失败不能留下部分安装')
            self.assertEqual(data.read_text(), 'keep')

    def test_macos_staging(self):
        script = ROOT / "script/package-macos.sh"
        with tempfile.TemporaryDirectory() as td:
            td = Path(td)
            binary = td / "laterterm"
            binary.write_bytes(b"binary")
            binary.chmod(0o755)
            out = td / "out"
            r = subprocess.run([script, "--binary", str(binary), "--output", str(out)], text=True, capture_output=True)
            self.assertEqual(r.returncode, 0, r.stderr)
            plist = out / "LaterTerm.app/Contents/Info.plist"
            self.assertTrue(plist.exists())
            text = plist.read_text()
            self.assertIn("io.github.crazykun.LaterTerm", text)
            self.assertIn("laterterm.icns", text)

    def test_deb_staging_control_and_dpkg_build(self):
        if shutil.which("dpkg-deb") is None:
            self.skipTest("本机无 dpkg-deb")
        script = ROOT / "script/package-deb.sh"
        with tempfile.TemporaryDirectory() as td:
            td = Path(td)
            binary = td / "laterterm fixture"
            binary.write_text("#!/bin/sh\n")
            binary.chmod(0o755)
            staging = td / "staging"
            r = subprocess.run([script, "--binary", str(binary), "--output", str(staging)], text=True, capture_output=True)
            self.assertEqual(r.returncode, 0, r.stderr)
            # control 字段与 workspace Cargo.toml 版本一致（tomllib 读取，不依赖行序）
            version = tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]["package"]["version"]
            control = (staging / "DEBIAN/control").read_text()
            self.assertIn("Package: laterterm\n", control)
            self.assertIn(f"Version: {version}\n", control)
            self.assertIn("Architecture: amd64\n", control)
            self.assertIn("Depends:", control)
            # 暂存树：usr/bin + desktop + hicolor + 随包文档
            self.assertTrue((staging / "usr/bin/laterterm").exists())
            desktop = staging / "usr/share/applications/laterterm.desktop"
            self.assertIn('Exec="/usr/bin/laterterm"', desktop.read_text())
            self.assertTrue((staging / "usr/share/icons/hicolor/256x256/apps/laterterm.png").exists())
            for name in ("README.md", "CHANGELOG.md", "LICENSE"):
                self.assertTrue((staging / f"usr/share/doc/laterterm/{name}").exists())
            # 脚本内组包 + dpkg-deb --info 自校验；这里再独立解析一次
            deb = td / f"laterterm_{version}_amd64.deb"
            self.assertTrue(deb.is_file())
            info = subprocess.run(["dpkg-deb", "--info", str(deb)], text=True, capture_output=True)
            self.assertEqual(info.returncode, 0, info.stderr)
            self.assertIn("Package: laterterm", info.stdout)
            self.assertIn(f"Version: {version}", info.stdout)

    def test_deb_refuses_existing_staging_and_deb(self):
        if shutil.which("dpkg-deb") is None:
            self.skipTest("本机无 dpkg-deb")
        script = ROOT / "script/package-deb.sh"
        with tempfile.TemporaryDirectory() as td:
            td = Path(td)
            binary = td / "laterterm"
            binary.write_text("#!/bin/sh\n")
            binary.chmod(0o755)
            staging = td / "staging"
            first = subprocess.run([script, "--binary", str(binary), "--output", str(staging)], text=True, capture_output=True)
            self.assertEqual(first.returncode, 0, first.stderr)
            version = tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]["package"]["version"]
            deb = td / f"laterterm_{version}_amd64.deb"
            before_deb = deb.read_bytes()
            # 暂存目录已存在 → 拒绝且不碰已有产物
            second = subprocess.run([script, "--binary", str(binary), "--output", str(staging)], text=True, capture_output=True)
            self.assertNotEqual(second.returncode, 0)
            self.assertEqual(deb.read_bytes(), before_deb)
            # 换新暂存目录但 .deb 已存在 → 同样拒绝
            third = subprocess.run([script, "--binary", str(binary), "--output", str(td / "staging2")], text=True, capture_output=True)
            self.assertNotEqual(third.returncode, 0)
            self.assertEqual(deb.read_bytes(), before_deb)

    def test_appimage_dry_run_stages_appdir_without_artifact(self):
        script = ROOT / "script/package-appimage.sh"
        with tempfile.TemporaryDirectory() as td:
            td = Path(td)
            binary = td / "laterterm"
            binary.write_text("#!/bin/sh\n")
            binary.chmod(0o755)
            out = td / "out"
            r = subprocess.run([script, "--binary", str(binary), "--output", str(out), "--dry-run"], text=True, capture_output=True)
            self.assertEqual(r.returncode, 0, r.stderr)
            appdir = out / "LaterTerm.AppDir"
            self.assertTrue((appdir / "usr/bin/laterterm").exists())
            desktop = appdir / "usr/share/applications/laterterm.desktop"
            text = desktop.read_text()
            self.assertIn("Exec=laterterm", text)
            self.assertIn("Icon=laterterm", text)
            self.assertTrue((appdir / "usr/share/icons/hicolor/128x128/apps/laterterm.png").exists())
            # 无 linuxdeploy（或 dry-run）时不产 .AppImage，只停在 AppDir + 元数据校验
            self.assertEqual(list(out.glob("*.AppImage")), [])

if __name__ == "__main__":
    unittest.main()
