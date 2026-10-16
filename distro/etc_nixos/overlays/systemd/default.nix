# SPDX-License-Identifier: MPL-2.0
final: prev:

# K1 MUSE Book --- (preserve real units and mask missing minimal-build imports)
#   aster_systemd = prev.systemdMinimal.overrideAttrs (old: {
#     patches = (old.patches or [ ]) ++ [
#       ./0001-Skip-mount-state-checking.patch
#       ./0002-Disable-loop-too-fast-warning.patch
#       ./0003-Switch-MS_SLAVE-to-MS_PRIVATE.patch
#     ];
#
#     postInstall = ''
#             ${old.postInstall or ""}
#
#             mkdir -p "$out/example/systemd/system"
#
#             cat > "$out/example/systemd/system/systemd-logind.service" <<'EOF'
#       # placeholder for $out
#       [Unit]
#       Description=systemd-logind (placeholder)
#       EOF
#
#             cat > "$out/example/systemd/system/systemd-user-sessions.service" <<'EOF'
#       # placeholder injected by override
#       [Unit]
#       Description=placeholder systemd-user-sessions (disabled)
#       EOF
#
#             cat > "$out/example/systemd/system/dbus-org.freedesktop.login1.service" <<'EOF'
#       # placeholder for $out
#       [Unit]
#       Description=placeholder dbus-org.freedesktop.login1.service
#       [Service]
#       Type=dbus
#       BusName=org.freedesktop.login1
#       ExecStart=/bin/true
#       EOF
#
#             cat > "$out/example/systemd/system/user@.service" <<'EOF'
#       # placeholder for $out
#       [Unit]
#       Description=placeholder user@.service
#       [Service]
#       Type=oneshot
#       RemainAfterExit=yes
#       ExecStart=/bin/true
#       EOF
#
#             cat > "$out/example/systemd/system/user-runtime-dir@.service" <<'EOF'
#       # placeholder for $out
#       [Unit]
#       Description=placeholder user-runtime-dir@.service
#       [Service]
#       Type=oneshot
#       RemainAfterExit=yes
#       ExecStart=/bin/mkdir -p /run/user/%i
#       EOF
#
#             cat > "$out/example/systemd/system/local-fs.target.wants/tmp.mount" <<'EOF'
#       # placeholder for $out
#       # This file is intentionally empty as a placeholder for tmp.mount
#       EOF
#
#             cat > "$out/example/systemd/system/systemd-firstboot.service" <<'EOF'
#       # placeholder for $out
#       [Unit]
#       Description=placeholder systemd-firstboot
#       [Service]
#       Type=oneshot
#       ExecStart=/bin/true
#       EOF
#
#             cat > "$out/example/systemd/system/systemd-random-seed.service" <<'EOF'
#       # placeholder for $out
#       [Unit]
#       Description=placeholder systemd-random-seed
#       [Service]
#       Type=oneshot
#       ExecStart=/bin/true
#       EOF
#
#             cat > "$out/example/systemd/system/systemd-vconsole-setup.service" <<'EOF'
#       # placeholder for $out
#       [Unit]
#       Description=placeholder systemd-vconsole-setup
#       [Service]
#       Type=oneshot
#       ExecStart=/bin/true
#       EOF
#
#     '';
#   });
# }
{
  aster_systemd = assert prev.lib.assertMsg
    (builtins.elem prev.lib.trivial.release [ "25.05" "26.05" ])
    "Asterinas systemd overlay supports only Nixpkgs 25.05 and 26.05";
    prev.systemdMinimal.overrideAttrs (old: {
      patches = (old.patches or [ ])
        ++ [ ./0001-Skip-mount-state-checking.patch ];
      patchFlags = (old.patchFlags or [ "-p1" ]) ++ [ "--fuzz=0" ];

      postInstall = (old.postInstall or "") + ''
        # NixOS imports these even when disabled; the minimal build omits them.
        # Supply explicit masks rather than services that pretend to implement login1.
        for unit in systemd-logind.service systemd-user-sessions.service dbus-org.freedesktop.login1.service; do
          unit_path="$out/example/systemd/system/$unit"
          if [ ! -e "$unit_path" ] && [ ! -L "$unit_path" ]; then
            ln -s /dev/null "$unit_path"
          fi
        done
      '';
    });
}
# K1 MUSE Book ---
