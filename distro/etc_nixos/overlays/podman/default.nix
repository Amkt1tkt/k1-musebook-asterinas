# SPDX-License-Identifier: MPL-2.0
final: prev:

# K1 MUSE Book --- (retain unsupported mqueue compatibility without weakening state or device rules)
#   runc = prev.runc.overrideAttrs (oldAttrs: {
#     patches = (oldAttrs.patches or [ ]) ++ [
#       ./runc-Disable-creating-dev-mqueue.patch
#       ./runc-Disable-eBPF-for-device-filtering.patch
#       ./runc-Switch-MS_SLAVE-to-MS_PRIVATE.patch
#     ];
#   });
#   podman =
#     (prev.podman.overrideAttrs (oldAttrs: {
#       patches = (oldAttrs.patches or [ ]) ++ [
#         ./Podman-Disable-etc-hosts-and-etc-resolv-conf-injection.patch
#       ];
#     })).override
#       { runc = final.runc; };
# }
{
  runc = prev.runc.overrideAttrs (old: {
    patches = (old.patches or [ ])
      ++ [ ./runc-Disable-creating-dev-mqueue.patch ];
  });
  podman = (prev.podman.overrideAttrs (old: {
    patches = (old.patches or [ ])
      ++ [ ./Podman-Skip-post-network-file-updates.patch ];
  })).override { runc = final.runc; };
}
# K1 MUSE Book ---
