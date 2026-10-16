# SPDX-License-Identifier: MPL-2.0
# K1 MUSE Book: static stage-1 checker using complete serial bitmap reads.
{ lib, stdenv, buildPackages, e2fsprogs, glibc, pkg-config, }:
stdenv.mkDerivation {
  pname = "e2fsck-k1-static";
  inherit (e2fsprogs) version src;
  patches = [ ./patches/e2fsprogs-k1-bitmap-threads.patch ];
  depsBuildBuild = [ buildPackages.stdenv.cc ];
  nativeBuildInputs = [ pkg-config ];
  # Keep the same glibc ABI as the existing checker. A static executable lets
  # stage 1 replace just /bin/e2fsck without mixing libext2fs dependencies.
  configureFlags = [
    "--disable-elf-shlibs"
    "--disable-bsd-shlibs"
    "--enable-libuuid"
    "--enable-libblkid"
    "--disable-uuidd"
    "--disable-nls"
    "--disable-fuse2fs"
    "--disable-e2initrd-helper"
  ];
  env.NIX_CFLAGS_COMPILE = lib.optionalString stdenv.hostPlatform.isRiscV64
    "-march=rv64gc -mabi=lp64d";
  buildPhase = ''
    runHook preBuild
    make -j"$NIX_BUILD_CORES" libs
    export NIX_LDFLAGS="$NIX_LDFLAGS -L${glibc.static}/lib"
    make -j"$NIX_BUILD_CORES" -C e2fsck LDFLAGS_STATIC=-static e2fsck.static
    runHook postBuild
  '';
  installPhase = ''
    runHook preInstall
    install -Dm755 e2fsck/e2fsck.static "$out/bin/e2fsck"
    runHook postInstall
  '';
  enableParallelBuilding = true;
  meta = {
    description = "Stage-1 e2fsck with serial bitmap reads for Asterinas";
    license = lib.licenses.gpl2Plus;
    platforms = lib.platforms.linux;
  };
}
