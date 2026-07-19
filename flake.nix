{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/release-26.05";
  };
  outputs =
    { self, nixpkgs }:
    let
      shell =
        system:
        let
          pkgs = import nixpkgs { inherit system; };
        in
        pkgs.mkShell {
          buildInputs = [
            pkgs.clippy
            pkgs.rustup
            pkgs.rustfmt
            # cargo build requires these
            pkgs.libiconv
            pkgs.curl
            # to release npm packages and whatnot
            pkgs.nodejs
          ];
        };
    in
    {
      devShell."aarch64-darwin" = shell "aarch64-darwin";
      devShell."x86_64-linux" = shell "x86_64-linux";
    };
}
