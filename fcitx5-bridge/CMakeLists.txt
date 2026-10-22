# Fcitx5 addon "voiceless-bridge": one plugin library plus its descriptor.
# Built by nix/fcitx5-bridge.nix (the Nix package passes the install
# prefix); the version here is what @PROJECT_VERSION@ in the descriptor
# becomes and mirrors the Nix package version.
cmake_minimum_required(VERSION 3.16)
project(voiceless-fcitx5-bridge VERSION 0.2.0 LANGUAGES CXX)

# Fcitx5's CMake config files need extra-cmake-modules on the module path;
# Core 5.1 is the API floor, the DBus component provides the bus module
# the object is registered on.
find_package(Fcitx5Core 5.1 REQUIRED)
find_package(Fcitx5Module REQUIRED COMPONENTS DBus)
include(GNUInstallDirs)

# MODULE: a dlopen'ed plugin nobody links against. PREFIX "lib" fixes the
# file name to libvoiceless-bridge.so, which `Library=` in the descriptor
# refers to.
add_library(voiceless-bridge MODULE voiceless-bridge.cpp)
set_target_properties(voiceless-bridge PROPERTIES PREFIX "lib" CXX_STANDARD 20)
target_link_libraries(voiceless-bridge Fcitx5::Core Fcitx5::Module::DBus)

# lib/fcitx5 and share/fcitx5/addon are the directories Fcitx5 scans for
# addon libraries and descriptors (FCITX_ADDON_DIRS / XDG_DATA_DIRS).
configure_file(voiceless-bridge.conf.in voiceless-bridge.conf @ONLY)
install(TARGETS voiceless-bridge DESTINATION "${CMAKE_INSTALL_LIBDIR}/fcitx5")
install(FILES "${CMAKE_CURRENT_BINARY_DIR}/voiceless-bridge.conf"
        DESTINATION "${CMAKE_INSTALL_DATADIR}/fcitx5/addon")
