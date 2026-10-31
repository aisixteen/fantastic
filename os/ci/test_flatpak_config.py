"""Static tests of the Flatpak packaging (#82): the manifest in packaging/flatpak/ and the
manual `flatpak` job of ci.yml (stdlib unittest, plain python3, no YAML library: the files
are read as text, the way test_container_check.py reads ci.yml).

    python3 -m unittest discover -s os/ci -p 'test_flatpak_config.py'

What flatpak-builder does with the manifest is the CI job's (`-f flatpak=true`); here: what
the files say.
"""

import json
import re
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent.parent
FLATPAK = REPO / "packaging" / "flatpak"
CI = (REPO / ".github" / "workflows" / "ci.yml").read_text()


def app_id():
    return json.loads((REPO / "src" / "app" / "tauri.conf.json").read_text())["identifier"]


def manifest_text():
    return (FLATPAK / f"{app_id()}.yml").read_text()


def scalar(text, key):
    m = re.search(rf"^{re.escape(key)}:\s*'?\"?([^'\"\n#]+?)'?\"?\s*(#.*)?$", text, re.M)
    return m.group(1).strip() if m else None


def ci_job(name):
    """The text of one top-level job of ci.yml, up to the next job."""
    m = re.search(rf"^  {re.escape(name)}:\n((?:    .*\n|\s*\n|  #.*\n)*)", CI, re.M)
    return m.group(1) if m else None


def uncommented(text):
    return "\n".join(l for l in text.splitlines() if not l.lstrip().startswith("#"))


class FlatpakManifest(unittest.TestCase):
    def test_app_id_matches_tauri_identifier(self):
        text = manifest_text()
        self.assertEqual(scalar(text, "app-id"), app_id())
        self.assertEqual(len(list(FLATPAK.glob("*.yml"))), 1, "one manifest")

    def test_runtime_is_pinned_gnome(self):
        text = manifest_text()
        self.assertEqual(scalar(text, "runtime"), "org.gnome.Platform")
        self.assertEqual(scalar(text, "sdk"), "org.gnome.Sdk")
        version = scalar(text, "runtime-version")
        self.assertRegex(version or "", r"^\d+$", "a pinned runtime, never master or latest")
        # the manifest quotes it: unquoted, YAML reads 48 as a number and flatpak-builder wants a string
        self.assertRegex(text, r"(?m)^runtime-version:\s*['\"]\d+['\"]")

    def test_sandbox_is_marked_placeholder_and_wide(self):
        text = manifest_text()
        head, _, tail = text.partition("finish-args:")
        self.assertTrue(tail, "finish-args present")
        self.assertRegex(head[-1200:], r"(?i)placeholder", "the comment above finish-args says it is a placeholder")
        args = re.findall(r"^\s+- (--\S+)", tail.split("\nmodules:")[0], re.M)
        for a in ("--share=network", "--filesystem=home", "--talk-name=org.freedesktop.Flatpak"):
            self.assertIn(a, args)

    def test_build_is_offline(self):
        text = manifest_text()
        code = uncommented(text)
        # only the esbuild module (Go fetches the pinned toolchain and module) has the network
        self.assertEqual(code.count("build-args"), 1, "one module has build-args")
        esb, _, rest = code.partition("- name: fantastic")
        self.assertIn("build-args", esb, "the esbuild module, before fantastic, is the networked one")
        self.assertNotIn("build-args", rest, "no network for the fantastic build: no --share=network in build-args")
        self.assertIn("go install github.com/evanw/esbuild/cmd/esbuild@v0.25.0", esb)
        self.assertIn("GOTOOLCHAIN=go1.23.4", esb)
        self.assertIn("tools/esbuild/linux-x64/esbuild", rest, "fantastic gets the staged esbuild")
        cargo_lines = [l for l in code.splitlines() if re.search(r"\bcargo\b.*\bbuild\b", l)]
        self.assertGreaterEqual(len(cargo_lines), 1, "the CLI sidecar is built with cargo")
        tauri_lines = [l for l in code.splitlines() if "tauri build" in l]
        self.assertEqual(len(tauri_lines), 1, "the app is built by tauri build")
        for l in cargo_lines + tauri_lines:  # tauri hands what follows `--` to its cargo
            self.assertRegex(l, r"--offline.*--locked|--locked.*--offline")
        self.assertIn("CARGO_NET_OFFLINE", code)
        self.assertIn("npm ci --offline", code)
        for name in ("cargo-sources.json", "cargo-sources-cli.json", "node-sources.json"):
            self.assertIn(f"- {name}", code, f"{name} is a source")
            data = json.loads((FLATPAK / name).read_text())
            self.assertIsInstance(data, list)
            self.assertTrue(data)
        # the vendored sources are the lockfiles' crates, both lockfiles
        cli = {s["dest"] for s in json.loads((FLATPAK / "cargo-sources-cli.json").read_text())}
        app = {s["dest"] for s in json.loads((FLATPAK / "cargo-sources.json").read_text())}
        self.assertTrue(any("tauri-2.12.1" in d for d in app))
        self.assertFalse(any("tauri-2.12.1" in d for d in cli))

    def test_installs_both_binaries_desktop_icon_metainfo(self):
        text = manifest_text()
        code = uncommented(text)
        aid = app_id()
        for want in ("/app/bin/fantastic-app", "/app/bin/fantastic\n", f"{aid}.desktop", f"{aid}.png", f"{aid}.metainfo.xml"):
            self.assertIn(want, code + "\n")
        self.assertIn("hicolor/128x128/apps", code)
        self.assertIn("share/applications", code)
        self.assertIn("share/metainfo", code)
        self.assertIn("command: fantastic-app", code)
        desktop = (FLATPAK / f"{aid}.desktop").read_text()
        self.assertIn("Exec=fantastic-app", desktop)
        self.assertIn(f"Icon={aid}", desktop)
        meta = (FLATPAK / f"{aid}.metainfo.xml").read_text()
        self.assertIn(f"<id>{aid}</id>", meta)
        self.assertIn(f'<launchable type="desktop-id">{aid}.desktop</launchable>', meta)
        self.assertIn("AGPL-3.0-or-later", meta)
        self.assertIn("A live canvas for every folder", meta)
        self.assertTrue((REPO / "src" / "app" / "icons" / "128x128.png").is_file())


