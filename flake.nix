{
  description = "roro — Roll On Roll Off";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
      cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = import nixpkgs { inherit system; };
        in
        rec {
          roro = pkgs.pkgsStatic.rustPlatform.buildRustPackage {
            pname = "roro";
            version = cargoToml.package.version;
            src = self;
            cargoLock.lockFile = ./Cargo.lock;
            nativeBuildInputs = [ pkgs.binutils ];
            postInstall = ''
              bash ${./scripts/check-static} "$out/bin/roro"
            '';
          };
          release-tools = pkgs.buildEnv {
            name = "roro-release-tools";
            paths = with pkgs; [
              gitleaks
              python3
            ];
          };
          default = roro;
          static = roro;
        }
      );
    };
}
