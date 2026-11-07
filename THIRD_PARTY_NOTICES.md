# Third-party notices

Noir Player itself is released under the [MIT licence](LICENSE).

Discover uses a few separate programs. They are not part of Noir Player's
source or installer. Noir Player downloads each one from its official release
page the first time it is needed, checks it, and keeps it in its own folder
(`noir-player/tools` in your local app data). Noir Player starts them as
separate processes and only passes them command-line arguments and files, so
their licences apply to those programs and not to Noir Player.

| Program | Used for | Licence | Source and releases |
| --- | --- | --- | --- |
| yt-dlp | Finding and downloading the audio | Unlicense (source); standalone executables see below | [github.com/yt-dlp/yt-dlp](https://github.com/yt-dlp/yt-dlp), [releases](https://github.com/yt-dlp/yt-dlp/releases) |
| FFmpeg (minimal build) | Embedding covers and converting to MP3 | LGPL-2.1-or-later | [github.com/Abdullah-Masood-05/ffmpeg-ytdl-minimal-build](https://github.com/Abdullah-Masood-05/ffmpeg-ytdl-minimal-build) (v1.1.0) |
| QuickJS-ng | Running YouTube's scripts when yt-dlp needs it | MIT | [github.com/quickjs-ng/quickjs](https://github.com/quickjs-ng/quickjs) (v0.18.0) |

## yt-dlp

yt-dlp's source code is released under the
[Unlicense](https://github.com/yt-dlp/yt-dlp/blob/master/LICENSE). Noir Player
downloads yt-dlp's standalone executables, and the yt-dlp README says of its
release files:

> While yt-dlp is licensed under the Unlicense, many of the release files
> contain code from other projects with different licenses.
>
> Most notably, the PyInstaller-bundled executables include GPLv3+ licensed
> code, and as such the combined work is licensed under GPLv3+.

(See [License](https://github.com/yt-dlp/yt-dlp#license) in the yt-dlp README
and its
[THIRD_PARTY_LICENSES.txt](https://github.com/yt-dlp/yt-dlp/blob/master/THIRD_PARTY_LICENSES.txt).)
So the executables Noir Player downloads are GPLv3+ as a combined work. They
are downloaded unmodified from yt-dlp's own releases, and their source is
available from yt-dlp's repository at the matching release tag.

## FFmpeg (minimal build)

A small, statically linked FFmpeg 7.1.1 built without any GPL components, so
it is LGPL-2.1-or-later. It includes:

- FFmpeg libraries and programs: [LGPL-2.1-or-later](https://www.ffmpeg.org/legal.html)
- [LAME](https://lame.sourceforge.io/) (MP3 encoder): LGPL
- [libopus](https://opus-codec.org/): BSD-3-Clause
- [zlib](https://zlib.net/): zlib licence

Corresponding source: FFmpeg
[`n7.1.1`](https://git.ffmpeg.org/gitweb/ffmpeg.git/tree/refs/tags/n7.1.1)
plus the
[`build.sh`](https://github.com/Abdullah-Masood-05/ffmpeg-ytdl-minimal-build/blob/main/build.sh)
in the build repository, which lists every configure option and the versions of
the libraries it links.

## QuickJS-ng

[QuickJS-ng](https://github.com/quickjs-ng/quickjs/blob/master/LICENSE) is
released under the MIT licence.
