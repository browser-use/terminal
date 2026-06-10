from __future__ import annotations

import os

from setuptools import setup
from setuptools.command.bdist_wheel import bdist_wheel


class PlatformWheel(bdist_wheel):
    def finalize_options(self) -> None:
        super().finalize_options()
        self.root_is_pure = False
        plat_name = os.environ.get("BROWSER_USE_CORE_PLAT_NAME")
        if plat_name:
            self.plat_name = plat_name

    def get_tag(self) -> tuple[str, str, str]:
        plat_name = os.environ.get("BROWSER_USE_CORE_PLAT_NAME")
        if plat_name:
            return "py3", "none", plat_name

        _python, _abi, platform = super().get_tag()
        return "py3", "none", platform


setup(cmdclass={"bdist_wheel": PlatformWheel})
