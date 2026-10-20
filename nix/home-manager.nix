self:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.programs.voiceless;
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

    xdg.configFile."voiceless/config.toml" = lib.mkIf (cfg.settings != { }) {
      source = toml.generate "voiceless-config.toml" cfg.settings;
    };

    xdg.configFile."eww-voiceless" = lib.mkIf cfg.overlay.enable {
      source = "${self}/contrib/eww-overlay";
    };

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

    systemd.user.services.voiceless-overlay = lib.mkIf cfg.overlay.enable {
      Unit = {
        Description = "Voiceless overlay event watcher";
        After = [
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

    wayland.windowManager.sway.extraConfig = lib.mkIf cfg.shortcuts.sway.enable ''
      bindsym ${cfg.shortcuts.sway.toggleKey} exec ${lib.getExe cfg.package} ctl toggle
    '';
  };
}
