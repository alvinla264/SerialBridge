{
  description = "SerialBridge: one local core owns the serial devices; AI clients and the human terminal share them";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    {
      self,
      nixpkgs,
    }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      eachSystem = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});

      # Cargo builds are per-package, so the core and the two clients are two
      # derivations. The clients compile the core's store path in as their
      # `SERIALBRIDGE_CORE_DEFAULT`, so a client installed on its own starts the
      # exact core it was built against (see crates/serialbridge-client/src/autostart.rs).
      build =
        pkgs:
        let
          version = "0.1.0";
          common = {
            inherit version;
            src = self;
            cargoLock.lockFile = ./Cargo.lock;
            # The workspace tests drive PTY pairs and real sockets; they belong
            # to `cargo test`, not to a sandboxed Nix build.
            doCheck = false;
          };
          serial-core = pkgs.rustPlatform.buildRustPackage (
            common
            // {
              pname = "serial-core";
              buildAndTestSubdir = "crates/serial-core";
              meta = {
                description = "The SerialBridge core: owns serial devices for one local user and serves clients";
                mainProgram = "serial-core";
                license = pkgs.lib.licenses.asl20;
              };
            }
          );
          clients = pkgs.rustPlatform.buildRustPackage (
            common
            // {
              pname = "serialbridge-clients";
              cargoBuildFlags = [
                "--package"
                "serial-mcp"
                "--package"
                "serial-terminal"
              ];
              SERIALBRIDGE_CORE_DEFAULT = "${serial-core}/bin/serial-core";
              # nixpkgs' cargo hook passes an explicit `--target`, so the
              # binaries land under the target triple.
              installPhase = ''
                runHook preInstall
                mkdir -p $out/bin
                target="target/${pkgs.stdenv.hostPlatform.rust.rustcTarget}/release"
                install -m755 "$target/serial-mcp" $out/bin/serial-mcp
                install -m755 "$target/serial-terminal" $out/bin/serial-terminal
                runHook postInstall
              '';
              meta = {
                description = "SerialBridge clients: the MCP server and the human terminal";
                mainProgram = "serial-terminal";
                license = pkgs.lib.licenses.asl20;
              };
            }
          );
        in
        {
          inherit serial-core clients;
          # Same binaries, correct `meta.mainProgram`, so `nix run .#serial-mcp`
          # and `nix run .#serial-terminal` each start the right one.
          serial-terminal = clients.overrideAttrs (old: {
            meta = old.meta // {
              mainProgram = "serial-terminal";
            };
          });
          serial-mcp = clients.overrideAttrs (old: {
            meta = old.meta // {
              mainProgram = "serial-mcp";
            };
          });
          default = pkgs.symlinkJoin {
            name = "serialbridge-${version}";
            paths = [
              serial-core
              clients
            ];
            meta = {
              description = "SerialBridge: core, MCP server and human terminal";
              mainProgram = "serial-terminal";
              license = pkgs.lib.licenses.asl20;
            };
          };
        };
    in
    {
      packages = eachSystem (
        pkgs:
        let
          built = build pkgs;
        in
        built
        // {
          serialbridge = built.default;
        }
      );

      # `nix flake check` builds these on the current system; the other
      # architecture is only evaluated (`--all-systems --no-build`) unless a
      # native builder or emulator is configured.
      checks = eachSystem (pkgs: {
        default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
        serial-mcp = self.packages.${pkgs.stdenv.hostPlatform.system}.serial-mcp;
      });

      devShells = eachSystem (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            rustc
            rustfmt
            clippy
          ];
        };
      });

      formatter = eachSystem (pkgs: pkgs.nixfmt);
    };
}
