{ self }:
{
  config,
  lib,
  pkgs,
  options,
  ...
}:
let
  cfg = config.programs.adhdiff;
  agentIntegration = cfg.enable && cfg.enableAgentIntegration;
  skillDir = "${cfg.package}/share/adhdiff/skills/adhdiff";
in
{
  options.programs.adhdiff = {
    enable = lib.mkEnableOption "adhdiff agent-annotated diff review";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalExpression "inputs.adhdiff.packages.${pkgs.stdenv.hostPlatform.system}.default";
      description = "adhdiff package to install.";
    };

    enableAgentIntegration = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = ''
        Wire the adhdiff agent skill bundled with the package into the
        supported agents (codex, opencode, claude-code, omp) whose Home
        Manager modules are present.

        Note: the wiring uses the attribute-set form of
        {option}`programs.<agent>.skills` (`skills.adhdiff = <dir>`). If
        you instead assign {option}`programs.<agent>.skills` a plain
        directory path, evaluation fails with a type conflict — use the
        attribute-set form or disable this option.
      '';
    };
  };

  config = lib.mkMerge [
    (lib.mkIf cfg.enable { home.packages = [ cfg.package ]; })
    (lib.mkIf agentIntegration (lib.mkMerge [
      (lib.optionalAttrs (options.programs ? codex) {
        programs.codex.skills.adhdiff = skillDir;
      })
      (lib.optionalAttrs (options.programs ? opencode) {
        programs.opencode.skills.adhdiff = skillDir;
      })
      (lib.optionalAttrs (options.programs ? claude-code) {
        programs.claude-code.skills.adhdiff = skillDir;
      })
      (lib.optionalAttrs (options.programs ? omp) {
        programs.omp.settings."skills.customDirectories" = [ "${cfg.package}/share/adhdiff/skills" ];
      })
    ]))
  ];
}
