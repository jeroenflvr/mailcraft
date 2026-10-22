# Building and installing on NixOS

MailCraft provides a [flake](flake.nix), a package definition in
[mailcraft.nix](mailcraft.nix), a development shell, an overlay, and a NixOS module.
The commands below require Nix with `nix-command` and `flakes` enabled.

## Build or install from a checkout

From the repository root:

```sh
nix build .#mailcraft
./result/bin/mailcraft --sample
```

To install into your user profile instead:

```sh
nix profile install .#mailcraft
```

Neither command requires a Microsoft client ID to build. Microsoft sign-in needs
an ID, supplied at runtime or built into the package as described below.

## Microsoft sign-in without publishing your client ID

`MAILCRAFT_MICROSOFT_CLIENT_ID` is the **Application (client) ID** of your Microsoft
Entra app registration, not a password or client secret. See
[Browser sign-in (OAuth)](docs/oauth.md#registering-mailcraft-with-microsoft) for
the required registration settings, including account types and redirect URIs.

You do not need to edit MailCraft's public repository to supply your ID. Choose
one of the following approaches.

### Set it at runtime (no rebuild)

For a single launch:

```sh
MAILCRAFT_MICROSOFT_CLIENT_ID="your-application-client-id" mailcraft
```

To make it available to desktop launches, add this to your local NixOS
configuration:

```nix
environment.sessionVariables.MAILCRAFT_MICROSOFT_CLIENT_ID =
  "your-application-client-id";
```

Apply your configuration:

```sh
sudo nixos-rebuild switch
```

For a flake-based system, use your usual `--flake` argument. Log out and back in
so your desktop session and newly launched applications inherit the variable.
Restart MailCraft if it was already running.

### Bake it into the package locally

Nix builds do not inherit arbitrary variables from your shell. Simply exporting
`MAILCRAFT_MICROSOFT_CLIENT_ID` before `nix build` does not embed it.
Instead, override the package's build attributes in your own NixOS configuration.

For a flake-based NixOS configuration, add MailCraft to your existing inputs:

```nix
inputs.mailcraft.url = "github:jeroenflvr/mailcraft";
```

Pass that input to your NixOS modules through your existing
`nixpkgs.lib.nixosSystem` call:

```nix
specialArgs = { inherit inputs; };
```

Then use a NixOS module like this:

```nix
{ inputs, pkgs, ... }:

{
  nixpkgs.overlays = [ inputs.mailcraft.overlays.default ];

  environment.systemPackages = [
    (pkgs.mailcraft.overrideAttrs (_: {
      MAILCRAFT_MICROSOFT_CLIENT_ID = "your-application-client-id";
    }))
  ];
}
```

Use this instead of importing `inputs.mailcraft.nixosModules.default`, which
already installs the unmodified package. If you already enable the overlay,
keep that declaration and replace your existing `pkgs.mailcraft` package entry
with the override above.

Rebuild your NixOS configuration as usual. The resulting binary contains the ID,
so desktop launches need no extra environment setting. A runtime value takes
precedence over a built-in value.

### Keep local configuration out of public repositories

Both NixOS approaches keep the ID out of the MailCraft repository, but the value
will be present in the local, world-readable Nix store (and in any build output
you distribute if it is embedded). A client ID is a public identifier; these
instructions keep it out of source control, not confidential.

If your NixOS configuration is also public, keep the actual value in a separate
local module outside that repository and import it only in your local
configuration. Git-backed flakes exclude untracked files, so merely putting an
untracked module beside your tracked configuration is not enough: arrange for
the local module to be an explicit local input, or use a local path-based
configuration. Do not commit the real value to either public repository.
