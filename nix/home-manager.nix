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
      bindsym ${cfg.shortcuts.sway.toggleKey} exec ${pkgs.systemd}/bin/busctl --user call dev.voiceless.Daemon /dev/voiceless/Daemon dev.voiceless.Daemon.Controller Activate
    '';
  };
}
