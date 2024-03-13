{
  nixConfig.extra-substituters = [
    "https://wrpc.cachix.org"
    "https://wasmcloud.cachix.org"
    "https://nixify.cachix.org"
    "https://crane.cachix.org"
    "https://bytecodealliance.cachix.org"
    "https://nix-community.cachix.org"
    "https://cache.garnix.io"
  ];
  nixConfig.extra-trusted-public-keys = [
    "wrpc.cachix.org-1:J1xnzWo1nnhlzOmZCA10/5wz87LwCFwQtnqCibCy67w="
    "wasmcloud.cachix.org-1:9gRBzsKh+x2HbVVspreFg/6iFRiD4aOcUQfXVDl3hiM="
    "nixify.cachix.org-1:95SiUQuf8Ij0hwDweALJsLtnMyv/otZamWNRp1Q1pXw="
    "crane.cachix.org-1:8Scfpmn9w+hGdXH/Q9tTLiYAE/2dnJYRJP7kl80GuRk="
    "bytecodealliance.cachix.org-1:0SBgh//n2n0heh0sDFhTm+ZKBRy2sInakzFGfzN531Y="
    "nix-community.cachix.org-1:mB9FSh9qf2dCimDSUo8Zy7bkq5CX+/rkCWyvRCYg3Fs="
    "cache.garnix.io:CTFPyKSLcx5RMJKfLo5EEPUObbA78b0YQ2DTCJXqr9g="
  ];

  inputs.nixify.inputs.nixlib.follows = "nixlib";
  inputs.nixify.url = "github:rvolosatovs/nixify";
  inputs.nixlib.url = "github:nix-community/nixpkgs.lib";
  inputs.wasmcloud-component-adapters.inputs.nixify.follows = "nixify";
  inputs.wasmcloud-component-adapters.url = "github:wasmCloud/wasmcloud-component-adapters/v0.8.0";
  inputs.wit-deps.inputs.nixify.follows = "nixify";
  inputs.wit-deps.inputs.nixlib.follows = "nixlib";
  inputs.wit-deps.url = "github:bytecodealliance/wit-deps/v0.3.5";

  outputs = {
    nixify,
    nixlib,
    wasmcloud-component-adapters,
    wit-deps,
    ...
  }:
    with builtins;
    with nixlib.lib;
    with nixify.lib;
      rust.mkFlake {
        src = ./.;

        overlays = [
          wit-deps.overlays.default
        ];

        excludePaths = [
          ".envrc"
          ".github"
          ".gitignore"
          "flake.nix"
          "LICENSE"
          "README.md"
        ];

        doCheck = false; # testing is performed in checks via `nextest`

        targets.wasm32-unknown-unknown = false;
        targets.wasm32-wasi = false;

        clippy.allTargets = true;
        clippy.deny = ["warnings"];
        clippy.workspace = true;

        test.allTargets = true;
        test.workspace = true;

        buildOverrides = {
          pkgs,
          pkgsCross ? pkgs,
          ...
        }: {
          buildInputs ? [],
          depsBuildBuild ? [],
          nativeBuildInputs ? [],
          nativeCheckInputs ? [],
          ...
        } @ args:
          with pkgs.lib; let
            darwin2darwin = pkgs.stdenv.hostPlatform.isDarwin && pkgsCross.stdenv.hostPlatform.isDarwin;

            depsBuildBuild' =
              depsBuildBuild
              ++ optional pkgs.stdenv.hostPlatform.isDarwin pkgs.darwin.apple_sdk.frameworks.SystemConfiguration
              ++ optional darwin2darwin pkgs.xcbuild.xcrun;
          in
            {
              env.WASI_PREVIEW1_COMMAND_COMPONENT_ADAPTER = wasmcloud-component-adapters.packages.${pkgs.stdenv.system}.wasi-preview1-command-component-adapter;
              env.WASI_PREVIEW1_REACTOR_COMPONENT_ADAPTER = wasmcloud-component-adapters.packages.${pkgs.stdenv.system}.wasi-preview1-reactor-component-adapter;

              buildInputs =
                buildInputs
                ++ optional pkgs.stdenv.hostPlatform.isDarwin pkgs.libiconv;

              depsBuildBuild = depsBuildBuild';
            }
            // optionalAttrs (args ? cargoArtifacts) {
              depsBuildBuild =
                depsBuildBuild'
                ++ optionals darwin2darwin [
                  pkgs.darwin.apple_sdk.frameworks.CoreFoundation
                  pkgs.darwin.apple_sdk.frameworks.CoreServices
                ];

              nativeCheckInputs =
                nativeCheckInputs
                ++ [
                  pkgs.nats-server
                ];
            };

        withPackages = {
          hostRustToolchain,
          packages,
          pkgs,
          ...
        }: let
          interpreters.aarch64-unknown-linux-gnu = "/lib/ld-linux-aarch64.so.1";
          interpreters.riscv64gc-unknown-linux-gnu = "/lib/ld-linux-riscv64-lp64d.so.1";
          interpreters.x86_64-unknown-linux-gnu = "/lib64/ld-linux-x86-64.so.2";

          mkFHS = {
            name,
            src,
            interpreter,
          }:
            pkgs.stdenv.mkDerivation {
              inherit
                name
                src
                ;

              buildInputs = [
                pkgs.patchelf
              ];

              dontBuild = true;
              dontFixup = true;

              installPhase = ''
                runHook preInstall

                for p in $(find . -type f); do
                  # https://en.wikipedia.org/wiki/Executable_and_Linkable_Format#File_header
                  if head -c 4 $p | grep $'\x7FELF' > /dev/null; then
                    patchelf --set-rpath /lib $p || true
                    patchelf --set-interpreter ${interpreter} $p || true
                  fi
                done

                mkdir -p $out
                cp -R * $out

                runHook postInstall
              '';
            };

          wrpc-aarch64-unknown-linux-gnu-fhs = mkFHS {
            name = "wrpc-aarch64-unknown-linux-gnu-fhs";
            src = packages.wrpc-aarch64-unknown-linux-gnu;
            interpreter = interpreters.aarch64-unknown-linux-gnu;
          };

          wrpc-riscv64gc-unknown-linux-gnu-fhs = mkFHS {
            name = "wrpc-riscv64gc-unknown-linux-gnu-fhs";
            src = packages.wrpc-riscv64gc-unknown-linux-gnu;
            interpreter = interpreters.riscv64gc-unknown-linux-gnu;
          };

          wrpc-x86_64-unknown-linux-gnu-fhs = mkFHS {
            name = "wrpc-x86_64-unknown-linux-gnu-fhs";
            src = packages.wrpc-x86_64-unknown-linux-gnu;
            interpreter = interpreters.x86_64-unknown-linux-gnu;
          };
        in
          packages
          // {
            inherit
              wrpc-aarch64-unknown-linux-gnu-fhs
              wrpc-riscv64gc-unknown-linux-gnu-fhs
              wrpc-x86_64-unknown-linux-gnu-fhs
              ;

            rust = hostRustToolchain;
          };

        withDevShells = {
          devShells,
          pkgs,
          ...
        }:
          extendDerivations {
            buildInputs = [
              pkgs.cargo-audit
              pkgs.nats-server
              pkgs.natscli
              pkgs.wit-deps
            ];
          }
          devShells;
      };
}
