import io
from pathlib import Path
import tarfile
import tempfile
import tomllib
import unittest
import zipfile

from release import ROOT, asset_pairs, release_plan, validate_assets


def manifest_tag(component):
    package = "omni-cli" if component == "client" else "omni-server"
    with (ROOT / "crates" / package / "Cargo.toml").open("rb") as source:
        return f'{component}-v{tomllib.load(source)["package"]["version"]}'


class ReleaseTests(unittest.TestCase):
    def test_components_have_independent_versions_and_target_counts(self):
        self.assertEqual(len(release_plan(manifest_tag("client"))["targets"]), 6)
        self.assertEqual(len(release_plan(manifest_tag("server"))["targets"]), 2)

    def test_mismatch_and_malformed_tags_are_refused(self):
        for tag in ["client-v999999.0.0", "v0.4.2", "server-v999999.0.0", "client-v0.4.2/evil", "client-v0.4.2-rc.1"]:
            with self.subTest(tag=tag), self.assertRaises(ValueError):
                release_plan(tag)

    def fixture(self, directory):
        plan = release_plan(manifest_tag("client"))
        for bare, archive, member in asset_pairs(plan):
            data = bare.encode()
            (directory / bare).write_bytes(data)
            if archive.endswith(".zip"):
                with zipfile.ZipFile(directory / archive, "w") as bundle:
                    bundle.writestr(member, data)
            else:
                with tarfile.open(directory / archive, "w:gz") as bundle:
                    info = tarfile.TarInfo(member)
                    info.size = len(data)
                    info.mode = 0o755
                    bundle.addfile(info, io.BytesIO(data))
        return plan

    def test_complete_assets_and_checksums_round_trip(self):
        with tempfile.TemporaryDirectory() as tmp:
            directory = Path(tmp)
            plan = self.fixture(directory)
            validate_assets(plan, directory)
            validate_assets(plan, directory, checksums=True)

    def test_missing_extra_empty_and_corrupt_assets_fail(self):
        for failure in ["missing", "extra", "empty", "corrupt", "checksums"]:
            with self.subTest(failure=failure), tempfile.TemporaryDirectory() as tmp:
                directory = Path(tmp)
                plan = self.fixture(directory)
                bare = directory / next(asset_pairs(plan))[0]
                if failure == "missing":
                    bare.unlink()
                elif failure == "extra":
                    (directory / "unrelated").write_text("extra")
                elif failure == "empty":
                    bare.write_bytes(b"")
                elif failure == "corrupt":
                    bare.write_bytes(b"different")
                else:
                    validate_assets(plan, directory)
                    (directory / "SHA256SUMS").write_text("incorrect")
                with self.assertRaises(ValueError):
                    validate_assets(plan, directory, checksums=failure == "checksums")


if __name__ == "__main__":
    unittest.main()
