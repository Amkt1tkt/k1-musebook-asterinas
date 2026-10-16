# SPDX-License-Identifier: MPL-2.0
# K1 MUSE Book: binary Rogue firmware for BXE-2-32 (BVNC 36.29.52.182).
# Fixed source commit, image hash and redistributable licence travel together.
# This image runs on the GPU firmware processor and is architecture-independent.
# Driver provenance: kernel/core/comps/k1-musebook/pvr/UPSTREAM.

{ lib, stdenvNoCC, fetchurl, }:

let
  rev = "8a58f81883f7be458daa34e418cc4079f995b279";

  raw = path: hash:
    fetchurl {
      url =
        "https://gitlab.freedesktop.org/imagination/linux-firmware/-/raw/${rev}/${path}";
      inherit hash;
    };

  # BVNC 36.29.52.182. The driver checks the BVNC in the image's trailing info
  # header against the one it read out of ROGUE_CR_CORE_ID, so the file name and
  # the hardware have to agree; see pvr/src/fw/info.rs.
  image = "rogue_36.29.52.182_v1.fw";

  # Pinned 126,976-byte image and its accompanying redistribution licence.
  imageFile = raw "powervr/${image}"
    "sha256-HKBt1MchdRtFqwI841eQ1SRcT+0NY23QO5c9H+pHjjA=";
  licenceFile =
    raw "LICENSE.powervr" "sha256-HJqmvWcDp84c24eVQvodisoRWjJ72Bmxk8lx3pxT9AI=";

in stdenvNoCC.mkDerivation {
  pname = "powervr-firmware";
  # The firmware's own version, from the info header: v1.1 build 6976702.
  version = "1.1-6976702";

  dontUnpack = true;
  preferLocalBuild = true;
  allowSubstitutes = false;

  installPhase = ''
    runHook preInstall

    install -Dm444 ${imageFile} $out/lib/firmware/powervr/${image}
    install -Dm444 ${licenceFile} $out/lib/firmware/powervr/LICENSE.powervr

    runHook postInstall
  '';

  # Check installed protocol fields as well as the fixed image hash. The
  # kernel performs the remaining identity, container and processor validation.
  doInstallCheck = true;
  installCheckPhase = ''
    runHook preInstallCheck

    fw=$out/lib/firmware/powervr/${image}
    size=$(stat -c %s "$fw")

    # The info header sits exactly 4096 bytes from the end.
    read_u32() {
      od -An -tu4 -j $(( size - 4096 + $1 )) -N4 "$fw" | tr -d ' \n'
    }

    info_version=$(read_u32 0)
    if [ "$info_version" != 3 ]; then
      echo "powervr-firmware: info_version is $info_version, driver accepts only 3" >&2
      exit 1
    fi

    # flags is at offset 28; bit 0 is PVR_FW_FLAGS_OPEN_SOURCE. The driver
    # refuses images without it, which is what keeps the open-source and DDK
    # firmware stacks from being mixed.
    flags=$(read_u32 28)
    if [ $(( flags & 1 )) -ne 1 ]; then
      echo "powervr-firmware: PVR_FW_FLAGS_OPEN_SOURCE clear in flags=$flags" >&2
      exit 1
    fi

    test -f $out/lib/firmware/powervr/LICENSE.powervr

    runHook postInstallCheck
  '';

  meta = {
    description = "PowerVR Rogue firmware for BXE-2-32 (BVNC 36.29.52.182)";
    homepage = "https://gitlab.freedesktop.org/imagination/linux-firmware";
    # Not in lib.licenses: binary redistribution only, no reverse engineering,
    # and usable only with Imagination-licensed hardware.
    license = {
      shortName = "LICENSE.powervr";
      fullName = "Imagination Technologies firmware licence";
      url =
        "https://gitlab.freedesktop.org/imagination/linux-firmware/-/raw/${rev}/LICENSE.powervr";
      free = false;
      redistributable = true;
    };
    sourceProvenance = [ lib.sourceTypes.binaryFirmware ];
    platforms = lib.platforms.all;
  };
}
