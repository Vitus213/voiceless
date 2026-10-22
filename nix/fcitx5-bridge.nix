# Fcitx5 addon package for fcitx5-bridge/ (CMake plus extra-cmake-modules,
# which Fcitx5's CMake config files require). Installs
# lib/fcitx5/libvoiceless-bridge.so and
# share/fcitx5/addon/voiceless-bridge.conf, the layout that
# `i18n.inputMethod.fcitx5.addons` merges into the Fcitx5 profile.
#
# The module is compiled against Fcitx5's C++ headers, and that C++ ABI is
# only stable within one Fcitx5 build, so it must be built with the same
# `fcitx5` package the user runs. The flake therefore builds it twice:
# `packages.voiceless-fcitx5-bridge` against the flake's own nixpkgs (for
# `nix build` and scripts/fcitx5-bridge-test.sh, which also runs that
# nixpkgs' fcitx5 through VOICELESS_FCITX5), and `overlays.default` via
# `final.callPackage` against the consumer's nixpkgs so the addon matches
# the system Fcitx5 (README "Fcitx5 bridge"). `version` mirrors the CMake
# project version.
{
  lib,
  stdenv,
  cmake,
  kdePackages,
  fcitx5,
}:

stdenv.mkDerivation {
  pname = "voiceless-fcitx5-bridge";
  version = "0.2.0";
  src = ../fcitx5-bridge;

  nativeBuildInputs = [
    cmake
    kdePackages.extra-cmake-modules
  ];
  buildInputs = [ fcitx5 ];

  meta = {
    description = "Fcitx5 module that lets Voiceless commit text into the focused input field";
    homepage = "https://github.com/Vitus213/voiceless";
    license = lib.licenses.mit;
    platforms = lib.platforms.linux;
  };
}
