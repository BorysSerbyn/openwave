{
  lib,
  stdenv,
  stdenvNoCC,
  fetchurl,
  dpkg,
  autoPatchelfHook,
  makeWrapper,
  wrapGAppsHook4,
  libraries,
  wrapLaunchers,
}:

stdenvNoCC.mkDerivation {
  pname = "openwave-bin";
  # Advance only after this release asset is published, independently of VERSION.
  version = "1.2.0";
  src = fetchurl {
    url = "https://github.com/rikkichy/openwave/releases/download/v1.2.0/openwave_1.2.0-1ubuntu24.04_amd64.deb";
    hash = "sha256-f+rKxN6fsohDp5ZMHl72239bRpthuN4544L+Sfq5TaM=";
  };

  nativeBuildInputs = [ dpkg autoPatchelfHook makeWrapper wrapGAppsHook4 ];
  buildInputs = libraries ++ [ stdenv.cc.cc.lib ];
  dontConfigure = true;
  dontBuild = true;
  dontWrapGApps = true;
  # Relocate before executing the helper, then record ELF payloads before wrapping.
  dontAutoPatchelf = true;

  unpackPhase = ''
    runHook preUnpack
    dpkg-deb --extract "$src" .
    runHook postUnpack
  '';
  installPhase = ''
    runHook preInstall
    mkdir -p "$out"
    cp -r usr/. "$out/"
    rm "$out/share/openwave/install-manifest.json"
    runHook postInstall
  '';
  postFixup = ''
    autoPatchelf "$out"
    "$out/libexec/openwave-maintenance" record-install --prefix "$out" --method nix
    mkdir -p "$out/lib/udev/rules.d"
    "$out/libexec/openwave-maintenance" udev-rules > "$out/lib/udev/rules.d/99-openwave.rules"
  '' + wrapLaunchers;

  meta = {
    description = "Prebuilt Linux control application for Elgato Wave devices";
    homepage = "https://github.com/rikkichy/openwave";
    license = lib.licenses.mit;
    sourceProvenance = [ lib.sourceTypes.binaryNativeCode ];
    mainProgram = "openwave";
    platforms = [ "x86_64-linux" ];
  };
}
