#!/usr/bin/env python3
"""CI workflow yml 与打包清单的契约测试。

本地/门禁兜底 Windows/macOS「无法实际构建」的部分：语法错、结构漂移、
产物扩展名缺失在本地 unittest 就红，不等真 tag 后的三平台构建才发现。
"""
from pathlib import Path
import glob
import unittest
import xml.etree.ElementTree as ET

import yaml

ROOT = Path(__file__).resolve().parents[2]
WIX_NS = "http://wixtoolset.org/schemas/v4/wxs"
# 固定 UpgradeCode：一次生成永不改，改了升级链（MajorUpgrade）就断——精确断言防手滑
EXPECTED_UPGRADE_CODE = "064cb4a5-5d90-4cbf-80ae-4719ca4b47f0"


def load_workflow(name: str) -> dict:
    return yaml.safe_load((ROOT / f".github/workflows/{name}").read_text(encoding="utf-8"))


def triggers(data: dict) -> dict:
    # pyyaml 是 YAML 1.1：裸 `on:` 键被解析成布尔 True，兼容两种取法
    return data.get("on", data.get(True, {}))


class WorkflowYamlTests(unittest.TestCase):
    def test_all_workflow_files_parse_as_yaml(self):
        files = sorted(glob.glob(str(ROOT / ".github/workflows/*.yml")))
        self.assertGreaterEqual(len(files), 4, "workflows 目录不应缩水")
        for path in files:
            with self.subTest(workflow=Path(path).name):
                data = yaml.safe_load(Path(path).read_text(encoding="utf-8"))
                self.assertIsInstance(data, dict)

    def test_release_has_all_platform_jobs(self):
        jobs = load_workflow("release.yml")["jobs"]
        for name in ("build-linux", "build-windows", "build-macos", "release"):
            self.assertIn(name, jobs)

    def test_release_collects_every_deliverable_extension(self):
        steps = load_workflow("release.yml")["jobs"]["release"]["steps"]
        collect = next(step for step in steps if step.get("name") == "收集产物")
        run = collect["run"]
        for pattern in ("*.tar.xz", "*.deb", "*.AppImage", "*.dmg", "*.zip", "*.msi", "*.sha256"):
            self.assertIn(pattern, run)

    def test_release_creates_github_release_with_changelog_notes(self):
        steps = load_workflow("release.yml")["jobs"]["release"]["steps"]
        create = next(step for step in steps if "gh release create" in step.get("run", ""))
        self.assertIn("--notes-file", create["run"])

    def test_release_linux_pins_2204_with_python312_and_locked_build(self):
        job = load_workflow("release.yml")["jobs"]["build-linux"]
        self.assertEqual(job["runs-on"], "ubuntu-22.04")
        # 22.04 系统 python3 是 3.10（无 tomllib），必须由 setup-python 提供 3.12
        setups = [step for step in job["steps"] if step.get("uses", "").startswith("actions/setup-python")]
        self.assertTrue(setups and setups[0]["with"]["python-version"] == "3.12")
        build = next(step for step in job["steps"] if step.get("name") == "构建")
        self.assertIn("--locked", build["run"])
        version_step = next(step for step in job["steps"] if step.get("name") == "读取版本号（tomllib + SemVer 校验）")
        self.assertIn("tomllib", version_step["run"])

    def test_release_linux_builds_deb_and_pinned_appimage(self):
        steps = load_workflow("release.yml")["jobs"]["build-linux"]["steps"]
        deb = next(step for step in steps if step.get("name") == "打包 deb")
        self.assertIn("script/package-assets.py deb", deb["run"])
        appimage = next(step for step in steps if step.get("name") == "打包 AppImage（钉版 linuxdeploy）")
        # linuxdeploy 钉具体版本号，不允许追 latest
        self.assertIn("releases/download/1-alpha-20251107-1/", appimage["run"])
        self.assertIn("script/package-appimage.sh", appimage["run"])

    def test_release_macos_uses_stepwise_hdiutil_and_plist_app(self):
        steps = load_workflow("release.yml")["jobs"]["build-macos"]["steps"]
        app = next(step for step in steps if step.get("name") == "组装 .app（Info.plist @VERSION@ 注入 + icns）")
        self.assertIn("script/package-macos.sh", app["run"])
        dmg = next(step for step in steps if step.get("name") == "分步组装 dmg")
        # 分步：固定大小建空镜像 → 挂载 → ditto → 卸载 → convert；一步式 -srcfolder 不允许回归
        for command in ("hdiutil create -size", "hdiutil convert", "hdiutil detach", "ditto"):
            self.assertIn(command, dmg["run"])
        self.assertNotIn("-srcfolder", dmg["run"])

    def test_packaging_steps_pass_absolute_paths(self):
        # 打包脚本强制绝对路径（package-assets.py absolute_path / package-appimage.sh 守卫
        # 对相对路径直接拒绝），yml 传仓库相对路径会在 runner 上必失败——回归防线
        linux_steps = load_workflow("release.yml")["jobs"]["build-linux"]["steps"]
        macos_steps = load_workflow("release.yml")["jobs"]["build-macos"]["steps"]
        deb = next(step for step in linux_steps if step.get("name") == "打包 deb")
        self.assertIn('--binary "$PWD/target/release/laterterm"', deb["run"])
        self.assertIn('--output "$PWD/packaging-staging/deb"', deb["run"])
        appimage = next(step for step in linux_steps if step.get("name") == "打包 AppImage（钉版 linuxdeploy）")
        self.assertIn('--binary "$PWD/target/release/laterterm"', appimage["run"])
        self.assertIn('--output "$PWD/packaging-staging/appimage"', appimage["run"])
        # 脚本会 cd 进输出目录，linuxdeploy 必须绝对路径
        self.assertIn('--linuxdeploy "$PWD/linuxdeploy.AppImage"', appimage["run"])
        lipo = next(step for step in macos_steps if step.get("name") == "lipo 合一")
        self.assertIn('-output "$PWD/laterterm-universal"', lipo["run"])
        app = next(step for step in macos_steps if step.get("name") == "组装 .app（Info.plist @VERSION@ 注入 + icns）")
        self.assertIn('--binary "$PWD/laterterm-universal"', app["run"])
        # 相对形态不得回归（扫描全部 run 步）
        for bad in ("--binary target/", "--binary laterterm-universal", "--output packaging-staging/"):
            for step in (*linux_steps, *macos_steps):
                if "run" in step:
                    self.assertNotIn(bad, step["run"], f"打包路径相对形态回归: {bad}")

    def test_auto_tag_listens_ci_and_dispatches_release(self):
        data = load_workflow("auto-tag.yml")
        self.assertEqual(triggers(data)["workflow_run"]["workflows"], ["CI"])
        steps = data["jobs"]["tag-and-dispatch"]["steps"]
        checkout = next(step for step in steps if step.get("uses") == "actions/checkout@v4")
        # 钉 CI 验证过的提交，防并发合入时 tag 指向未验证 HEAD
        self.assertEqual(checkout["with"]["ref"], "${{ github.event.workflow_run.head_sha }}")
        tag_step = next(step for step in steps if step.get("name") == "比对版本号并打 tag")
        self.assertIn("tomllib", tag_step["run"])
        dispatch = next(step for step in steps if "gh workflow run release.yml" in step.get("run", ""))
        self.assertIn('--ref "$TAG"', dispatch["run"])


class WiXManifestTests(unittest.TestCase):
    def test_wxs_is_wellformed_with_fixed_upgradecode(self):
        root = ET.parse(ROOT / "packaging/windows/laterterm.wxs").getroot()
        self.assertEqual(root.tag, f"{{{WIX_NS}}}Wix")
        package = next(root.iter(f"{{{WIX_NS}}}Package"))
        self.assertEqual(package.attrib["UpgradeCode"], EXPECTED_UPGRADE_CODE)
        self.assertEqual(package.attrib["Name"], "LaterTerm")


if __name__ == "__main__":
    unittest.main()
