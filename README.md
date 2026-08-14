# Depac
Declarative Pacman

## Usage
Depac takes a single argument, the path of the file containing the json configuration, described below. There is no other way to interact with the program.

```jsonc
{
  // Packages to install via pacman
  "packages": [
    "kitty",
    "firefox",
    "mpv",
    "signal-desktop",
    "linux",
  ]
  // PKGBUILDs to build from source, e.g. for AUR
  "pkgbuilds": [
    {
      "base": "depac-git",
      // AUR rpc can be disabled, manual git checks will be done instead
      // Especially useful for -git packages, where the reported AUR version
      // is not updated.
      "rpc": false
    },
    {
      "base": "nvidia-utils-beta",
      "artifacts": ["nvidia-utils-beta", "opencl-nvidia-beta", "nvidia-settings-beta"],
      // Although incorrect here, you can provide an additional git argument to change
      // the git repo we clone, if it is not on the AUR. rpc is also disabled by default if this is set.
      // "git": "https://github.com/username/customrepo.git"
    },
    // If the aur package's package name provides nothing extra than the package base, then you can
    // simply put a string. 
    "glsl_analyzer-bin"
  ],
  // This packages will be ignored by depac
  "ignore": [
    "rose-pine-hyprcursor"
  ],
  "settings": {
    // When superuser is needed, what command to use. Defaults to sudo.
    "elevation": "sudo"
  }
}
```
Note, only json, not jsonc is supported. The comments above are purely illustrative. 

Depac is intended to be used as part of a larger system, ideally with a config that generates the json for you.\
To see depac in action as a component in an advanced configuration, see [PubDoots](https://github.com/kingdomkind/PubDoots).

##  Licensing
The project's source code is licensed under `LGPL-3.0-or-later`.

The branding (eg. project name, logos etc.) is not covered by the aforementioned license and remains the sole property of `kingdomkind`. Reasonable descriptive use (eg. packaging, articles, etc.) is completely fine.
