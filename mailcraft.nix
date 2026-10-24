{ lib
, rustPlatform
, pkg-config
, openssl
, perl
, cmake
, clang
, llvmPackages
, libxkbcommon
, wayland
, libGL
, vulkan-loader
, libx11
, libxcursor
, libxi
, libxrandr
}:

rustPlatform.buildRustPackage rec {
  pname = "mailcraft";
  version = "0.19.0";
  src = ./.;

  cargoLock.lockFile = ./Cargo.lock;

  nativeBuildInputs = [ pkg-config perl cmake clang ];

  buildInputs = [
    openssl
    libxkbcommon
    wayland
    libGL
    vulkan-loader
    libx11
    libxcursor
    libxi
    libxrandr
  ];

  LIBCLANG_PATH = "${llvmPackages.libclang.lib}/lib";
  OPENSSL_NO_VENDOR = 1;

  cargoBuildFlags = [ "-p" "mailcraft" "-p" "mailcraft-cli" ];

  # winit/glutin/wgpu dlopen these at runtime; put them on the RPATH.
  postFixup = ''
    patchelf --add-rpath "${lib.makeLibraryPath buildInputs}" $out/bin/mailcraft
  '';

  meta = with lib; {
    description = "Local-first email client in Rust";
    homepage = "https://github.com/jeroenflvr/mailcraft";
    license = licenses.mit;
    platforms = platforms.linux;
    mainProgram = "mailcraft";
  };
}
