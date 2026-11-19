<p align="center"><img src="docs/logo.png" width="128" alt="Kraken Mount"></p>

A PS5 game mounter. It finds games on the internal SSD, the M.2 drive and USB drives.

**Supported formats**: debug FPKG (`.pkg`), game folders (plain, or packed with zstd or LZ4) and
exFAT images (`.exfat`, or `.zstd` holding an exFAT image), mounted read-only as ShadowMount+ does.
For large, demanding games, use the zstd pack format: it performs better than FPKG.

## How to

1. Download `kraken-mount.elf` from [Releases](https://github.com/quer3q/kraken-mount/releases)
   and send it to your jailbroken PS5 with the ELF loader (elfldr, port 9021) or payload-manager:
   `nc -w 3 <ps5-ip> 9021 < kraken-mount.elf`. kstuff must be loaded, and ShadowMount+ must not be
   running: Kraken Mount waits until both are true.
2. Put each game in a scan root:
   - internal SSD: `/data/homebrew`, `/data/etaHEN/games`
   - M.2 or extended drive (`/mnt/ext0`, `/mnt/ext1`): `homebrew`, `etaHEN/games` or the drive root
   - USB drive (`/mnt/usb0` to `/mnt/usb7`): `homebrew`, `etaHEN/games` or the drive root
3. Each game gets a "<name> registered" notification and its own home-screen tile.

[ampr-emu-ng](https://github.com/quer3q/ampr-emu-ng) comes bundled and is installed into
`/data/krakenmount/ampr/` at start. To run another build, put its `libSceAmpr.sprx` there; it is
kept until you delete it.

A game that needs the DLC or PlayGo emulator and ships none gets the bundled one in its fakelib
(setting `add_emulators`, on by default); a game's own copy is kept, unless it is a known older
release of the bundled one, which is upgraded. Details:
[DLC emulator](docs/dlcemu.md), [PlayGo stub](docs/playgo.md).

To use another build for one game only (a debug build, say), put it in
`/data/krakenmount/ampr-titles/<TITLE_ID>/`; an empty folder there runs that game without
ampr-emu-ng. After each launch Kraken Mount reads `/data/ampr_emu_status.txt` and warns when
ampr-emu-ng did not load or reported a problem (details in the log).

Debugging one game: add its title id to "Debug ampr-emu-ng for these games" in the settings
(`ampr_debug_titles`). That game then runs the bundled debug build, and while it runs Kraken Mount
saves its metrics to `/data/krakenmount/debug/<TITLE_ID>/` (also at
`http://<ps5-ip>:8097/api/debug/<TITLE_ID>/metrics.txt`).

Building both projects (Linux or WSL, Docker running):
`./ps5/build-all.sh ../ampr-emu-ng-main` builds ampr-emu-ng, bundles it (`ps5/vendor-ampr.sh`) and
builds kraken-mount; the results go to `salida/`.

Caches live in `/data/krakenmount/caches/<TITLE_ID>`, settings in
`/data/krakenmount/config.toml` and the log in `/data/krakenmount/kraken.log`.

To build it yourself: `ps5/build.sh` (needs Docker), output `target/ps5/kraken-mount.elf`.

## Thanks

- [drakmor/ShadowMountPlus](https://github.com/drakmor/ShadowMountPlus): home-screen
  registration, the fakelib sandbox overlay and backports are ported from it.
- [drakmor/dlc_emu](https://github.com/drakmor/dlc_emu): the bundled DLC emulator
  (`libSceAppContent.sprx`, `libSceNpEntitlementAccess.sprx`, `libSceGameUpdate.sprx`), added to a
  game's fakelib when the game has a `dlc_emu.ini` at its root; a known older release the game ships
  is upgraded, any other copy is kept.
- [drakmor/pgo_stub](https://github.com/drakmor/pgo_stub): the bundled PlayGo stub
  (`libScePlayGo.sprx`), added to a game's fakelib when its `eboot.bin` imports `libScePlayGo` and the
  game ships none.
- [devchmod/TITAN-CORE](https://github.com/devchmod/TITAN-CORE) (GPL-3.0-or-later): the live
  telemetry sampler (`telemetry.rs`, `telemetry_math.rs`), the metadata snapshot before
  registration (`backup.rs`) and the "a failed process scan never means absent" rule in
  `launch::proc_scan` are ported or adapted from it.
- [ps5-payload-sdk](https://github.com/ps5-payload-dev/sdk): the PS5 toolchain.

## License

GPL-3.0-or-later (`LICENSE`). Third-party notices are in `THIRD_PARTY_NOTICES`.
