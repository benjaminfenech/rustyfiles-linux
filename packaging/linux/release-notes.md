A Network section on the Overview page, Home in Quick access, and a quiet sign in the sidebar when an update is out.

## What's new since 0.11.0

- **Network on the Overview page.** Network shares no longer sit among the drives: they have their own Network section, with your saved network locations and the servers found on your network, as in the sidebar.
- **Home in Quick access.** The Overview page's Quick access starts with your home folder.
- **Update dot.** When a newer rfiles is out (or one is installed and waits for a restart), a small dot appears on the Settings icon in the sidebar. Hover it for "Update available"; Settings → About installs it.

**Updating:** from 0.8.0 or newer, Settings → About → Install (or the status bar's "rfiles 0.11.1 available"). From 0.7.0 or older, install by hand once as below.

## Install

| System | File | Command |
| --- | --- | --- |
| Debian 12+, Ubuntu 22.04+ | `rfiles_…_amd64.deb` / `_arm64.deb` | `sudo apt install ./rfiles_*.deb` |
| Fedora | `rfiles-….x86_64.rpm` / `.aarch64.rpm` | `sudo dnf install ./rfiles-*.rpm` |
| openSUSE | the same `.rpm` | `sudo zypper install ./rfiles-*.rpm` |
| Arch, CachyOS, Manjaro | `rfiles-git-…-x86_64.pkg.tar.zst` | `sudo pacman -U ./rfiles-git-*.pkg.tar.zst` |
| Any distribution | `rfiles-….x86_64.flatpak` / `.aarch64.flatpak` | `flatpak install --user ./rfiles-*.flatpak` |

Optional extras for previews and drives: poppler (PDFs), libavif / libheif tools (AVIF, HEIC), LibRaw (camera RAW), FFmpeg (older video formats), udisks2 (mounting drives), kio-fuse or gvfs (network locations), git (git status). The [README](https://github.com/benjaminfenech/rfiles#optional-extras) lists the package names for each distribution.

There is no Windows build. For Windows, use [upstream rustyfiles](https://github.com/gelenmedya/rustyfiles/releases).
