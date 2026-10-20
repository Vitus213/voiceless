{
  lib,
  stdenv,
  cmake,
  kdePackages,
  fcitx5,
}:

stdenv.mkDerivation {
  pname = "voiceless-fcitx5-bridge";
  version = "0.1.0";
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
