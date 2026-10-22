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
# Per-system outputs: packages.{default,voiceless} (CUDA stack),
# packages.voiceless-cpu, packages.voiceless-fcitx5-bridge, apps.default,
# checks.default (the CPU package, whose build runs `cargo test`),
# devShells.{default,cpu} and formatter (nixfmt over the Nix files).
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
        cudaEngineEnv = {
          TRANSCRIBE_CMAKE_ARGS = "-DCMAKE_CUDA_ARCHITECTURES=${cudaArchitectures}";
          # build.rs: transcribe-cpp-sys omits these from its link line.
          VOICELESS_CUDA_LIB_DIRS = pkgs.lib.concatStringsSep ":" [
            "${pkgs.lib.getLib cuda.cuda_cudart}/lib"
            "${pkgs.lib.getLib cuda.cuda_cudart}/lib/stubs"
            "${pkgs.lib.getLib cuda.libcublas}/lib"
          ];
        };

        mkVoiceless =
          stack:
          let
            buildRustPackage =
              if stack.gpu then
                pkgs.rustPlatform.buildRustPackage.override { stdenv = cuda.backendStdenv; }
              else
                pkgs.rustPlatform.buildRustPackage;
          in
          buildRustPackage (
            {
              pname = "voiceless";
              version = "0.1.0";
              src = ./rust;
              cargoLock.lockFile = ./rust/Cargo.lock;
              buildFeatures = pkgs.lib.optional stack.gpu "cuda";
              # cmake is for transcribe-cpp-sys's build script, not this crate.
              dontUseCmakeConfigure = true;
              nativeBuildInputs = [
                pkgs.makeWrapper
                pkgs.pkg-config
              ]
              ++ pkgs.lib.optionals stack.gpu [
                pkgs.cmake
                cuda.cuda_nvcc
                pkgs.autoAddDriverRunpath
              ];
              buildInputs = [
                stack.sherpaOnnx
                stack.onnxruntime
              ]
              ++ pkgs.lib.optionals stack.gpu cudaLibs;
              # GPU test binaries need the NVIDIA driver, absent in the sandbox;
              # the CPU package (checks.default) runs the suite.
              doCheck = !stack.gpu;
              # The tests spawn pw-record, wtype and wl-copy as subprocesses
              # (doctor and injection paths); they must resolve on PATH.
              nativeCheckInputs = [
                pkgs.pipewire
                pkgs.wtype
                pkgs.wl-clipboard
              ];
              # Read by the sherpa-onnx-sys build script to find the shared
              # libraries instead of building its own copy.
              SHERPA_ONNX_LIB_DIR = stack.sherpaRuntimeLib + "/lib";
              # The binary execs the same three tools at runtime and dlopens
              # the sherpa/onnxruntime libraries; the GPU provider also needs
              # the host driver from /run/opengl-driver/lib (suffix, so a
              # user-set LD_LIBRARY_PATH wins over it).
              postInstall = ''
                wrapProgram "$out/bin/voiceless" \
                  --prefix PATH : ${
                    pkgs.lib.makeBinPath [
                      pkgs.pipewire
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
            // pkgs.lib.optionalAttrs stack.gpu cudaEngineEnv
          );
        voiceless = mkVoiceless stack;
        voicelessCpu = mkVoiceless stackCpu;
        fcitx5Bridge = pkgs.callPackage ./nix/fcitx5-bridge.nix { };

        mkShell =
          stack:
          (if stack.gpu then pkgs.mkShell.override { stdenv = cuda.backendStdenv; } else pkgs.mkShell) (
            {
              # `cargo build --features cuda` builds the GPU engine here.
              nativeBuildInputs = pkgs.lib.optionals stack.gpu [
                pkgs.cmake
                cuda.cuda_nvcc
              ];
              buildInputs = pkgs.lib.optionals stack.gpu cudaLibs;
              packages = [
                python
                pkgs.just
                pkgs.nixfmt
                pkgs.cargo
                pkgs.rustc
                # Same toolchain as rustc: a clippy from the user profile
                # rejects crates built here (E0514).
                pkgs.clippy
                pkgs.rustfmt
                pkgs.pipewire
                stack.sherpaOnnx
                stack.onnxruntime
                pkgs.wl-clipboard
                pkgs.wtype
                pkgs.dbus
                pkgs.systemd
              ];

              # Used by scripts/fcitx5-bridge-test.sh.
              VOICELESS_FCITX5 = pkgs.fcitx5;
              VOICELESS_FCITX5_BRIDGE = fcitx5Bridge;
              SHERPA_ONNX_LIB_DIR = "${stack.sherpaRuntimeLib}/lib";
              LD_LIBRARY_PATH = "${stack.sherpaRuntimeLib}/lib:/run/opengl-driver/lib";
            }
            // pkgs.lib.optionalAttrs stack.gpu cudaEngineEnv
          );
      in
      {
        packages.default = voiceless;
        packages.voiceless = voiceless;
        packages.voiceless-cpu = voicelessCpu;
        packages.voiceless-fcitx5-bridge = fcitx5Bridge;

        apps.default = flake-utils.lib.mkApp { drv = voiceless; };

        # CI-safe check: builds and tests the CPU stack only.
        checks.default = voicelessCpu;

        # `nix develop` gets the GPU toolchain (cmake, nvcc, CUDA libs) so
        # `cargo build --features cuda` works; `.#cpu` is what CI uses and
        # what a machine without a GPU or disk for CUDA should pick.
        devShells.default = mkShell stack;
        devShells.cpu = mkShell stackCpu;

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
