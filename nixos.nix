self:
{ config, lib, pkgs, ... }:

let
  cfg = config.services.din-drive;
  inherit (lib) mkEnableOption mkOption mkIf types;
in
{
  options.services.din-drive = {
    enable = mkEnableOption "Din Drive, an encrypted drive on Nostr and Blossom mounted in each user's home";

    package = mkOption {
      type = types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      description = "The din-drive package.";
    };

    users = mkOption {
      type = types.listOf types.str;
      default = [ ];
      description = ''
        Users whose drive is mounted when they log in. A user's drive starts
        once they've created it (`din-drive init`); until then the service
        exits without retrying.
      '';
    };

    mountpoint = mkOption {
      type = types.str;
      default = "Drive";
      description = "Where the drive is mounted, relative to the user's home.";
    };
  };

  config = mkIf cfg.enable {
    environment.systemPackages = [ cfg.package ];

    systemd.user.services.din-drive = {
      description = "Din Drive";
      wantedBy = [ "default.target" ];
      unitConfig.ConditionUser = map (u: "|" + u) cfg.users;
      serviceConfig = {
        ExecStart = "${cfg.package}/bin/din-drive mount %h/${cfg.mountpoint}";
        # fusermount3 is a setuid wrapper.
        Environment = [ "PATH=/run/wrappers/bin" ];
        # Signer locked or offline: try again. No drive yet (3): wait to be
        # restarted once there is one.
        Restart = "on-failure";
        RestartSec = 15;
        RestartPreventExitStatus = 3;
        TimeoutStopSec = 20;
      };
    };
  };
}
