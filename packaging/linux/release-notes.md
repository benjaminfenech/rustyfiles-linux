A terminal inside rfiles: press F4 for your shell under the file list, in the folder you're in.

## What's new since 0.11.1

- **Terminal pane.** F4 opens your shell in a pane under the file list, in the current folder; F4 again hides it and keeps the shell running. "Run command here…" opens it too, and `>` and a command in the address bar (`> make`) runs the command in it.
- **It follows you.** Browsing to a folder moves the shell there while nothing is running in it, and a `cd` typed in the pane takes the file list along.
- **A real terminal.** Colours, scrollback, full-screen programs (vim, less, htop), mouse selection, Ctrl+Shift+C / Ctrl+Shift+V to copy and paste, middle-click paste, Shift+PageUp/PageDown to scroll back. Keys go to the shell while it has focus, so Ctrl+W deletes a word rather than closing the tab. Each tab has its own pane; drag its top edge to resize it.
- **Skip things in it:** the shell gets `TERM_PROGRAM=rfiles`, so your shell's startup file can leave things out there, for example `[[ $TERM_PROGRAM != rfiles ]] && fastfetch`.
- "Open in terminal" still opens your own terminal app.

**Updating:** from 0.8.0 or newer, Settings → About → Install (or the status bar's "rfiles 0.12.0 available", or the dot on Settings). From 0.7.0 or older, install by hand once as below.

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
