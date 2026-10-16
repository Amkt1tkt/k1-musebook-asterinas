# SPDX-License-Identifier: MPL-2.0

final: prev: {
  jtreg =
    let
      # K1 MUSE Book --- (Java compilers and build tools run on the build platform)
      buildPackages = final.buildPackages;
      javaBuildInputs = [
        buildPackages.ant
        buildPackages.openjdk21
      ];
      # K1 MUSE Book ---

      # JT Harness
      # K1 MUSE Book --- (JAR construction does not need a target C compiler)
      # jtharness = prev.stdenv.mkDerivation rec {
      jtharness = prev.stdenvNoCC.mkDerivation rec {
        # K1 MUSE Book ---
        pname = "jtharness";
        version = "6.0-b24";
        src = prev.fetchzip {
          url = "https://github.com/openjdk/jtharness/archive/refs/tags/jt${version}.zip";
          sha256 = "sha256-E0YlUXBVwC+m7ZVF/RYqKXpRneMYY9y3ewtz8+26T/U";
        };
        # K1 MUSE Book --- (Ant invokes the native JDK during a cross build)
        # buildInputs = with prev.pkgs; [ ant openjdk21 ];
        nativeBuildInputs = javaBuildInputs;
        JAVA_HOME = "${buildPackages.openjdk21}";
        # K1 MUSE Book ---
        buildPhase = ''
          ant -DBUILD_DIR=$NIX_BUILD_TOP -f build/build.xml dist
        '';
        installPhase = ''
          mkdir -p $out
          cp -r $NIX_BUILD_TOP/binaries/* $out
        '';
      };

      # AsmTools
      # K1 MUSE Book --- (JAR construction does not need a target C compiler)
      # asmtools = prev.stdenv.mkDerivation rec {
      asmtools = prev.stdenvNoCC.mkDerivation rec {
        # K1 MUSE Book ---
        pname = "asmtools";
        version = "7.0-b09";
        src = prev.fetchzip {
          url = "https://github.com/openjdk/asmtools/archive/refs/tags/${version}.zip";
          sha256 = "sha256-jgrAmfgCPisAEMP9oTCKmdjMeLuBpVPKg4pW9SL/4lY";
        };
        # K1 MUSE Book --- (Ant invokes the native JDK during a cross build)
        # buildInputs = with prev.pkgs; [ ant openjdk21 ];
        nativeBuildInputs = javaBuildInputs;
        JAVA_HOME = "${buildPackages.openjdk21}";
        # K1 MUSE Book ---
        buildPhase = ''
          ant -DBUILD_DIR=$NIX_BUILD_TOP -f build/build.xml release
        '';
        installPhase = ''
          mkdir -p $out
          cp -r $NIX_BUILD_TOP/release/* $out
        '';
      };

      # JUnit Platform Console Standalone (includes JUnit Jupiter, JUnit Vintage, and dependencies)
      # K1 MUSE Book --- (JAR construction does not need a target C compiler)
      # junit = prev.stdenv.mkDerivation rec {
      junit = prev.stdenvNoCC.mkDerivation rec {
        # K1 MUSE Book ---
        pname = "junit";
        version = "1.11.0";

        junit = prev.fetchurl {
          url = "https://repo1.maven.org/maven2/org/junit/platform/junit-platform-console-standalone/${version}/junit-platform-console-standalone-${version}.jar";
          sha256 = "sha256-n06UydMP5lts3JoGe/g6IsckzLDOaHi6I2giXg0wIJU=";
        };

        license = prev.fetchurl {
          # K1 MUSE Book --- (keep the license tied to the selected JUnit release)
          # url = "https://github.com/junit-team/junit-framework/raw/refs/heads/main/LICENSE.md";
          url = "https://raw.githubusercontent.com/junit-team/junit-framework/r5.11.0/LICENSE.md";
          # K1 MUSE Book ---
          sha256 = "sha256-WqTNRMERrdF40cLi/jbVikhAEsgBZ9+SX4Js1k1BG/A";
        };

        buildCommand = ''
          mkdir -p $out/lib

          cp ${license} $out/LICENSE.md
          cp ${junit} $out/lib/junit-platform-console-standalone.jar
        '';
      };

      # TestNG and its dependencies
      # K1 MUSE Book --- (JAR construction does not need a target C compiler)
      # testng = prev.stdenv.mkDerivation rec {
      testng = prev.stdenvNoCC.mkDerivation rec {
        # K1 MUSE Book ---
        pname = "testng";
        version = "7.3.0";

        testng = prev.fetchurl {
          url = "https://repo1.maven.org/maven2/org/testng/testng/${version}/testng-${version}.jar";
          sha256 = "sha256-Y3J0iPlxfVfw0KD+5aH8EKK+nPz/LsOnGHZW1mPAd04";
        };

        license = prev.fetchurl {
          url = "https://github.com/testng-team/testng/raw/refs/tags/${version}/LICENSE.txt";
          sha256 = "sha256-wbnfEnXnafPbqwANHkV6LUsPKOtdpsd+SNw37rogLtc";
        };

        jcommander = prev.fetchurl {
          url = "https://repo1.maven.org/maven2/com/beust/jcommander/1.78/jcommander-1.78.jar";
          sha256 = "sha256-eJHeu4S1+D6b1XWT6+zjOZq74P2TjPMGs1NMV5E7lhU";
        };

        guice = prev.fetchurl {
          url = "https://repo1.maven.org/maven2/com/google/inject/guice/4.2.3/guice-4.2.3.jar";
          sha256 = "sha256-oh5Q/7tn563FtGz3ueGkgPHg8E/UIB3bHGXakSkGAa8";
        };

        buildCommand = ''
          mkdir -p $out/lib

          cp ${license} $out/LICENSE.txt
          cp ${testng} $out/lib/testng.jar
          cp ${jcommander} $out/lib/jcommander.jar
          cp ${guice} $out/lib/guice.jar
        '';
      };
      # K1 MUSE Book --- (the output contains JARs and a target launcher)
      # prev.stdenv.mkDerivation rec {
    in
    prev.stdenvNoCC.mkDerivation rec {
      # K1 MUSE Book ---
      pname = "jtreg";
      version = "7.5.2";
      number = "1";
      src = prev.fetchzip {
        url = "https://github.com/openjdk/jtreg/archive/refs/tags/jtreg-${version}+${number}.zip";
        sha256 = "sha256-/2VOs+ybAG2Axs7IzJ7O0Mtf30Mp0waEojfO5gHSBXg=";
      };

      patches = [
        ./0001-Make-jtreg-use-tools-from-PATH.patch
        # K1 MUSE Book --- (resolve the selected runtime Java without a build-time JDK path)
        ./0002-Resolve-runtime-Java-from-PATH.patch
        # K1 MUSE Book ---
        # K1 MUSE Book --- (preserve strict compilation of the pinned helper sources)
        # ./0002-Drop-Werror-for-helper-classes-on-JDK-21.patch
        # K1 MUSE Book ---
      ];

      JAVATEST_JAR = "${jtharness}/lib/javatest.jar";
      JTHARNESS_NOTICES = "${jtharness}/legal/copyright.txt ${jtharness}/LICENSE";

      ASMTOOLS_JAR = "${asmtools}/lib/asmtools.jar";
      ASMTOOLS_NOTICES = "${asmtools}/LICENSE";

      JUNIT_JARS = "${junit}/lib/junit-platform-console-standalone.jar";
      JUNIT_NOTICES = "${junit}/LICENSE.md";

      TESTNG_JARS = "${testng}/lib/testng.jar ${testng}/lib/jcommander.jar ${testng}/lib/guice.jar";
      TESTNG_NOTICES = "${testng}/LICENSE.txt";

      # K1 MUSE Book --- (jtreg runs javac, java and javadoc during the build)
      # JDKHOME = "${prev.pkgs.openjdk21}";
      JDKHOME = "${buildPackages.openjdk21}";
      # K1 MUSE Book ---
      JAVA_SPECIFICATION_VERSION = "21";

      # K1 MUSE Book --- (all helper programs execute on the build platform)
      # buildInputs = with prev.pkgs; [
      #   ant openjdk21 hostname pandoc perl html-tidy unzip zip
      # ];
      nativeBuildInputs =
        javaBuildInputs
        ++ (with buildPackages; [
          hostname
          pandoc
          perl
          html-tidy
          unzip
          zip
        ]);
      # K1 MUSE Book ---
      buildPhase = ''
        make BUILD_VERSION=${version} BUILD_NUMBER=${number} -C make
      '';
      installPhase = ''
        mkdir -p $out
        cp -r build/images/jtreg/* $out
      '';
    };
}
