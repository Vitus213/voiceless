# Voiceless flake: builds the Rust binary (rust/), the Fcitx5 bridge module
# (fcitx5-bridge/) and the development shells, and exports the overlay and
# the Home Manager module.
#
# Two stacks come out of `mkStack`:
#   - CUDA (the default; only on x86_64-linux): onnxruntime and sherpa-onnx
#     with cudaSupport, plus the transcribe.cpp GPU engine (`cuda` cargo
#     feature) for GGUF Qwen3-ASR. This is what a NixOS desktop with an
#     NVIDIA GPU runs. It pulls the multi-GB CUDA toolchain and cannot run
#     its tests in the sandbox (no driver), so its `doCheck` is off.
#   - CPU: plain onnxruntime/sherpa-onnx, no `cuda` feature, full test
#     suite. CI builds and tests this one (`checks.default`,
#     `devShells.cpu`); on aarch64-linux both stacks are CPU.
#
# packages.{default,voiceless}, packages.voiceless-cpu and
# packages.voiceless-fcitx5-bridge are available on both architectures.
# x86_64-linux also has voiceless-cpu-avx2 and voiceless-vulkan for GGUF.
# checks.default builds and tests the CPU package. devShells.default and
# devShells.cpu exist on both architectures, devShells.vulkan on x86_64-linux.
# System-independent: overlays.default (voiceless, voiceless-fcitx5-bridge)
# and homeManagerModules.default (nix/home-manager.nix).
#
# The NVRTC patchelf step in `sherpaRuntimeLib`: nixpkgs' CUDA execution
# provider for onnxruntime calls NVRTC symbols without linking libnvrtc, so
# dlopen of libonnxruntime_providers_cuda.so fails with an undefined symbol
# (nvrtcGetProgramLogSize). symlinkJoin alone would only link the broken
# file, so the provider is copied and patched to NEED
# libnvrtc.so.12 with an rpath into cuda_nvrtc. The joined directory is
# what SHERPA_ONNX_LIB_DIR and the runtime LD_LIBRARY_PATH point at.
#
# `cudaArchitectures` limits the transcribe.cpp kernels to the listed CUDA
# compute capabilities (default "86" = RTX 30xx) because compiling every
# architecture multiplies the GPU engine build time; owners of other GPUs
# add theirs. It reaches transcribe-cpp-sys through TRANSCRIBE_CMAKE_ARGS,
# and VOICELESS_CUDA_LIB_DIRS feeds rust/build.rs the CUDA link paths.
{
  description = "Voiceless：面向 NixOS/Wayland 的原生 Rust 实时语音输入";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
    }:
    flake-utils.lib.eachSystem [ "x86_64-linux" "aarch64-linux" ] (
      system:
      let
        # allowUnfree: the CUDA toolchain and cudnn carry NVIDIA's licence.
        pkgs = import nixpkgs {
          inherit system;
          config.allowUnfree = true;
        };
        # Shared by sherpa-onnx's python3Packages override and the dev shell
        # (scripts/asr_benchmark/*.py); the Rust crate itself needs no Python.
        python = pkgs.python312;

        # The CUDA stack pulls the full CUDA/cudnn toolchain (several GB).
        # It stays the default for local NixOS machines with a GPU; the CPU
        # stack is what CI and constrained machines build and test.
        mkStack =
          cuda:
          let
            onnxruntime =
              if cuda && system == "x86_64-linux" then
                pkgs.onnxruntime.override {
                  cudaSupport = true;
                  cudaPackages = pkgs.cudaPackages_12;
                }
              else
                pkgs.onnxruntime;
            sherpaOnnx = pkgs.sherpa-onnx.override (
              {
                python3Packages = python.pkgs;
              }
              // pkgs.lib.optionalAttrs (cuda && system == "x86_64-linux") {
                cudaSupport = true;
                inherit onnxruntime;
              }
            );
            withCuda = cuda && system == "x86_64-linux";
            nvrtc = pkgs.cudaPackages_12.cuda_nvrtc;
            sherpaRuntimeLib = pkgs.symlinkJoin {
              name = "voiceless-sherpa-runtime-libs";
              paths = [
                sherpaOnnx
                onnxruntime
              ];
              # nixpkgs' CUDA provider calls NVRTC without linking it, so
              # loading it fails with "undefined symbol: nvrtcGetProgramLogSize".
              # Ship a copy that links libnvrtc itself.
              postBuild = pkgs.lib.optionalString withCuda ''
                provider=$out/lib/libonnxruntime_providers_cuda.so
                target=$(readlink -f "$provider")
                rm "$provider"
                cp "$target" "$provider"
                chmod u+w "$provider"
                ${pkgs.patchelf}/bin/patchelf \
                  --add-needed libnvrtc.so.12 \
                  --add-rpath ${pkgs.lib.getLib nvrtc}/lib \
                  "$provider"
              '';
            };
          in
          {
            inherit onnxruntime sherpaOnnx sherpaRuntimeLib;
            gpu = withCuda;
          };
        stack = mkStack true;
        stackCpu = mkStack false;

        # transcribe.cpp GPU engine (GGUF Qwen3-ASR). Kernels are built for
        # these CUDA architectures only, to keep the build short: 86 = RTX
        # 30xx. Add e.g. "89" (RTX 40xx) or "120" (RTX 50xx) as needed.
        cudaArchitectures = "86";
        cuda = pkgs.cudaPackages_12;
        cudaLibs = with cuda; [
          cccl
          cuda_cudart
          libcublas
        ];
        cudaCmakeArgs = [ "-DCMAKE_CUDA_ARCHITECTURES=${cudaArchitectures}" ];
        cudaLinkEnv = {
          # build.rs: transcribe-cpp-sys omits these from its link line.
          VOICELESS_CUDA_LIB_DIRS = pkgs.lib.concatStringsSep ":" [
            "${pkgs.lib.getLib cuda.cuda_cudart}/lib"
            "${pkgs.lib.getLib cuda.cuda_cudart}/lib/stubs"
            "${pkgs.lib.getLib cuda.libcublas}/lib"
          ];
        };
        vulkanBuildInputs = with pkgs; [
          vulkan-headers
          vulkan-loader
          spirv-headers
        ];
        vulkanShaderCompiler = pkgs.lib.getBin pkgs.shaderc;
        # transcribe-cpp-sys clears CMAKE_PREFIX_PATH. Pass the SDK outputs
        # directly to FindVulkan and the SPIR-V shader generator.
        vulkanCmakeArgs = [
          "-DVulkan_INCLUDE_DIR=${pkgs.lib.getDev pkgs.vulkan-headers}/include"
          "-DVulkan_LIBRARY=${pkgs.lib.getLib pkgs.vulkan-loader}/lib/libvulkan.so"
          "-DVulkan_GLSLC_EXECUTABLE=${vulkanShaderCompiler}/bin/glslc"
          "-DSPIRV-Headers_DIR=${pkgs.lib.getDev pkgs.spirv-headers}/share/cmake/SPIRV-Headers"
        ];

        consoleAssets = pkgs.buildNpmPackage {
          pname = "voiceless-console";
          version = (builtins.fromJSON (builtins.readFile ./frontend/package.json)).version;
          nodejs = pkgs.nodejs_22;
          src = pkgs.lib.fileset.toSource {
            root = ./frontend;
            fileset = pkgs.lib.fileset.unions [
              ./frontend/index.html
              ./frontend/package.json
              ./frontend/package-lock.json
              ./frontend/scripts
              ./frontend/src
              ./frontend/tsconfig.json
              ./frontend/vite.config.ts
            ];
          };
          npmDeps = pkgs.importNpmLock { npmRoot = ./frontend; };
          npmConfigHook = pkgs.importNpmLock.npmConfigHook;
          npmRebuildFlags = [ "--ignore-scripts" ];
          installPhase = ''
            runHook preInstall
            mkdir -p "$out"
            cp dist/index.html dist/app.js dist/style.css "$out/"
            runHook postInstall
          '';
        };

        mkVoiceless =
          {
            stack,
            avx2 ? false,
            vulkan ? false,
          }:
          let
            gguf = stack.gpu || avx2 || vulkan;
            buildRustPackage =
              if stack.gpu then
                pkgs.rustPlatform.buildRustPackage.override { stdenv = cuda.backendStdenv; }
              else
                pkgs.rustPlatform.buildRustPackage;
          in
          buildRustPackage (
            {
              pname = "voiceless";
              version = (builtins.fromTOML (builtins.readFile ./rust/Cargo.toml)).package.version;
              # Rust and browser tests replay the same PCM and PipeWire fixtures.
              src = pkgs.lib.fileset.toSource {
                root = ./.;
                fileset = pkgs.lib.fileset.unions [
                  ./rust
                  ./frontend/e2e/fixtures
                ];
              };
              cargoRoot = "rust";
              buildAndTestSubdir = "rust";
              cargoLock.lockFile = ./rust/Cargo.lock;
              VOICELESS_CONSOLE_DIST = "${consoleAssets}";
              buildFeatures =
                if stack.gpu then
                  [ "cuda" ]
                else if vulkan then
                  [ "vulkan" ]
                else
                  pkgs.lib.optional gguf "transcribe";
              # cmake is for transcribe-cpp-sys's build script, not this crate.
              dontUseCmakeConfigure = true;
              nativeBuildInputs = [
                pkgs.makeWrapper
                pkgs.pkg-config
              ]
              ++ pkgs.lib.optional gguf pkgs.cmake
              ++ pkgs.lib.optional vulkan vulkanShaderCompiler
              ++ pkgs.lib.optionals stack.gpu [
                cuda.cuda_nvcc
                pkgs.autoAddDriverRunpath
              ];
              buildInputs = [
                stack.sherpaOnnx
                stack.onnxruntime
              ]
              ++ pkgs.lib.optionals stack.gpu cudaLibs
              ++ pkgs.lib.optionals vulkan vulkanBuildInputs;
              # CUDA needs the NVIDIA driver. Vulkan's model-free checks
              # run in the sandbox with the same CPU Sherpa-ONNX stack.
              doCheck = !stack.gpu;
              # The tests spawn pw-record, wtype and wl-copy as subprocesses
              # (doctor and injection paths); they must resolve on PATH.
              nativeCheckInputs = [
                pkgs.dbus
                pkgs.pipewire
                pkgs.procps
                pkgs.wtype
                pkgs.wl-clipboard
              ];
              # Read by the sherpa-onnx-sys build script to find the shared
              # libraries instead of building its own copy.
              SHERPA_ONNX_LIB_DIR = stack.sherpaRuntimeLib + "/lib";
              # The binary execs the desktop tools and dlopens
              # the sherpa/onnxruntime libraries; the GPU provider also needs
              # the host driver from /run/opengl-driver/lib (suffix, so a
              # user-set LD_LIBRARY_PATH wins over it).
              postInstall = ''
                wrapProgram "$out/bin/voiceless" \
                  --prefix PATH : ${
                    pkgs.lib.makeBinPath [
                      pkgs.pipewire
                      pkgs.wayland-utils
                      pkgs.wtype
                      pkgs.wl-clipboard
                    ]
                  } \
                  --prefix LD_LIBRARY_PATH : ${stack.sherpaRuntimeLib}/lib \
                  --suffix LD_LIBRARY_PATH : /run/opengl-driver/lib
              '';
              meta = {
                description = "Fast realtime voice input for Linux";
                homepage = "https://github.com/Vitus213/voiceless";
                license = pkgs.lib.licenses.mit;
                mainProgram = "voiceless";
                platforms = pkgs.lib.platforms.linux;
              };
            }
            // pkgs.lib.optionalAttrs stack.gpu (
              cudaLinkEnv // { TRANSCRIBE_CMAKE_ARGS = pkgs.lib.concatStringsSep " " cudaCmakeArgs; }
            )
            // pkgs.lib.optionalAttrs avx2 {
              TRANSCRIBE_CMAKE_ARGS = pkgs.lib.concatStringsSep " " [
                "-DTRANSCRIBE_X86_CONSERVATIVE=ON"
                "-DGGML_SSE42=ON"
                "-DGGML_AVX=ON"
                "-DGGML_AVX2=ON"
                "-DGGML_F16C=ON"
                "-DGGML_FMA=ON"
                "-DGGML_BMI2=ON"
              ];
            }
            // pkgs.lib.optionalAttrs vulkan {
              TRANSCRIBE_CMAKE_ARGS = pkgs.lib.concatStringsSep " " vulkanCmakeArgs;
            }
          );
        voiceless = mkVoiceless { inherit stack; };
        voicelessCpu = mkVoiceless { stack = stackCpu; };
        fcitx5Bridge = pkgs.callPackage ./nix/fcitx5-bridge.nix { };

        # The desktop host's system libraries: WebKitGTK, GTK and the tray
        # indicator. They stay out of the core package, which needs none.
        desktopLibs = with pkgs; [
          webkitgtk_4_1
          gtk3
          libsoup_3
          libayatana-appindicator
        ];
        voicelessDesktop = pkgs.rustPlatform.buildRustPackage {
          pname = "voiceless-desktop";
          version = (builtins.fromTOML (builtins.readFile ./desktop/Cargo.toml)).package.version;
          # The host links the core crate as a library from ../rust: the source
          # is the repository and the crate is built from desktop/.
          src = ./.;
          cargoRoot = "desktop";
          buildAndTestSubdir = "desktop";
          cargoLock.lockFile = ./desktop/Cargo.lock;
          VOICELESS_CONSOLE_DIST = "${consoleAssets}";
          SHERPA_ONNX_LIB_DIR = "${stackCpu.sherpaRuntimeLib}/lib";
          nativeBuildInputs = [
            pkgs.pkg-config
            pkgs.makeWrapper
            pkgs.wrapGAppsHook3
          ];
          buildInputs = desktopLibs ++ [
            stackCpu.sherpaOnnx
            stackCpu.onnxruntime
          ];
          # The host's decisions are tested with the core: `cargo test --test desktop_contract`.
          doCheck = false;
          postInstall = ''
            # The host keeps its file name: GTK reports it as the program name,
            # by which the launcher finds its window. So the executable lives
            # under libexec, and bin holds its environment wrapper.
            mkdir -p "$out/libexec/voiceless-desktop"
            mv "$out/bin/voiceless-desktop" "$out/libexec/voiceless-desktop/voiceless-desktop"
            makeWrapper "$out/libexec/voiceless-desktop/voiceless-desktop" "$out/bin/voiceless-desktop" \
              --inherit-argv0 \
              --prefix LD_LIBRARY_PATH : ${stackCpu.sherpaRuntimeLib}/lib \
              --suffix LD_LIBRARY_PATH : /run/opengl-driver/lib
            # The launcher and the icons: the files the Debian package installs too.
            install -Dm644 ${./desktop/packaging/voiceless-desktop.desktop} \
              "$out/share/applications/voiceless-desktop.desktop"
            install -Dm644 ${./desktop/icons/32x32.png} \
              "$out/share/icons/hicolor/32x32/apps/voiceless-desktop.png"
            install -Dm644 ${./desktop/icons/128x128.png} \
              "$out/share/icons/hicolor/128x128/apps/voiceless-desktop.png"
          '';
          meta = {
            description = "Voiceless desktop window and tray over the running daemon";
            license = pkgs.lib.licenses.mit;
            platforms = pkgs.lib.platforms.linux;
            mainProgram = "voiceless-desktop";
          };
        };

        mkShell =
          {
            stack,
            vulkan ? false,
            extra ? [ ],
            env ? { },
          }:
          (if stack.gpu then pkgs.mkShell.override { stdenv = cuda.backendStdenv; } else pkgs.mkShell) (
            {
              nativeBuildInputs =
                pkgs.lib.optional (stack.gpu || vulkan) pkgs.cmake
                ++ pkgs.lib.optional stack.gpu cuda.cuda_nvcc
                ++ pkgs.lib.optional vulkan vulkanShaderCompiler;
              buildInputs = pkgs.lib.optionals stack.gpu cudaLibs ++ pkgs.lib.optionals vulkan vulkanBuildInputs;
              packages = [
                python
                pkgs.just
                pkgs.nixfmt
                pkgs.cargo
                pkgs.nodejs_22
                pkgs.rustc
                # Same toolchain as rustc: a clippy from the user profile
                # rejects crates built here (E0514).
                pkgs.clippy
                pkgs.rustfmt
                pkgs.pipewire
                pkgs.procps
                pkgs.wayland-utils
                stack.sherpaOnnx
                stack.onnxruntime
                pkgs.wl-clipboard
                pkgs.wtype
                pkgs.dbus
                pkgs.systemd
              ]
              ++ extra;

              # Used by scripts/fcitx5-bridge-test.sh.
              VOICELESS_FCITX5 = pkgs.fcitx5;
              VOICELESS_FCITX5_BRIDGE = fcitx5Bridge;
              VOICELESS_CONSOLE_DIST = "${consoleAssets}";
              # Cargo's links override uses the packaged libraries directly. The
              # upstream shared build script copies their read-only store modes
              # into target/debug and then fails when it needs to overwrite them.
              VOICELESS_CARGO_CONFIG = pkgs.writeText "voiceless-sherpa.toml" ''
                [target.${pkgs.stdenv.hostPlatform.rust.rustcTarget}.sherpa-onnx]
                rustc-link-search = ["native=${stack.sherpaRuntimeLib}/lib"]
                rustc-link-lib = ["dylib=sherpa-onnx-c-api", "dylib=onnxruntime"]
              '';
              SHERPA_ONNX_LIB_DIR = "${stack.sherpaRuntimeLib}/lib";
              LD_LIBRARY_PATH = "${stack.sherpaRuntimeLib}/lib:/run/opengl-driver/lib";
            }
            // pkgs.lib.optionalAttrs stack.gpu cudaLinkEnv
            // pkgs.lib.optionalAttrs (stack.gpu || vulkan) {
              TRANSCRIBE_CMAKE_ARGS = pkgs.lib.concatStringsSep " " (
                pkgs.lib.optionals stack.gpu cudaCmakeArgs ++ pkgs.lib.optionals vulkan vulkanCmakeArgs
              );
            }
            // env
          );
      in
      {
        packages = {
          default = voiceless;
          inherit voiceless;
          voiceless-cpu = voicelessCpu;
          voiceless-fcitx5-bridge = fcitx5Bridge;
          voiceless-desktop = voicelessDesktop;
        }
        // pkgs.lib.optionalAttrs (system == "x86_64-linux") {
          voiceless-cpu-avx2 = mkVoiceless {
            stack = stackCpu;
            avx2 = true;
          };
          voiceless-vulkan = mkVoiceless {
            stack = stackCpu;
            vulkan = true;
          };
        };

        apps.default = flake-utils.lib.mkApp { drv = voiceless; };

        # CI-safe check: builds and tests the CPU stack only.
        checks.default = voicelessCpu;

        devShells = {
          # `just lint` enables every Cargo feature, including Vulkan.
          default = mkShell {
            inherit stack;
            vulkan = stack.gpu;
          };
          cpu = mkShell { stack = stackCpu; };
          # `voiceless-desktop` (Tauri 2): the same toolchain plus WebKitGTK and the tray indicator.
          desktop = mkShell {
            stack = stackCpu;
            extra = desktopLibs ++ [
              pkgs.pkg-config
              pkgs.wrapGAppsHook3
            ];
            env = {
              LD_LIBRARY_PATH = "${stackCpu.sherpaRuntimeLib}/lib:${pkgs.lib.makeLibraryPath desktopLibs}:/run/opengl-driver/lib";
            };
          };
        }
        // pkgs.lib.optionalAttrs (system == "x86_64-linux") {
          vulkan = mkShell {
            stack = stackCpu;
            vulkan = true;
          };
        };

        # `nix fmt`: formats only the two Nix files (fcitx5-bridge.nix is
        # not listed; Rust is formatted by `just lint`).
        formatter = pkgs.writeShellApplication {
          name = "voiceless-format";
          runtimeInputs = [ pkgs.nixfmt ];
          text = ''
            nixfmt flake.nix nix/home-manager.nix
          '';
        };
      }
    )
    // {
      overlays.default = final: prev: {
        voiceless = self.packages.${prev.system}.voiceless;
        # Built against the consumer's nixpkgs so the module matches the
        # running Fcitx5's C++ ABI.
        voiceless-fcitx5-bridge = final.callPackage ./nix/fcitx5-bridge.nix { };
      };

      homeManagerModules.default = import ./nix/home-manager.nix self;
    };
}
