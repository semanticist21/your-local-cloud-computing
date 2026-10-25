#!/usr/bin/env python3
"""Signing diagnostics must retain the failure reason without leaking credentials."""
import importlib.util
import os
from pathlib import Path
import subprocess
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("sign_macos", Path(__file__).with_name("sign-macos.py"))
signing = importlib.util.module_from_spec(spec)
spec.loader.exec_module(signing)


class Diagnostics(unittest.TestCase):
    def test_codesign_error_redacts_credentials_and_identity(self):
        credentials = {name: "sensitive-" + name for name in signing.CREDENTIALS}
        stderr = "\n".join(credentials.values()) + '\nDeveloper ID Application: Example (TEAM)\nerrSecInternalComponent\n'
        result = subprocess.CompletedProcess([], 1, "", stderr)
        with patch.dict(os.environ, credentials), patch.object(subprocess, "run", return_value=result):
            with self.assertRaises(RuntimeError) as error:
                signing.run("codesign", "--force")
        message = str(error.exception)
        self.assertIn("errSecInternalComponent", message)
        self.assertNotIn("Example", message)
        for value in credentials.values():
            self.assertNotIn(value, message)

    def test_other_tools_keep_sensitive_output_private(self):
        result = subprocess.CompletedProcess([], 1, "private material", "private material")
        with patch.object(subprocess, "run", return_value=result):
            with self.assertRaises(RuntimeError) as error:
                signing.run("security", "-i", stdin="private material")
        self.assertNotIn("private material", str(error.exception))


if __name__ == "__main__":
    unittest.main()
