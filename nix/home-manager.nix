# Home Manager module `programs.voiceless`, exported by the flake as
# homeManagerModules.default. `self` is the flake itself, so the module can
# default `package` to its own build and ship contrib/eww-overlay.
#
# With `enable = true` the module:
#   - installs `package` (default: the flake's CUDA build for the host);
#   - generates ~/.config/voiceless/config.toml from `settings` when the
#     attrset is non-empty (pkgs.formats.toml, so the file is a symlink into
#     the Nix store); an empty `settings` leaves a hand-written file alone;
#   - with `service.enable`: the `voiceless.service` user unit running
#     `voiceless daemon --mode <service.mode>`;
#   - with `overlay.enable`: links contrib/eww-overlay to
#     ~/.config/eww-voiceless and adds `voiceless-overlay-eww.service` (the
#     eww daemon) and `voiceless-overlay.service` (the Python watcher that
#     follows `voiceless ctl watch` and drives the pill);
#   - with `shortcuts.sway.enable`: a Sway `bindsym` running
#     `voiceless ctl toggle` (one D-Bus call to the daemon).
#
# Unit ordering: all three units are PartOf/WantedBy graphical-session.target
# and start After it; voiceless-overlay additionally starts After
# voiceless.service and voiceless-overlay-eww.service. These are ordering
# constraints only (no Requires/Wants), so enabling the overlay without the
# daemon service leaves the watcher running against an absent daemon.
#
# Secrets: `AppConfig::load` (rust/src/config.rs) rejects a config file
# containing `api_key` or `access_key` unless its mode is 0600 or stricter.
# A store-generated file is world-readable, so keys written through
# `settings` fail that gate; keep them in a hand-managed 0600 file.
#
# The option `description` strings are the user-facing (Chinese) text shown
# by Home Manager's option docs; README.md / README.zh.md repeat them.
self:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.programs.voiceless;
  # Generates config.toml; `toml.type` also types the `settings` option so
  # Home Manager rejects values TOML cannot represent.
  toml = pkgs.formats.toml { };
