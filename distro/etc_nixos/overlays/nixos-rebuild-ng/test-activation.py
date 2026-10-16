# SPDX-License-Identifier: MPL-2.0
# K1 MUSE Book: unsupported activation must fail before changing system state.

from pathlib import Path
from subprocess import CompletedProcess
from typing import Any
from unittest.mock import Mock

import pytest

import nixos_rebuild as nr
from nixos_rebuild import nix, services
from nixos_rebuild.models import Action, NixOSRebuildError


@pytest.mark.parametrize("action", ["switch", "test", "dry-activate"])
@pytest.mark.parametrize(
    "options",
    [
        [],
        ["--upgrade"],
        ["--upgrade-all"],
        ["--rollback"],
        ["--rollback", "--profile-name", "rescue"],
        ["--flake", "fixture#system"],
        ["--target-host", "root@target", "--sudo"],
        ["--build-host", "root@builder"],
    ],
)
def test_unsupported_activation_has_no_side_effects(
    monkeypatch: pytest.MonkeyPatch, action: str, options: list[str]
) -> None:
    tripwire = Mock(side_effect=AssertionError("activation began before rejection"))
    for module, names in [
        (nr, ["run"]),
        (nix, ["upgrade_channels", "set_profile", "switch_to_configuration"]),
        (services, ["reexec", "write_version_suffix", "build_and_activate_system"]),
    ]:
        for name in names:
            if hasattr(module, name):
                monkeypatch.setattr(module, name, tripwire)

    with pytest.raises(NixOSRebuildError, match="use nixos-rebuild boot, then reboot"):
        nr.execute(["nixos-rebuild", action, *options])

    tripwire.assert_not_called()


def test_boot_activation_runs_directly(monkeypatch: pytest.MonkeyPatch) -> None:
    calls: list[tuple[list[str | Path], dict[str, Any]]] = []

    def run(args: list[str | Path], **kwargs: Any) -> CompletedProcess[str]:
        calls.append((args, kwargs))
        return CompletedProcess(args, 0)

    monkeypatch.setattr(nix, "run_wrapper", run)
    nix.switch_to_configuration(
        Path("/fixture-system"),
        Action.BOOT,
        target_host=None,
        sudo=False,
        install_bootloader=True,
    )

    assert calls[-1][0] == [Path("/fixture-system/bin/switch-to-configuration"), "boot"]
    environment = calls[-1][1].get("extra_env") or calls[-1][1]["env"]
    assert environment["NIXOS_INSTALL_BOOTLOADER"] == "1"
