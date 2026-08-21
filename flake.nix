{
  description = "Starlight PC";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
    flake-utils.url = "github:numtide/flake-utils";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      nixpkgs,
      flake-utils,
      fenix,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs { inherit system; };
        toolchain = fenix.packages.${system}.stable.withComponents [
          "cargo"
          "rustc"
          "rustfmt"
          "clippy"
          "rust-src"
        ];

        linuxLibs = with pkgs; [
          fontconfig
          freetype
          vulkan-loader
          wayland
          libxkbcommon
          libGL
          xorg.libX11
          xorg.libXcursor
          xorg.libXi
          xorg.libxcb
        ];
        rustPlatform = pkgs.makeRustPlatform {
          cargo = toolchain;
          rustc = toolchain;
        };

        starlight = rustPlatform.buildRustPackage {
          pname = "starlight";
          version = (fromTOML (builtins.readFile ./Cargo.toml)).package.version;

          src = ./.;

          cargoLock = {
            lockFile = ./Cargo.lock;
            outputHashes = {
              "collections-0.1.0" = "sha256-BuY0YCmTlkGqrDLjOTatzWa/VoOo4kQtPxL7QkwjWTA=";
              "gpui-component-0.5.2" = "sha256-BOUSwFpSECRvsNi+F5psCO4mvgyF7SAbJ01r6RyH3A8=";
              "zed-font-kit-0.14.1-zed" = "sha256-KXygi0olNQi5yM8eaJVykNDtbPMDjT+cWPBF8UrtXR4=";
              "zed-reqwest-0.12.15-zed" = "sha256-p4SiUrOrbTlk/3bBrzN/mq/t+1Gzy2ot4nso6w6S+F8=";
              "zed-scap-0.0.8-zed" = "sha256-BihiQHlal/eRsktyf0GI3aSWsUCW7WcICMsC2Xvb7kw=";
              "zed-xim-0.4.0-zed" = "sha256-pRT4Sz1JU9ros47/7pmIW9kosWOGMOItcnNd+VrvnpE=";
              "wasm_thread-0.3.3" = "sha256-+lRLCIk0S6Y5ORYjDKsYYHia2FtoSoh+rWkQh7mnPBE=";
            };
          };

          nativeBuildInputs =
            [ pkgs.makeWrapper ]
            ++ pkgs.lib.optionals pkgs.stdenv.isLinux [ pkgs.pkg-config ];

          buildInputs = pkgs.lib.optionals pkgs.stdenv.isLinux linuxLibs;

          preBuild = ''
            ln -sfn gpui-component-assets-0.5.1 "$NIX_BUILD_TOP/cargo-vendor-dir/assets"
          '';

          doCheck = false;

          postFixup = pkgs.lib.optionalString pkgs.stdenv.isLinux ''
            wrapProgram $out/bin/Starlight \
              --prefix LD_LIBRARY_PATH : ${pkgs.lib.makeLibraryPath linuxLibs}
          '';

          meta = with pkgs.lib; {
            description = "Among Us mod manager";
            license = licenses.gpl3Only;
            platforms = platforms.unix;
            mainProgram = "Starlight";
          };
        };
      in
      {
        packages = {
          default = starlight;
          starlight = starlight;
        };

        apps.default = {
          type = "app";
          program = "${starlight}/bin/Starlight";
        };

        devShells.default = pkgs.mkShell {
          nativeBuildInputs = [ toolchain ] ++ pkgs.lib.optionals pkgs.stdenv.isLinux [ pkgs.pkg-config ];

          buildInputs = pkgs.lib.optionals pkgs.stdenv.isLinux linuxLibs;

          LD_LIBRARY_PATH = pkgs.lib.optionalString pkgs.stdenv.isLinux (pkgs.lib.makeLibraryPath linuxLibs);
        };
      }
    );
}
