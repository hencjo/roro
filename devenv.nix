{ pkgs, ... }:
{
  packages = [
    pkgs.git
    pkgs.python3
    pkgs.rootlesskit
    pkgs.docker
    pkgs.gh
    pkgs.gitleaks
    pkgs.curl
    pkgs.jq
    pkgs.skopeo
    pkgs.cargo-release
    pkgs.shellcheck
    pkgs.file
    pkgs.binutils
  ];
  languages.rust = {
    enable = true;
    channel = "stable";
    components = [
      "rustc"
      "cargo"
      "clippy"
      "rustfmt"
      "rust-analyzer"
    ];
    targets = [ "x86_64-unknown-linux-musl" ];
  };
  git-hooks.hooks = {
    rustfmt.enable = true;
    nixfmt.enable = true;
  };
}
