{ pkgs }:
{
  kernel_5_15 = pkgs.stdenv.mkDerivation {
    name = "download-kernel-5.15";
    src = pkgs.fetchurl {
      url = "https://github.com/danobi/vmtest/releases/download/test_assets/bzImage-v5.15-fedora38";
      hash = "sha256-nq8W72vuNKCgO1OS6aJtAfg7AjHavRZ7WAkP7X6V610=";
    };
    dontUnpack = true;
    installPhase = ''
      mkdir -p $out
      cp -r $src $out/bzImage
    '';
  };

  kernel_6_0 = pkgs.stdenv.mkDerivation {
    name = "download-kernel-6.0";
    src = pkgs.fetchurl {
      url = "https://github.com/danobi/vmtest/releases/download/test_assets/bzImage-v6.0-fedora38";
      hash = "sha256-ZBBQ0yVUn+Isd2b+a32oMEbNo8T1v46P3rEtZ+1j9Ic=";
    };
    dontUnpack = true;
    installPhase = ''
      mkdir -p $out
      cp -r $src $out/bzImage
    '';
  };

  kernel_6_2 = pkgs.stdenv.mkDerivation {
    name = "download-kernel-6.2";
    src = pkgs.fetchurl {
      url = "https://github.com/danobi/vmtest/releases/download/test_assets/bzImage-v6.2-fedora38";
      hash = "sha256-YO2HEIWTuEEJts9JrW3V7UVR7t4J3+8On+tjdELa2m8=";
    };
    dontUnpack = true;
    installPhase = ''
      mkdir -p $out
      cp -r $src $out/bzImage
    '';
  };

  kernel_6_6 = pkgs.stdenv.mkDerivation {
    name = "download-kernel-6.6";
    src = pkgs.fetchurl {
      url = "https://github.com/danobi/vmtest/releases/download/test_assets/bzImage-v6.6-fedora38";
      hash = "sha256-6Fu16SPBITP0sI3lapkckZna6GKBn2hID038itt82jA=";
    };
    dontUnpack = true;
    installPhase = ''
      mkdir -p $out
      cp -r $src $out/bzImage
    '';
  };

  vmtest = pkgs.rustPlatform.buildRustPackage {
    name = "vmtest";
    src = pkgs.fetchFromGitHub {
      owner = "danobi";
      repo = "vmtest";
      rev = "51f11bf301fea054342996802a16ed21fb5054f4";
      sha256 = "sha256-qtTq0dnDHi1ITfQzKrXz+1dRMymAFBivWpjXntD09+A=";
    };
    cargoHash = "sha256-SHjjCWz4FVVk1cczkMltRVEB3GK8jz2tVABNSlSZiUc=";
    # nativeCheckInputs = [ pkgs.qemu ];

    # There are some errors trying to access `/build/source/tests/*`.
    doCheck = false;

    meta = with pkgs.lib; {
      description = "Helps run tests in virtual machines";
      homepage = "https://github.com/danobi/vmtest/";
      license = licenses.asl20;
      mainProgram = "";
      maintainers = with maintainers; [ ];
      platforms = platforms.linux;
    };
  };
}
