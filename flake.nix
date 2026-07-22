{
  description = "dialogos - a passive AI chat bot on the Logos chat network";

  # libchat is the single source of the pinned toolchain, the nixpkgs revision,
  # and the native logos-delivery library. Following its nixpkgs (and thus its
  # glibc) for our build and runtime is what avoids the GLIBC_ABI_DT_X86_64_PLT
  # mismatch that a stock nixpkgs would cause against the native library.
  inputs = {
    libchat.url = "github:logos-messaging/libchat?rev=6ab0d8a79f59cc3b5ed6626b072e7189fa677231";
    nixpkgs.follows = "libchat/nixpkgs";
    rust-overlay.follows = "libchat/rust-overlay";
  };

  outputs = { self, nixpkgs, rust-overlay, libchat }:
    let
      systems = [ "aarch64-darwin" "x86_64-darwin" "aarch64-linux" "x86_64-linux" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f {
        inherit system;
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };
      });
    in
    {
      packages = forAllSystems ({ system, pkgs }:
        let
          # Reuse libchat's exact toolchain file (channel = stable + clippy,
          # rustfmt) so our crate compiles under the same rustc as the library.
          rustToolchain = pkgs.rust-bin.fromRustupToolchainFile "${libchat}/rust_toolchain.toml";
          rustPlatform = pkgs.makeRustPlatform {
            cargo = rustToolchain;
            rustc = rustToolchain;
          };
          logosDeliveryLib = libchat.packages.${system}.logos-delivery;

          dialogos = rustPlatform.buildRustPackage {
            pname = "dialogos";
            version = "0.1.0";
            src = pkgs.lib.cleanSource ./.;

            # allowBuiltinFetchGit avoids per-dependency outputHashes for the
            # git deps (logos-chat, chat_proto, de-mls). They are still pinned by
            # rev via Cargo.lock; the tradeoff is builtins.fetchGit runs at eval
            # instead of content-hash vendoring. Hardening to explicit
            # outputHashes is a follow-up once a nix host can compute them.
            cargoLock = {
              lockFile = ./Cargo.lock;
              allowBuiltinFetchGit = true;
            };

            nativeBuildInputs = [ pkgs.pkg-config pkgs.cmake pkgs.perl pkgs.protobuf ]
              ++ pkgs.lib.optionals pkgs.stdenv.isLinux [ pkgs.patchelf ];
            buildInputs = [ logosDeliveryLib ];

            # logos-delivery-rust's build.rs finds the native library here;
            # protoc is needed by a transitive build script on a cold build.
            LOGOS_DELIVERY_LIB_DIR = "${logosDeliveryLib}/lib";
            PROTOC = "${pkgs.protobuf}/bin/protoc";

            # Tests need the native node at runtime and run as their own CI job
            # (nix develop -c cargo test); keep the package build decoupled from
            # them so the image artifact does not depend on test runtime setup.
            doCheck = false;
          };
        in
        {
          inherit dialogos;
          default = dialogos;
        }
        # The image is Linux-only: buildLayeredImage assembles an OCI/Linux
        # container, so exposing it on darwin would just produce an unrunnable one.
        // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
          # Release artifact: the binary plus its full runtime closure (which
          # includes the native library) and CA roots for the outbound HTTPS to
          # the LLM provider and the registry.
          image = pkgs.dockerTools.buildLayeredImage {
            name = "dialogos";
            tag = "latest";
            contents = [ dialogos pkgs.cacert ];
            config = {
              Entrypoint = [ "/bin/dialogos" ];
              Cmd = [ "--config" "/etc/dialogos/config.toml" ];
              Env = [ "SSL_CERT_FILE=/etc/ssl/certs/ca-bundle.crt" ];
            };
          };
        }
      );

      devShells = forAllSystems ({ system, pkgs }:
        let
          rustToolchain = pkgs.rust-bin.fromRustupToolchainFile "${libchat}/rust_toolchain.toml";
          logosDeliveryLib = libchat.packages.${system}.logos-delivery;
        in
        {
          default = pkgs.mkShell {
            nativeBuildInputs = [ rustToolchain pkgs.pkg-config pkgs.cmake pkgs.perl pkgs.protobuf ]
              ++ pkgs.lib.optionals pkgs.stdenv.isLinux [ pkgs.patchelf ];
            buildInputs = [ logosDeliveryLib ];
            PROTOC = "${pkgs.protobuf}/bin/protoc";
            # LOGOS_DELIVERY_LIB_DIR is consumed at build time; adding it to the
            # loader path too lets `cargo test` load the native node at runtime.
            shellHook = ''
              export LOGOS_DELIVERY_LIB_DIR="${logosDeliveryLib}/lib"
              export LD_LIBRARY_PATH="${logosDeliveryLib}/lib''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
            '';
          };
        }
      );
    };
}
