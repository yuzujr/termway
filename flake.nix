{
  description = "termway: control a remote Wayland desktop through an SSH terminal";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
    in
    {
      packages = forAllSystems (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.rustPlatform.buildRustPackage {
            pname = "termway";
            version = "0.1.0";
            src = nixpkgs.lib.cleanSource ./.;
            cargoLock = { lockFile = ./Cargo.lock; };
            nativeBuildInputs = [ pkgs.pkg-config pkgs.wayland-scanner ];
            buildInputs = [ pkgs.wayland ];
            meta = with nixpkgs.lib; {
              description = "Control a remote Wayland desktop through an SSH terminal";
              license = with licenses; [ mit asl20 ];
              mainProgram = "termway";
              platforms = platforms.linux;
            };
          };
        });

      devShells = forAllSystems (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.mkShell {
            packages = with pkgs; [
              cargo
              clippy
              pkg-config
              rustc
              rustfmt
              wayland
              wayland-protocols
              ydotool
            ];
          };
        });
    };
}
