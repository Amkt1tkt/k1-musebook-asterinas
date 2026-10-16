{
  config,
  lib,
  pkgs,
  options,
  ...
}:
let
  # K1 MUSE Book --- (kernel is a GPT partition on K1, not in this closure)
  # kernel = builtins.path {
  #   name = "asterinas-osdk-bin";
  #   path = config.aster_nixos.kernel;
  # };
  kernel =
    if config.aster_nixos.kernel == null then
      null
    else
      builtins.path {
        name = "asterinas-osdk-bin";
        path = config.aster_nixos.kernel;
      };
  # K1 MUSE Book ---
  stage-1-init = pkgs.writeShellScript "stage-1-init" ''
    #!/bin/sh
    # SPDX-License-Identifier: MPL-2.0

    NEW_ROOT=""
    NEW_INIT=""
    BREAK=""
    ARGS=""

    for arg in "$@"; do
      case "$arg" in
        root=*)
          NEW_ROOT=''${arg#root=}
          ;;
        init=*)
          NEW_INIT=''${arg#init=}
          ;;
        rd.break=*)
          BREAK=''${arg#rd.break=}
          ;;
        *)
          ARGS="$ARGS $arg"
          ;;
      esac
    done

    if [ "$BREAK" = "1" ]; then
      echo "Breaking into initramfs shell..."
      exec /bin/sh
    fi

    if [ -z "$NEW_ROOT" ] || [ -z "$NEW_INIT" ]; then
      echo "Error: 'root=' and 'init=' parameters are required."
      exit 1
    fi

    mkdir -p /dev
    mount -t devtmpfs devtmpfs /dev
    mkdir -p /dev/pts
    mount -t devpts devpts /dev/pts
    chmod 0666 /dev/pts/ptmx
    ln -s pts/ptmx /dev/ptmx
    mkdir -p /sysroot
    mount -t ext2 "$NEW_ROOT" /sysroot
    mount -t proc none /sysroot/proc
    mount --move /dev /sysroot/dev

    exec switch_root /sysroot "$NEW_INIT" "$ARGS"
  '';

  initramfs = pkgs.makeInitrd {
    contents = [
      {
        object = "${pkgs.busybox}/bin";
        symlink = "/bin";
      }
      {
        object = stage-1-init;
        symlink = "/init";
      }
    # K1 MUSE Book --- (firmware is loaded before switch_root; GPU blob must be in the initrd)
    # ];
    ]
    # `kernel/.../firmware.rs` reads `/lib/firmware/<name>` from the first
    # kernel thread, before stage-1 mounts the real root. Rootfs copies in
    # `modules/gpu.nix` are invisible at that moment. Guarded on the package
    # existing so this boot-critical module does not require the GPU overlay.
    ++ lib.optional (pkgs ? powervr-firmware) {
      object = "${pkgs.powervr-firmware}/lib/firmware";
      symlink = "/lib/firmware";
    };
    # K1 MUSE Book ---
  };
in
{
  boot.loader.grub.enable = true;
  boot.loader.grub.configurationLimit = 1;
  boot.loader.grub.efiSupport = true;
  boot.loader.grub.device = "nodev";
  boot.loader.grub.efiInstallAsRemovable = true;
  boot.initrd.enable = false;
  boot.kernel.enable = false;
  # Hook function will be called in stage-2-init and before running systemd.
  boot.postBootCommands = ''
    echo "Executing postBootCommands..."

    # The Asterinas Makefile and build scripts run /bin/bash, which NixOS does not create.
    # /bin/sh is bash on NixOS, so a link to it is enough.
    ln -sfn sh /bin/bash

    if [ "${config.aster_nixos.disable-systemd}" = "true" ]; then
      ${config.aster_nixos.stage-2-hook}
    fi
  '';
  # Suppress error and warning messages of systemd.
  # TODO: Fix errors and warnings from systemd and remove this setting.
  environment.sessionVariables = {
    SYSTEMD_LOG_LEVEL = "crit";
  };
  system.systemBuilderCommands = ''
    echo "PATH=/bin:/nix/var/nix/profiles/system/sw/bin earlycon loglevel=${config.aster_nixos.log-level} console=${config.aster_nixos.console} systemd.getty_auto=no -- root=/dev/vda2 init=/nix/var/nix/profiles/system/init rd.break=${
      if config.aster_nixos.break-into-stage-1-shell then "1" else "0"
    }"  > $out/kernel-params
    sed -i 's_^\([[:space:]]*\)\(exec > >(tee -i /run/log/stage-2-init.log) 2>&1\)$_\1# \2_' $out/init
    if [ "${config.aster_nixos.disable-systemd}" = "true" ]; then
      sed -i 's/^[[:space:]]*echo "starting systemd..."$/# &/' $out/init
      sed -i 's/^[[:space:]]*exec \/run\/current-system\/systemd\/lib\/systemd\/systemd "$@"$/# &/' $out/init
    fi
    # K1 MUSE Book --- (no kernel in the closure when aster_nixos.kernel is null)
    # A '#' line is still Nix-interpolated; ''${ keeps the original as text.
    # ln -s ''${kernel} $out/kernel
    ${lib.optionalString (kernel != null) "ln -s ${kernel} $out/kernel"}
    # K1 MUSE Book ---
    ln -s ${initramfs}/initrd $out/initrd
  '';
  system.activationScripts.modprobe = lib.mkForce "";

  nix.nixPath = [
    # `pkgs.path` is a path value, and Nix copies a path value into the store
    # whenever it is interpolated into a string. For nixpkgs, that copies the whole
    # source tree on every evaluation and makes `nixos-rebuild` on Asterinas very slow.
    # `builtins.storePath` turns `pkgs.path` into a reference to its existing store path,
    # which also keeps nixpkgs in the system closure for `<nixpkgs>` lookups.
    "nixpkgs=${builtins.storePath pkgs.path}"
  ]
  ++ builtins.filter (entry: !(lib.hasPrefix "nixpkgs=" entry)) options.nix.nixPath.default
  ++ [ "nixpkgs-overlays=/etc/nixos/overlays" ];
  nix.settings = {
    experimental-features = [
      "nix-command"
      "flakes"
    ];
    filter-syscalls = false;
    require-sigs = false;
    sandbox = false;
    # FIXME: Support Nix build users (nixbld*) and remove this setting. For detailed gaps, see
    # <https://github.com/asterinas/asterinas/issues/2672>.
    build-users-group = "";
    substituters = [ "${config.aster_nixos.substituters}" ];
    trusted-public-keys = [ "${config.aster_nixos.trusted-public-keys}" ];
  };

  # FIXME: Currently, during `nixos-rebuild`, `texinfo/install-info` encounters a `SIGBUS`.
  documentation.info.enable = false;
}