class CiFlatpakJob(unittest.TestCase):
    def test_input_is_boolean_default_false(self):
        m = re.search(r"^      flatpak:\n((?:        .*\n)+)", CI, re.M)
        self.assertIsNotNone(m, "workflow_dispatch input flatpak")
        self.assertIn("type: boolean", m.group(1))
        self.assertIn("default: false", m.group(1))
        self.assertLess(CI.index("      flatpak:\n"), CI.index("\njobs:\n"), "an input, not a job")

    def test_job_is_gated_by_the_input(self):
        job = ci_job("flatpak")
        self.assertIsNotNone(job, "a flatpak job")
        self.assertRegex(job, r"(?m)^    if: inputs\.flatpak\s*$")
        self.assertRegex(job, r"(?m)^    runs-on: ubuntu-")
        self.assertNotRegex(job, r"(?m)^    needs:")
        manifest = f"packaging/flatpak/{app_id()}.yml"
        self.assertRegex(job, rf"flatpak-builder .*{re.escape(manifest)}")
        self.assertTrue((REPO / manifest).is_file())
        self.assertIn("--disable-rofiles-fuse", job)

    def test_job_uploads_and_publishes_nothing(self):
        job = uncommented(ci_job("flatpak") or "")
        self.assertTrue(job)
        for bad in ("upload-artifact", "build-bundle", "build-export", "gh release", "ostree", "secrets.", "permissions", "flatpak-builder-lint", "--repo="):
            self.assertNotIn(bad, job)

    def test_other_jobs_unchanged_by_flatpak(self):
        for name in re.findall(r"^  ([a-z][a-z0-9_-]*):\n", CI.split("\njobs:\n")[1], re.M):
            if name == "flatpak":
                continue
            job = ci_job(name)
            self.assertNotIn("inputs.flatpak", job, name)
            self.assertNotRegex(job, r"(?m)^    needs:.*flatpak", name)
        self.assertEqual(sorted(re.findall(r"^  ([a-z][a-z0-9_-]*):\n", CI.split("\njobs:\n")[1], re.M)),
                         ["binaries", "container", "e2e", "flatpak", "os", "units"])
        units = ci_job("units")
        self.assertNotRegex(units, r"(?m)^    if:")
        self.assertIn("units_os: ${{ inputs.e2e && 'all' || inputs.units_os }}", units)
        self.assertEqual(CI.count("inputs.flatpak"), 1, "only the flatpak job reads the input")


if __name__ == "__main__":
    unittest.main()