in
{
  options.programs.voiceless = {
    enable = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "是否启用 Voiceless 语音输入。";
    };

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.system}.voiceless;
      defaultText = lib.literalExpression "voiceless.packages.${pkgs.system}.voiceless";
      description = "要安装的 Voiceless 软件包。";
    };

    settings = lib.mkOption {
      type = toml.type;
      default = { };
      description = "写入 voiceless/config.toml 的配置。";
    };

    service.enable = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "是否启动常驻的 Voiceless headless daemon 用户服务。";
    };

    service.mode = lib.mkOption {
      type = lib.types.str;
      default = "quick";
      description = "daemon 会话使用的文本处理模式（例如 quick、prompt-optimize）。";
    };

    overlay.enable = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = ''
        是否启用 eww 浮层：识别时在屏幕底部显示实时文字。会安装
        ~/.config/eww-voiceless，并启动独立的 eww daemon 与事件订阅服务
        （需要 service.enable）。
      '';
    };

    shortcuts.sway = {
      enable = lib.mkOption {
        type = lib.types.bool;
        default = false;
        description = ''
          是否添加 Sway 全局快捷键。GlobalShortcuts 门户后端仍须由用户在
          NixOS 层配置；此 Home Manager 模块不会代为配置门户。
        '';
      };

      toggleKey = lib.mkOption {
        type = lib.types.str;
        default = "$mod+Shift+d";
        description = "Sway 开始/停止识别快捷键。";
      };
    };
  };

  config = lib.mkIf cfg.enable {
    home.packages = [ cfg.package ];

    # ~/.config/voiceless/config.toml, only when `settings` has content; the
    # binary treats unknown keys as errors, so typos surface at first run.
    xdg.configFile."voiceless/config.toml" = lib.mkIf (cfg.settings != { }) {
      source = toml.generate "voiceless-config.toml" cfg.settings;
    };

    # The whole overlay directory (eww.yuck, eww.scss, the watcher, the
    # listener and voiceless-overlay.conf) as ~/.config/eww-voiceless; the
    # units below reference the files by that path.
    xdg.configFile."eww-voiceless" = lib.mkIf cfg.overlay.enable {
      source = "${self}/contrib/eww-overlay";
    };

    # eww daemon dedicated to the overlay (its own --config dir, so a user's
    # main eww instance is untouched). The watcher opens/closes the window
    # and feeds it through the FIFO that eww.yuck's deflisten reads.
    systemd.user.services.voiceless-overlay-eww = lib.mkIf cfg.overlay.enable {
      Unit = {
        Description = "eww daemon for the Voiceless overlay";
        After = [ "graphical-session.target" ];
        PartOf = [ "graphical-session.target" ];
      };
      Service = {
        # Foreground so Restart=on-failure can track it.
        ExecStart = "${lib.getExe pkgs.eww} --config %h/.config/eww-voiceless daemon --no-daemonize";
        Restart = "on-failure";
        RestartSec = 3;
      };
      Install.WantedBy = [ "graphical-session.target" ];
    };

    # Python watcher (contrib/eww-overlay/voiceless-overlay.py): runs
    # `voiceless ctl watch`, translates the daemon's JSON events into pill
    # frames and `eww open/close`. VOICELESS_BIN / EWW_BIN / EWW_CONFIG tell
    # it which binaries and config dir to use; it restarts `ctl watch`
    # itself if the bus drops, systemd restarts it if the script dies.
    systemd.user.services.voiceless-overlay = lib.mkIf cfg.overlay.enable {
      Unit = {
        Description = "Voiceless overlay event watcher";
        # graphical-session.target must be listed: a target adds After= on
        # every unit it Wants unless that unit orders itself After the
        # target, which would close a cycle through voiceless.service and
        # make systemd drop this job at login.
        After = [
          "graphical-session.target"
          "voiceless.service"
          "voiceless-overlay-eww.service"
        ];
        PartOf = [ "graphical-session.target" ];
      };
      Service = {
        ExecStart = "${lib.getExe pkgs.python3} %h/.config/eww-voiceless/voiceless-overlay.py";
        Environment = [
          "VOICELESS_BIN=${lib.getExe cfg.package}"
          "EWW_BIN=${lib.getExe pkgs.eww}"
          "EWW_CONFIG=%h/.config/eww-voiceless"
        ];
        Restart = "on-failure";
        RestartSec = 3;
      };
      Install.WantedBy = [ "graphical-session.target" ];
    };

    # The resident daemon: keeps the engine warm, owns dev.voiceless.Daemon
    # on the session bus, listens for the push-to-talk key and serves the
    # loopback web console. Tied to graphical-session.target because it
    # needs the Wayland session for injection. TimeoutStopSec covers a
    # session that is still finishing and injecting when stop is requested.
    systemd.user.services.voiceless = lib.mkIf cfg.service.enable {
      Unit = {
        Description = "Voiceless headless 语音输入 daemon";
        After = [ "graphical-session.target" ];
        PartOf = [ "graphical-session.target" ];
      };

      Service = {
        ExecStart = "${lib.getExe cfg.package} daemon --mode ${cfg.service.mode}";
        Restart = "on-failure";
        RestartSec = 2;
        TimeoutStopSec = 20;
      };

      Install.WantedBy = [ "graphical-session.target" ];
    };

    # Sway only: `ctl toggle` starts or stops a daemon session. The portal
    # GlobalShortcuts backend is a NixOS-level concern and is not touched.
    wayland.windowManager.sway.extraConfig = lib.mkIf cfg.shortcuts.sway.enable ''
      bindsym ${cfg.shortcuts.sway.toggleKey} exec ${lib.getExe cfg.package} ctl toggle
    '';
  };
}
