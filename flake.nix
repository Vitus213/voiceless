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
        pkgs = import nixpkgs {
          inherit system;
          config.allowUnfree = true;
        };
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
          };
        stack = mkStack true;
        stackCpu = mkStack false;

        mkVoiceless =
          stack:
          pkgs.rustPlatform.buildRustPackage {
            pname = "voiceless";
            version = "0.1.0";
            src = ./rust;
            cargoLock.lockFile = ./rust/Cargo.lock;
            nativeBuildInputs = [
              pkgs.makeWrapper
              pkgs.pkg-config
            ];
            buildInputs = [
              stack.sherpaOnnx
              stack.onnxruntime
            ];
            nativeCheckInputs = [
              pkgs.pipewire
              pkgs.wtype
              pkgs.wl-clipboard
            ];
            SHERPA_ONNX_LIB_DIR = stack.sherpaRuntimeLib + "/lib";
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
          };
        voiceless = mkVoiceless stack;
        voicelessCpu = mkVoiceless stackCpu;
        fcitx5Bridge = pkgs.callPackage ./nix/fcitx5-bridge.nix { };

        mkShell =
          stack:
          pkgs.mkShell {
            packages = [
              python
              pkgs.just
              pkgs.nixfmt
              pkgs.cargo
              pkgs.rustc
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
          };
      in
      {
        packages.default = voiceless;
        packages.voiceless = voiceless;
        packages.voiceless-cpu = voicelessCpu;
        packages.voiceless-fcitx5-bridge = fcitx5Bridge;

        apps.default = flake-utils.lib.mkApp { drv = voiceless; };

        # CI-safe check: builds and tests the CPU stack only.
        checks.default = voicelessCpu;

        devShells.default = mkShell stack;
        devShells.cpu = mkShell stackCpu;

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
