{
  description = "adhdiff dev shell — native deps for building/running the Tauri app";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  inputs.crane.url = "github:ipetkov/crane";

  outputs =
    {
      self,
      nixpkgs,
      crane,
      ...
    }:
    let
      forEachSystem =
        f:
        nixpkgs.lib.genAttrs [ "x86_64-linux" "aarch64-linux" ] (
          system: f nixpkgs.legacyPackages.${system}
        );
    in
    {
      packages = forEachSystem (pkgs: rec {
        adhdiff = pkgs.callPackage ./nix/package.nix { craneLib = crane.mkLib pkgs; };
        default = adhdiff;
      });

      homeManagerModules.default = import ./nix/home-manager.nix { inherit self; };

      devShells = forEachSystem (pkgs: {
        default =
          let
            runtimeLibs = with pkgs; [
              glib
              gtk3
              libsoup_3
              webkitgtk_4_1
              cairo
              pango
              gdk-pixbuf
              at-spi2-core
            ];
            # Scope the rpath flag to the host target: a global RUSTFLAGS
            # would break trunk's wasm32-unknown-unknown link step.
            rustTarget = pkgs.stdenv.hostPlatform.rust.rustcTarget;
            targetRustflags =
              "CARGO_TARGET_${nixpkgs.lib.toUpper (builtins.replaceStrings [ "-" ] [ "_" ] rustTarget)}_RUSTFLAGS";
          in
          pkgs.mkShell (
            {
              # pkg-config lets the sys crates (gtk, soup, webkit) find the
              # libs below; hook exports PKG_CONFIG_PATH from buildInputs.
              nativeBuildInputs = [ pkgs.pkg-config ];
              buildInputs = with pkgs; [
                webkitgtk_4_1
                libsoup_3
                gtk3
              ];
              packages = [
                pkgs.trunk
                pkgs.cargo-tauri
                pkgs.binaryen
              ];
              # rustc 1.99+ emits bulk-memory ops that trunk's downloaded
              # wasm-opt v123 rejects; force trunk to use nixpkgs' binaryen
              # (find_system matches this exact version string against
              # `wasm-opt --version` on PATH).
              TRUNK_TOOLS_WASM_OPT = "version_${pkgs.binaryen.version}";
              # GTK stack is dlopened/needed at runtime by wry/tao.
              LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath runtimeLibs;
              # Bake RUNPATH into host binaries so target/{debug,release}/adhdiff
              # runs outside this shell.
              "${targetRustflags}" =
                "-C link-arg=-Wl,-rpath,${pkgs.lib.makeLibraryPath runtimeLibs}";
            }
          );
      });
    };
}
