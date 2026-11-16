"""Microsoft Defender result handling. The real scan is checked on a PC with the EICAR test file."""
import subprocess
import unittest
from pathlib import Path
from unittest import mock

from virus_scan import scan

FOUND = ("Scan starting...\nScan finished.\nScanning C:\\Users\\me\\job\\archive.zip found 2 threats.\n"
         "----------------------------- Threat information ------------------------------\n"
         "Threat                  : Virus:DOS/EICAR_Test_File\nResources               : 1 total\n"
         "Threat                  : Virus:DOS/EICAR_Test_File\nThreat                  : Trojan:Win32/Example\n")


class Scan(unittest.TestCase):
    def result(self, code, out=""):
        with mock.patch.object(scan, "defender", lambda: Path("MpCmdRun.exe")), \
                mock.patch.object(scan.subprocess, "run", lambda *a, **k: subprocess.CompletedProcess(a, code, out, "")):
            return scan.scan(r"C:\Users\me\job\archive.zip")

    def test_results_name_the_threat_but_never_the_folder(self):
        self.assertEqual(self.result(0), (True, ""))
        self.assertEqual(self.result(2, FOUND), (False, "Virus:DOS/EICAR_Test_File, Trojan:Win32/Example"))
        self.assertEqual(self.result(2, "odd output"), (False, "Threat found"))
        self.assertEqual(self.result(1, r"Failed C:\Users\me\job\archive.zip"), (None, "Failed archive.zip"))
        self.assertEqual(self.result(5), (None, "Defender exit code 5"))
        with mock.patch.object(scan, "defender", lambda: None):
            self.assertIsNone(scan.scan("x")[0])


if __name__ == "__main__":
    unittest.main()
