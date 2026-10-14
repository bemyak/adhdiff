{
  lib,
  craneLib,
  rustPlatform,
  fetchCrate,
  fetchurl,
  runCommand,
  buildWasmBindgenCli,
  pkg-config,
  trunk,
  binaryen,
  llvmPackages,
  webkitgtk_4_1,
  libsoup_3,
  gtk3,
  glib,
  cairo,
  pango,
  gdk-pixbuf,
  at-spi2-core,
}:

let
  # Guard: the bundle URL below hardcodes the tree-sitter-language-pack
  # release; fail eval loudly when Cargo.lock moves to another version.
  cargoLockParsed = builtins.fromTOML (builtins.readFile ../Cargo.lock);
  cargoToml = builtins.fromTOML (builtins.readFile ../Cargo.toml);
  tslpVersion = lib.findFirst (p: p.name == "tree-sitter-language-pack") null cargoLockParsed.package;
in
assert lib.assertMsg (tslpVersion != null && tslpVersion.version == "1.22.1")
  "nix/package.nix: parser-sources URL pins 1.22.1 but Cargo.lock has ${tslpVersion.version or "missing"} — update both";
let
  # The grammar sources are compiled in statically, but the crate's build
  # script downloads them from GitHub releases, which the sandbox forbids.
  # Feed it a prefetched bundle through the crate's file:// override; the
  # build insists on a .sha256 sidecar next to the URL, so lay both out in
  # one directory.
  tslpParserSources = fetchurl {
    url = "https://github.com/xberg-io/tree-sitter-language-pack/releases/download/v1.22.1/parser-sources-1.22.1.tar.zst";
    hash = "sha256-Ln++rEbKXPtoXFPNipK7YGonJLEk0oxax9BO+akT11Y=";
  };
  tslpBundleDir = runCommand "tslp-parser-sources-1.22.1" { } ''
    mkdir $out
    ln -s ${tslpParserSources} $out/parser-sources-1.22.1.tar.zst
    cd $out
    sha256sum parser-sources-1.22.1.tar.zst | cut -d' ' -f1 | tr -d '\n' > parser-sources-1.22.1.tar.zst.sha256
  '';

  # Cargo.lock pins wasm-bindgen 0.2.129 and trunk requires the
  # wasm-bindgen CLI to match the lockfile version exactly, while pinned
  # nixpkgs only ships up to 0.2.127 — build the exact CLI from crates.io.
  wasm-bindgen-cli = buildWasmBindgenCli rec {
    src = fetchCrate {
      pname = "wasm-bindgen-cli";
      version = "0.2.129";
      hash = "sha256-pcecKQd7E8Opw6bkFoE569epUi7gh5qpQF1e5PJY6V8=";
    };

    cargoDeps = rustPlatform.fetchCargoVendor {
      inherit src;
      inherit (src) pname version;
      hash = "sha256-vmUrWVU7kPJJxO5qIVeAkwQyWDELO1Z4Z5gitz2kco8=";
    };
  };

  root = ../.;
  src = lib.fileset.toSource {
    inherit root;
    fileset = lib.fileset.unions [
      (craneLib.fileset.commonCargoSources root)
      (root + "/.cargo/config.toml")
      (root + "/build.rs")
      (root + "/tauri.conf.json")
      (root + "/icons")
      (root + "/skills")
      (root + "/src/prompt_template.md")
      (root + "/frontend/index.html")
      (root + "/frontend/styles.css")
      (root + "/frontend/src/help.txt")
    ];
  };

  commonArgs = {
    pname = "adhdiff";
    inherit (cargoToml.package) version;
    inherit src;
    strictDeps = true;

    # frontend/ is wasm-only (built by trunk); the host build covers adhdiff
    # and its path dependency adhdiff-shared.
    cargoExtraArgs = "--locked -p adhdiff";

    nativeBuildInputs = [
      pkg-config
      trunk
      binaryen
      llvmPackages.lld
      wasm-bindgen-cli
    ];

    buildInputs = [
      webkitgtk_4_1
      libsoup_3
      gtk3
      glib
      cairo
      pango
      gdk-pixbuf
      at-spi2-core
    ];

    # rustc 1.99+ emits bulk-memory ops that trunk's pinned wasm-opt v123
    # rejects; point trunk at nixpkgs' binaryen instead (see devShell).
    env.TRUNK_TOOLS_WASM_OPT = "version_${binaryen.version}";
    # tree-sitter-language-pack is a dependency, so its build script runs in
    # the deps stage too; both stages need the same env to share artifacts.
    env.TSLP_LINK_MODE = "static";
    env.TSLP_LANGUAGES = "rust,go,python,bash,nix,toml,javascript,typescript,markdown,markdown_inline,hcl,json,yaml,html,css,proto";
    env.TSLP_SOURCE_BUNDLE_URL = "file://${tslpBundleDir}/parser-sources-1.22.1.tar.zst";

    # Tests run in the devShell and CI; the Nix build only ships the binary.
    doCheck = false;
  };

  # crane replaces build.rs and the sources with stubs here, so the deps
  # build needs neither frontend/dist nor tauri.conf.json.
  cargoArtifacts = craneLib.buildDepsOnly commonArgs;
in
craneLib.buildPackage (
  commonArgs
  // {
    inherit cargoArtifacts;

    # tauri-codegen embeds frontend/dist at compile time, so the Leptos
    # frontend must be built before the host crate. The CLI's
    # beforeBuildCommand is not run by plain cargo, so run trunk here.
    preBuild = ''
      pushd frontend
      trunk build --offline --frozen --release
      popd
    '';

    installPhaseCommand = ''
      install -Dm755 target/release/adhdiff $out/bin/adhdiff

      mkdir -p $out/share/adhdiff/skills
      cp -r skills/adhdiff $out/share/adhdiff/skills/
    '';

    meta = {
      description = "Agent-annotated diff review CLI with desktop and terminal UIs";
      license = lib.licenses.gpl3Plus;
      mainProgram = "adhdiff";
    };
  }
)
