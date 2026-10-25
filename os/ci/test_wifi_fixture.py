"""Unit tests of the CI image's Wi-Fi fixture (os/mkosi/mkosi.profiles/ci/): the files
say what the network e2e relies on (stdlib unittest, plain python3).

    python3 -m unittest discover -s os/ci -p 'test_*.py'

Whether `mac80211_hwsim` exists in the guest kernel, and whether NetworkManager lists
`fantastic-ci`, is the e2e's to find out in QEMU.
"""

import configparser
import unittest
from pathlib import Path

MKOSI = Path(__file__).resolve().parent.parent / "mkosi"
PROFILE = MKOSI / "mkosi.profiles/ci"
EXTRA = PROFILE / "extra/etc"


def keyvalues(path: Path) -> dict[str, str]:
    """`key=value` lines of a hostapd config."""
    return dict(
        ln.split("=", 1) for ln in path.read_text().splitlines() if ln.strip() and not ln.lstrip().startswith("#")
    )


class WifiFixture(unittest.TestCase):
    def test_the_ci_profile_installs_hostapd_and_iw_and_the_base_image_wlr_randr(self):
        ci = (PROFILE / "mkosi.conf").read_text()
        packages = [ln.strip() for ln in ci.split("Packages=")[1].splitlines()[1:] if ln.startswith(" ")]
        self.assertTrue({"hostapd", "iw", "socat", "qemu-guest-agent"} <= set(packages), packages)
        base = (MKOSI / "mkosi.conf").read_text()
        self.assertIn("\n        wlr-randr\n", base)
        # the fixture is CI only: the shipped image has no hostapd
        self.assertNotIn("hostapd", base)

    def test_hostapd_serves_the_wpa2_network_fantastic_ci_on_wlan1(self):
        c = keyvalues(EXTRA / "hostapd/fantastic-ci.conf")
        self.assertEqual(c["interface"], "wlan1")
        self.assertEqual(c["ssid"], "fantastic-ci")
        self.assertEqual((c["wpa"], c["wpa_key_mgmt"], c["rsn_pairwise"]), ("2", "WPA-PSK", "CCMP"))
        self.assertEqual(c["wpa_passphrase"], "fantastic-ci-psk")
        self.assertTrue(8 <= len(c["wpa_passphrase"]) <= 63)

    def test_the_unit_loads_two_radios_then_runs_hostapd_and_is_skipped_without_the_module(self):
        cp = configparser.ConfigParser(strict=False, interpolation=None, delimiters=("=",))
        cp.optionxform = str
        cp.read_string((EXTRA / "systemd/system/fantastic-ci-hwsim.service").read_text())
        self.assertEqual(cp["Service"]["ExecCondition"], "/sbin/modprobe mac80211_hwsim radios=2")
        self.assertEqual(cp["Service"]["ExecStart"], "/usr/sbin/hostapd /etc/hostapd/fantastic-ci.conf")
        self.assertIn("NetworkManager.service", cp["Unit"]["Before"])
        # enabled by a symlink in the profile's tree
        link = EXTRA / "systemd/system/multi-user.target.wants/fantastic-ci-hwsim.service"
        self.assertTrue(link.is_symlink())
        self.assertEqual(link.resolve(), (EXTRA / "systemd/system/fantastic-ci-hwsim.service").resolve())

    def test_networkmanager_leaves_the_access_point_radio_alone(self):
        cp = configparser.ConfigParser(interpolation=None)
        cp.read_string((EXTRA / "NetworkManager/conf.d/90-ci-hwsim.conf").read_text())
        self.assertEqual(cp["keyfile"]["unmanaged-devices"], "interface-name:wlan1")


if __name__ == "__main__":
    unittest.main()
