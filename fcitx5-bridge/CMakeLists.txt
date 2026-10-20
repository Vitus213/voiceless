cmake_minimum_required(VERSION 3.16)
project(voiceless-fcitx5-bridge VERSION 0.1.0 LANGUAGES CXX)

find_package(Fcitx5Core 5.1 REQUIRED)
find_package(Fcitx5Module REQUIRED COMPONENTS DBus)
include(GNUInstallDirs)

add_library(voiceless-bridge MODULE voiceless-bridge.cpp)
set_target_properties(voiceless-bridge PROPERTIES PREFIX "lib" CXX_STANDARD 20)
target_link_libraries(voiceless-bridge Fcitx5::Core Fcitx5::Module::DBus)

configure_file(voiceless-bridge.conf.in voiceless-bridge.conf @ONLY)
install(TARGETS voiceless-bridge DESTINATION "${CMAKE_INSTALL_LIBDIR}/fcitx5")
install(FILES "${CMAKE_CURRENT_BINARY_DIR}/voiceless-bridge.conf"
        DESTINATION "${CMAKE_INSTALL_DATADIR}/fcitx5/addon")
