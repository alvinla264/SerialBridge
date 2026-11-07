{
  description = "SerialNexus: one local core owns the serial devices; AI clients and the human terminal share them";

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
      # `SERIAL_NEXUS_CORE_DEFAULT`, so a client installed on its own starts the
      # exact core it was built against (see crates/serial-nexus-client/src/autostart.rs).
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
          serial-nexus-core = pkgs.rustPlatform.buildRustPackage (
            common
            // {
              pname = "serial-nexus-core";
              buildAndTestSubdir = "crates/serial-nexus-core";
              meta = {
                description = "The SerialNexus core: owns serial devices for one local user and serves clients";
                mainProgram = "serial-nexus-core";
                license = pkgs.lib.licenses.asl20;
              };
            }
          );
          clients = pkgs.rustPlatform.buildRustPackage (
            common
            // {
              pname = "serial-nexus-clients";
              cargoBuildFlags = [
                "--package"
                "serial-nexus-mcp"
                "--package"
                "serial-nexus-terminal"
              ];
              SERIAL_NEXUS_CORE_DEFAULT = "${serial-nexus-core}/bin/serial-nexus-core";
              # nixpkgs' cargo hook passes an explicit `--target`, so the
              # binaries land under the target triple.
              installPhase = ''
                runHook preInstall
                mkdir -p $out/bin
                target="target/${pkgs.stdenv.hostPlatform.rust.rustcTarget}/release"
                install -m755 "$target/serial-nexus-mcp" $out/bin/serial-nexus-mcp
                install -m755 "$target/serial-nexus-terminal" $out/bin/serial-nexus-terminal
                runHook postInstall
              '';
              meta = {
                description = "SerialNexus clients: the MCP server and the human terminal";
                mainProgram = "serial-nexus-terminal";
                license = pkgs.lib.licenses.asl20;
              };
            }
          );
        in
        {
          inherit serial-nexus-core clients;
          # Same binaries, correct `meta.mainProgram`, so `nix run .#serial-nexus-mcp`
          # and `nix run .#serial-nexus-terminal` each start the right one.
          serial-nexus-terminal = clients.overrideAttrs (old: {
            meta = old.meta // {
              mainProgram = "serial-nexus-terminal";
            };
          });
          serial-nexus-mcp = clients.overrideAttrs (old: {
            meta = old.meta // {
              mainProgram = "serial-nexus-mcp";
            };
          });
          default = pkgs.symlinkJoin {
            name = "serial-nexus-${version}";
            paths = [
              serial-nexus-core
              clients
            ];
            meta = {
              description = "SerialNexus: core, MCP server and human terminal";
              mainProgram = "serial-nexus-terminal";
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
          serial-nexus = built.default;
        }
      );

      # `nix flake check` builds these on the current system; the other
      # architecture is only evaluated (`--all-systems --no-build`) unless a
      # native builder or emulator is configured.
      checks = eachSystem (pkgs: {
        default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
        serial-nexus-mcp = self.packages.${pkgs.stdenv.hostPlatform.system}.serial-nexus-mcp;
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
