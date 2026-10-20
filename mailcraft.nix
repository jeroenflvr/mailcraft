{ lib
, rustPlatform
, pkg-config
, openssl
, perl
, cmake
, clang
, libxkbcommon
, wayland
, libGL
, vulkan-loader
, xorg
}:

rustPlatform.buildRustPackage rec {
  pname = "mailcraft";
  version = "0.18.11";
  src = ./.;

  cargoLock.lockFile = ./Cargo.lock;

  nativeBuildInputs = [ pkg-config perl cmake clang ];

  buildInputs = [
    openssl
    libxkbcommon
    wayland
    libGL
    vulkan-loader
    xorg.libX11
    xorg.libXcursor
    xorg.libXi
    xorg.libXrandr
  ];

  LIBCLANG_PATH = "${clang.lib}/lib";
  OPENSSL_NO_VENDOR = 1;

  cargoBuildFlags = [ "-p" "mailcraft" "-p" "mailcraft-cli" ];

  meta = with lib; {
    description = "Local-first email client in Rust";
    homepage = "https://github.com/jeroenflvr/mailcraft";
    license = licenses.mit;
    platforms = platforms.linux;
    mainProgram = "mailcraft";
  };
}
